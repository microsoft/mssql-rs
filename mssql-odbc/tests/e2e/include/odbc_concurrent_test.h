// Copyright (c) Microsoft Corporation. All rights reserved.
// odbc_concurrent_test.h  –  One test body, run single-threaded and under
// concurrent handle-sharing modes.
//
//   ODBC_TEST_F(Fixture, Name) { ... }
//       Registers Fixture.Name, which runs exactly like TEST_F, plus
//       Fixture.Name/CrossThread, /SharedEnv, and /SharedStmt.
//   ODBC_TEST_F_MODES(Fixture, Name, odbc_concurrency::kAllModes) { ... }
//       Chooses the modes explicitly; kAllModes adds /SharedDbc.
//
// The body is written as for TEST_F: it uses stmt_, dbc_, and the fixture's
// helpers. Every non-Single run uses fresh fixture instances (SetUp, body,
// TearDown) whose handles depend on the mode:
//
//   CrossThread  One fixture; SetUp, body, and TearDown each run on a different
//                thread, so handles are allocated, used, and freed on three
//                threads. Deterministic.
//   SharedEnv    Workers share the test's ENV; each SetUp connects its own DBC.
//                Every failure fails the test.
//   SharedDbc    Workers share the test's DBC; each worker gets its own STMT.
//                Without MARS a statement can find the connection busy (HY000);
//                an iteration whose only failures are busy errors is retried.
//                Opt in only for bodies that create no fixed-name objects.
//   (SharedEnv and SharedDbc capture each worker's failures on its own thread
//   and re-report them with their original file and line, so HasFatalFailure()
//   inside the body does not see them. Do not use these modes for bodies that
//   rely on it.)
//   SharedStmt   An owner thread runs the body on its own connection while
//                observer threads read that STMT's and DBC's diagnostics with
//                SQLGetDiagField and SQLGetDiagRec, the only calls that change
//                no state and clear no records. Observers check that every
//                record is well formed. Owner failures are reported directly.
//                The body must not free or replace stmt_ or dbc_, and the
//                fixture must Connect() in SetUp(): there is nothing to observe
//                without a statement, so an ENV-only fixture must leave this
//                mode out with ODBC_TEST_F_MODES rather than register it.
//                The Driver Manager serializes calls per handle (Windows, and
//                unixODBC at the Threading=1 run_e2e.sh sets), so only the DBC
//                reads overlap the owner's calls in the driver; STMT reads run
//                between them.
//
// Worker() and Iteration() identify the run (0, 0 when single-threaded).
// WorkerToken() derives a value unique to the run, so a result delivered to
// the wrong connection or statement cannot match.
//
// Failures in a non-Single mode carry a trace naming the mode and the worker,
// iteration, or thread role. The first failure stops further iterations. An
// exception from a fixture phase on any thread fails the test as gtest would
// on its own thread (see RunPhase).
//
// Env (defaults are the PR smoke size; nightly sets the stress size):
//   MSSQL_ODBC_MT_THREADS          Threads running at once: SharedEnv and
//                                  SharedDbc workers, or the SharedStmt owner
//                                  plus observers, so 1 runs the owner alone
//                                  (default 2)
//   MSSQL_ODBC_MT_ITERS            Times each worker, or the SharedStmt owner,
//                                  runs the body (default 1)
//   MSSQL_ODBC_MT_TIMEOUT_SECONDS  Hang watchdog (default 300) for each phase
//                                  of a non-Single test: the fixture's SetUp,
//                                  the run, and its TearDown; see Watchdog for
//                                  what happens on expiry
//   MSSQL_ODBC_MT_DUMP_DIR         Where the watchdog writes a Windows
//                                  minidump (default: working directory)
//   CrossThread ignores the first two: it always runs the body once.

#pragma once

#include "odbc_test_fixture.h"

#include <gtest/gtest-spi.h>

#if defined(_WIN32)
#include <dbghelp.h>
#elif defined(__APPLE__)
#include <sys/sysctl.h>
#include <sys/types.h>
#include <unistd.h>
#endif

#include <algorithm>
#include <atomic>
#include <cctype>
#include <cerrno>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <exception>
#include <fstream>
#include <functional>
#include <iostream>
#include <memory>
#include <mutex>
#include <shared_mutex>
#include <string>
#include <thread>
#include <vector>

namespace odbc_concurrency {

enum Mode : unsigned {
    kSingle = 1u << 0,
    kCrossThread = 1u << 1,
    kSharedEnv = 1u << 2,
    kSharedDbc = 1u << 3,
    kSharedStmt = 1u << 4,
};

constexpr Mode kModes[] = {kSingle, kCrossThread, kSharedEnv, kSharedDbc, kSharedStmt};
constexpr unsigned kDefaultModes = kSingle | kCrossThread | kSharedEnv | kSharedStmt;
constexpr unsigned kAllModes = kDefaultModes | kSharedDbc;

inline const char* ModeName(Mode mode) {
    switch (mode) {
        case kSingle:
            return "Single";
        case kCrossThread:
            return "CrossThread";
        case kSharedEnv:
            return "SharedEnv";
        case kSharedDbc:
            return "SharedDbc";
        case kSharedStmt:
            return "SharedStmt";
    }
    return "UnknownMode";
}

class WorkerScope {
   public:
    int Worker() const { return worker_; }
    int Iteration() const { return iteration_; }
    std::string WorkerToken(const std::string& base) const {
        return base + "-w" + std::to_string(worker_) + "-i" + std::to_string(iteration_);
    }

   protected:
    void SetWorker(int worker, int iteration) {
        worker_ = worker;
        iteration_ = iteration;
    }

   private:
    int worker_ = 0;
    int iteration_ = 0;
};

namespace detail {

constexpr int kMaxConflictRetries = 10000;

// Fails the test and returns |fallback| when the setting is present but invalid.
inline int ReadSetting(const char* name, int fallback, int max_value) {
#ifdef _WIN32
    char* buffer = nullptr;
    size_t length = 0;
    if (_dupenv_s(&buffer, &length, name) != 0 || buffer == nullptr) {
        return fallback;
    }
    const std::string text(buffer);
    free(buffer);
#else
    const char* env = std::getenv(name);
    if (env == nullptr) {
        return fallback;
    }
    const std::string text(env);
#endif
    if (text.empty()) {
        return fallback;
    }
    errno = 0;
    char* end = nullptr;
    const long parsed = std::strtol(text.c_str(), &end, 10);
    if (errno != 0 || end == text.c_str() || *end != '\0' || parsed <= 0 || parsed > max_value) {
        ADD_FAILURE() << name << " must be an integer in [1, " << max_value << "], got '" << text
                      << "'";
        return fallback;
    }
    return static_cast<int>(parsed);
}

struct Config {
    int threads = 0;
    int iterations = 0;
    int timeout_seconds = 0;
};

inline Config LoadConfig() {
    Config config;
    config.threads = ReadSetting("MSSQL_ODBC_MT_THREADS", 2, 256);
    config.iterations = ReadSetting("MSSQL_ODBC_MT_ITERS", 1, 1000000);
    config.timeout_seconds = ReadSetting("MSSQL_ODBC_MT_TIMEOUT_SECONDS", 300, 86400);
    return config;
}

class StartGate {
   public:
    explicit StartGate(int participants) : remaining_(participants) {}

    void ArriveAndWait() {
        std::unique_lock<std::mutex> lock(mutex_);
        if (--remaining_ == 0) {
            open_ = true;
            cv_.notify_all();
            return;
        }
        cv_.wait(lock, [this] { return open_; });
    }

   private:
    std::mutex mutex_;
    std::condition_variable cv_;
    int remaining_;
    bool open_ = false;
};

// A hung worker holds live ODBC handles, so the process cannot unwind safely.
// On expiry the watchdog reports where every worker stopped and terminates the
// process so its thread stacks can be inspected afterwards:
//   Windows  writes a minidump of every thread, then exits with 124. gtest turns
//            off Windows Error Reporting and abort()'s crash dump on Windows,
//            so an abort() there leaves nothing to inspect.
//   Others   abort(), so the OS writes a core file when core dumps are enabled.
// A process under a debugger is never terminated: sitting at a breakpoint for
// longer than the timeout is not a hang. Nor is one whose debugger status
// cannot be determined (detection exists for Windows, Linux, and macOS).
//
// Env: MSSQL_ODBC_MT_DUMP_DIR  directory for the Windows minidump (default:
//      the working directory).

enum class DebuggerState { kAbsent, kAttached, kUnknown };

inline DebuggerState QueryDebugger() {
#if defined(_WIN32)
    return IsDebuggerPresent() != FALSE ? DebuggerState::kAttached : DebuggerState::kAbsent;
#elif defined(__linux__)
    std::ifstream status("/proc/self/status");
    std::string line;
    while (std::getline(status, line)) {
        if (line.rfind("TracerPid:", 0) == 0) {
            return std::strtol(line.c_str() + 10, nullptr, 10) != 0 ? DebuggerState::kAttached
                                                                    : DebuggerState::kAbsent;
        }
    }
    return DebuggerState::kUnknown;
#elif defined(__APPLE__)
    // Apple Technical Q&A QA1361: the kernel marks a traced process P_TRACED.
    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, static_cast<int>(getpid())};
    struct kinfo_proc info = {};
    size_t size = sizeof(info);
    if (sysctl(mib, 4, &info, &size, nullptr, 0) != 0) {
        return DebuggerState::kUnknown;
    }
    return (info.kp_proc.p_flag & P_TRACED) != 0 ? DebuggerState::kAttached
                                                 : DebuggerState::kAbsent;
#else
    return DebuggerState::kUnknown;
#endif
}

[[noreturn]] inline void TerminateHungProcess() {
#if defined(_WIN32)
    std::string dir = ".";
    char* env = nullptr;
    size_t length = 0;
    if (_dupenv_s(&env, &length, "MSSQL_ODBC_MT_DUMP_DIR") == 0 && env != nullptr) {
        if (*env != '\0') {
            dir = env;
        }
        free(env);
    }
    const std::string path =
        dir + "\\odbc-mt-watchdog-" + std::to_string(GetCurrentProcessId()) + ".dmp";
    const HANDLE file = CreateFileA(path.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS,
                                    FILE_ATTRIBUTE_NORMAL, nullptr);
    const bool written =
        file != INVALID_HANDLE_VALUE &&
        MiniDumpWriteDump(GetCurrentProcess(), GetCurrentProcessId(), file,
                          static_cast<MINIDUMP_TYPE>(MiniDumpWithThreadInfo |
                                                     MiniDumpWithUnloadedModules),
                          nullptr, nullptr, nullptr) != FALSE;
    if (file != INVALID_HANDLE_VALUE) {
        CloseHandle(file);
    }
    std::cerr << (written ? "  minidump: " : "  minidump failed: ") << path << "\n";
    std::cerr.flush();
    std::_Exit(124);
#else
    std::cerr << "  aborting for a core dump\n";
    std::cerr.flush();
    std::abort();
#endif
}

class Watchdog {
   public:
    Watchdog(std::string label, std::chrono::seconds timeout,
             const std::vector<std::atomic<int>>& progress)
        : thread_([this, label, timeout, &progress] {
              std::unique_lock<std::mutex> lock(mutex_);
              if (cv_.wait_for(lock, timeout, [this] { return stopped_; })) {
                  return;
              }
              const DebuggerState debugger = QueryDebugger();
              if (debugger != DebuggerState::kAbsent) {
                  std::cerr << label << ": watchdog expired after " << timeout.count() << "s "
                            << (debugger == DebuggerState::kAttached
                                    ? "with a debugger attached"
                                    : "and cannot tell whether a debugger is attached")
                            << "; not terminating.\n";
                  cv_.wait(lock, [this] { return stopped_; });
                  return;
              }
              std::cerr << label << ": watchdog expired after " << timeout.count()
                        << "s; terminating.\n";
              for (size_t i = 0; i < progress.size(); ++i) {
                  std::cerr << "  worker " << i << ": completed_iterations=" << progress[i].load()
                            << "\n";
              }
              TerminateHungProcess();
          }) {}

    ~Watchdog() {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            stopped_ = true;
        }
        cv_.notify_one();
        thread_.join();
    }

    Watchdog(const Watchdog&) = delete;
    Watchdog& operator=(const Watchdog&) = delete;

   private:
    std::mutex mutex_;
    std::condition_variable cv_;
    bool stopped_ = false;
    std::thread thread_;
};

// gtest runs the coordinator fixture's SetUp() before TestBody() and its
// TearDown() after, outside the runners' watchdogs. In a non-Single mode those
// phases can hang on what the run left behind (a SharedDbc connection left
// locked hangs the coordinator's disconnect), so each gets a watchdog of its
// own. The run in between keeps the runner's watchdog, which reports
// per-worker progress. Only the gtest-created coordinator is watched this way;
// the runners' per-iteration fixtures are already inside a runner's watchdog.
class CoordinatorWatch {
   public:
    void WatchCoordinator(std::string test_label) {
        label_ = std::move(test_label);
        Arm("SetUp()");
    }

   protected:
    template <typename Run>
    void RunWatched(Run&& run) {
        const bool watched = watchdog_ != nullptr;
        watchdog_.reset();
        try {
            run();
        } catch (...) {
            if (watched) {
                Arm("TearDown()");
            }
            throw;
        }
        if (watched) {
            Arm("TearDown()");
        }
    }

   private:
    void Arm(const char* phase) {
        watchdog_.reset();
        watchdog_.reset(new Watchdog(label_ + " coordinator " + phase,
                                     std::chrono::seconds(LoadConfig().timeout_seconds),
                                     progress_));
    }

    std::string label_;
    // No workers outside the run, so the watchdog has no progress to report.
    std::vector<std::atomic<int>> progress_;
    std::unique_ptr<Watchdog> watchdog_;
};

inline bool MentionsConnectionBusy(const std::string& text) {
    std::string lower(text);
    std::transform(lower.begin(), lower.end(), lower.begin(),
                   [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    return lower.find("[hy000]") != std::string::npos && lower.find("busy") != std::string::npos;
}

inline bool StatementReportsBusy(SQLHSTMT stmt) {
    return stmt != SQL_NULL_HSTMT &&
           MentionsConnectionBusy(ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt));
}

enum class Outcome { kClean, kSkipped, kConflict, kFailed };

// An iteration is a conflict only when every failure is explained by a busy
// connection. Failure text carries the diagnostic when the body used
// ASSERT_SQL_OK; a bare ASSERT_EQ on the return code does not, so the last
// failure may instead be explained by the statement's own diagnostic.
inline Outcome Classify(const ::testing::TestPartResultArray& parts, bool statement_busy) {
    int last_failure = -1;
    bool skipped = false;
    for (int i = 0; i < parts.size(); ++i) {
        const ::testing::TestPartResult& part = parts.GetTestPartResult(i);
        if (part.skipped()) {
            skipped = true;
        } else if (part.failed()) {
            last_failure = i;
        }
    }
    if (last_failure < 0) {
        return skipped ? Outcome::kSkipped : Outcome::kClean;
    }
    for (int i = 0; i <= last_failure; ++i) {
        const ::testing::TestPartResult& part = parts.GetTestPartResult(i);
        if (!part.failed() || MentionsConnectionBusy(part.message())) {
            continue;
        }
        const bool explained_by_statement =
            i == last_failure && part.fatally_failed() && statement_busy;
        if (!explained_by_statement) {
            return Outcome::kFailed;
        }
    }
    return Outcome::kConflict;
}

inline void ReportFailures(const ::testing::TestPartResultArray& parts) {
    for (int i = 0; i < parts.size(); ++i) {
        const ::testing::TestPartResult& part = parts.GetTestPartResult(i);
        if (!part.failed()) {
            continue;
        }
        const char* file = part.file_name() != nullptr ? part.file_name() : "(unknown)";
        ADD_FAILURE_AT(file, part.line_number()) << part.message();
    }
}

inline std::string FirstMessage(const ::testing::TestPartResultArray& parts) {
    for (int i = 0; i < parts.size(); ++i) {
        const ::testing::TestPartResult& part = parts.GetTestPartResult(i);
        if (part.failed() || part.skipped()) {
            return part.message();
        }
    }
    return std::string();
}

// A fresh statement must run on the shared connection once every worker is
// done, proving no worker left it claimed.
inline void VerifyConnectionIdle(SQLHDBC dbc) {
    SQLHSTMT stmt = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &stmt), SQL_HANDLE_DBC, dbc);
    const SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    const SQLRETURN exec_rc = SQLExecDirect(stmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(exec_rc, SQL_HANDLE_STMT, stmt);
    if (SQL_SUCCEEDED(exec_rc)) {
        SQLINTEGER value = 0;
        EXPECT_SQL_OK(SQLFetch(stmt), SQL_HANDLE_STMT, stmt);
        EXPECT_SQL_OK(SQLGetData(stmt, 1, SQL_C_SLONG, &value, 0, nullptr), SQL_HANDLE_STMT,
                      stmt);
        EXPECT_EQ(1, value) << "connection idle check";
    }
    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
}

struct WorkerResult {
    int completed = 0;
    int conflicts = 0;
    std::string skip_message;
};

// gtest turns an exception into a test failure only on the thread running the
// test, and only around the calls it makes itself. The runners call fixture
// phases themselves, often on other threads, where an escaping exception would
// call std::terminate (and, in CrossThread, strand the next phase). RunPhase
// runs one phase the way gtest would: an exception becomes a fatal failure on
// the calling thread, worded as gtest words it, and the runner goes on to
// TearDown. With --gtest_catch_exceptions=0 it propagates instead, as gtest's
// own handling does, so the process dies where it threw and leaves a dump.
// Returns false when the phase threw.
template <typename F>
bool RunPhase(const char* location, F&& phase) {
    if (!GTEST_FLAG_GET(catch_exceptions)) {
        phase();
        return true;
    }
    std::string message;
    try {
        phase();
        return true;
    } catch (const std::exception& e) {
        message = std::string("C++ exception with description \"") + e.what() +
                  "\" thrown in " + location + ".";
    } catch (...) {
        message = std::string("Unknown C++ exception thrown in ") + location + ".";
    }
    [&] { FAIL() << message; }();
    return false;
}

// A worker's finished fixtures, destroyed after every worker has joined.
// testing::Test holds a GTestFlagSaver: its destructor writes every
// process-global gtest flag back through the non-Abseil GTEST_FLAG_SET, a bare
// global assignment. Workers read those same globals the whole time they run —
// RunPhase reads catch_exceptions, and gtest's failure reporting reads
// break_on_failure and throw_on_failure — so destroying a fixture on its worker
// races every other worker, and serializing destruction against construction
// alone does not fix it. Keeping the fixtures alive leaves only flag reads in
// the parallel window, which is safe. TearDown has already run, so no ODBC
// handle is held open by the wait. Only this runner builds fixtures off the
// gtest thread; CrossThread, SharedStmt, and the gtest factory each build and
// destroy theirs on one thread.
template <typename T>
using RetainedFixtures = std::vector<std::unique_ptr<T>>;

// Runs one iteration with its failures captured for this thread only. Workers
// must not report into, or read, the current test's shared TestResult while
// other workers write to it: gtest appends to it under a lock but reads it
// (HasFailure, IsSkipped) without one.
template <typename T>
void RunCapturedIteration(SQLHENV env, SQLHDBC dbc, Mode mode, int worker, int iteration,
                          const std::string& tag, ::testing::TestPartResultArray& parts,
                          bool& statement_busy, RetainedFixtures<T>& retained) {
    ::testing::ScopedFakeTestPartResultReporter reporter(
        ::testing::ScopedFakeTestPartResultReporter::INTERCEPT_ONLY_CURRENT_THREAD, &parts);
    SCOPED_TRACE(tag);
    std::unique_ptr<T> test;
    // Retains the fixture on every path out, including a throwing constructor.
    struct Retain {
        std::unique_ptr<T>& test;
        RetainedFixtures<T>& retained;
        ~Retain() {
            if (test != nullptr) {
                retained.push_back(std::move(test));
            }
        }
    } retain{test, retained};
    if (!RunPhase("the test fixture's constructor", [&] { test.reset(new T(mode)); })) {
        return;
    }
    RunPhase("SetUp()", [&] { test->ConcurrentSetUp(env, dbc, worker, iteration); });
    bool set_up = true;
    for (int i = 0; i < parts.size(); ++i) {
        const ::testing::TestPartResult& part = parts.GetTestPartResult(i);
        set_up = set_up && !part.fatally_failed() && !part.skipped();
    }
    if (set_up) {
        RunPhase("the test body", [&] { test->ConcurrentBody(); });
    }
    RunPhase("TearDown()", [&] {
        statement_busy = StatementReportsBusy(test->ConcurrentStmt());
        test->ConcurrentTearDown();
    });
}

// SharedEnv: every failure fails the iteration; nothing is retried.
template <typename T>
bool RunSharedEnvIteration(SQLHENV env, Mode mode, int worker, int iteration,
                           const std::string& tag, WorkerResult& result,
                           RetainedFixtures<T>& retained) {
    ::testing::TestPartResultArray parts;
    bool statement_busy = false;
    RunCapturedIteration<T>(env, SQL_NULL_HDBC, mode, worker, iteration, tag, parts,
                            statement_busy, retained);
    bool failed = false;
    bool skipped = false;
    for (int i = 0; i < parts.size(); ++i) {
        failed = failed || parts.GetTestPartResult(i).failed();
        skipped = skipped || parts.GetTestPartResult(i).skipped();
    }
    if (failed) {
        ReportFailures(parts);
        return false;
    }
    if (skipped) {
        result.skip_message = FirstMessage(parts);
        return false;
    }
    return true;
}

// SharedDbc: an iteration whose only failures are a busy connection is retried
// instead of failing the test.
template <typename T>
bool RunSharedDbcIteration(SQLHENV env, SQLHDBC dbc, Mode mode, int worker, int iteration,
                           const std::string& tag, WorkerResult& result,
                           RetainedFixtures<T>& retained) {
    for (int attempt = 0;; ++attempt) {
        ::testing::TestPartResultArray parts;
        bool statement_busy = false;
        RunCapturedIteration<T>(env, dbc, mode, worker, iteration, tag, parts, statement_busy,
                                retained);
        switch (Classify(parts, statement_busy)) {
            case Outcome::kClean:
                return true;
            case Outcome::kSkipped:
                result.skip_message = FirstMessage(parts);
                return false;
            case Outcome::kFailed:
                ReportFailures(parts);
                return false;
            case Outcome::kConflict:
                ++result.conflicts;
                if (attempt >= kMaxConflictRetries) {
                    ADD_FAILURE() << tag << ": connection still busy after "
                                  << kMaxConflictRetries
                                  << " retries; last: " << FirstMessage(parts);
                    return false;
                }
                std::this_thread::sleep_for(std::chrono::milliseconds(1));
                break;
        }
    }
}

template <typename T>
void RunConcurrent(T& coordinator, Mode mode, const char* name) {
    const Config config = LoadConfig();
    ASSERT_FALSE(::testing::Test::HasFailure());
    const SQLHENV env = coordinator.ConcurrentEnv();
    const SQLHDBC dbc = mode == kSharedDbc ? coordinator.ConcurrentDbc() : SQL_NULL_HDBC;
    ASSERT_TRUE(env != SQL_NULL_HENV) << name << ": fixture SetUp() allocated no ENV";
    if (mode == kSharedDbc) {
        ASSERT_TRUE(dbc != SQL_NULL_HDBC)
            << name << ": SharedDbc needs a fixture that calls Connect() in SetUp()";
    }

    const std::string label = std::string(name) + " [" + ModeName(mode) + "]";
    std::cout << label << ": threads=" << config.threads << " iterations=" << config.iterations
              << std::endl;

    const size_t count = static_cast<size_t>(config.threads);
    std::vector<WorkerResult> results(count);
    // One per worker, so retaining never synchronizes between them. Destroyed
    // after the join below; see RetainedFixtures.
    std::vector<RetainedFixtures<T>> retained(count);
    std::vector<std::atomic<int>> progress(count);
    for (std::atomic<int>& p : progress) {
        p.store(0);
    }
    std::atomic<bool> stop{false};
    StartGate gate(config.threads);
    const auto started = std::chrono::steady_clock::now();
    {
        Watchdog watchdog(label, std::chrono::seconds(config.timeout_seconds), progress);
        std::vector<std::thread> workers;
        workers.reserve(count);
        for (int worker = 0; worker < config.threads; ++worker) {
            workers.emplace_back([&, worker] {
                const size_t index = static_cast<size_t>(worker);
                gate.ArriveAndWait();
                for (int iteration = 0; iteration < config.iterations && !stop.load();
                     ++iteration) {
                    const std::string tag = std::string(ModeName(mode)) +
                                            " worker=" + std::to_string(worker) +
                                            " iteration=" + std::to_string(iteration);
                    bool ok = false;
                    if (mode == kSharedEnv) {
                        ok = RunSharedEnvIteration<T>(env, mode, worker, iteration, tag,
                                                      results[index], retained[index]);
                    } else {
                        ok = RunSharedDbcIteration<T>(env, dbc, mode, worker, iteration, tag,
                                                      results[index], retained[index]);
                    }
                    if (!ok) {
                        stop.store(true);
                        break;
                    }
                    ++results[index].completed;
                    progress[index].store(results[index].completed);
                }
            });
        }
        for (std::thread& worker : workers) {
            worker.join();
        }
        // Every worker has joined, so this thread is the only one left to touch
        // the gtest flag globals; see RetainedFixtures.
        retained.clear();
        // Still under the watchdog: a defect that leaves the DBC locked would
        // otherwise hang here with no dump.
        if (mode == kSharedDbc) {
            VerifyConnectionIdle(dbc);
        }
    }
    const long long elapsed_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                                     std::chrono::steady_clock::now() - started)
                                     .count();

    int completed = 0;
    int conflicts = 0;
    std::string skip_message;
    for (const WorkerResult& result : results) {
        completed += result.completed;
        conflicts += result.conflicts;
        if (skip_message.empty()) {
            skip_message = result.skip_message;
        }
    }
    std::cout << label << " completed: iterations=" << completed << " conflicts=" << conflicts
              << " elapsed_ms=" << elapsed_ms << std::endl;

    if (::testing::Test::HasFailure() || ::testing::Test::IsSkipped()) {
        return;
    }
    if (!skip_message.empty()) {
        GTEST_SKIP() << skip_message;
    }
    for (size_t worker = 0; worker < results.size(); ++worker) {
        EXPECT_EQ(config.iterations, results[worker].completed) << "worker=" << worker;
    }
}

// CrossThread: handles are allocated on one thread, used on a second, and freed
// on a third, the way thread pools, async runtimes, and finalizers move them.
// All three threads exist before SetUp starts and outlive TearDown, so the OS
// cannot hand a finished thread's ID to a later phase and let a driver's
// affinity check pass by accident. The phases still run one at a time.
template <typename T>
void RunCrossThread(Mode mode, const char* name) {
    const Config config = LoadConfig();
    ASSERT_FALSE(::testing::Test::HasFailure());
    const std::string label = std::string(name) + " [" + ModeName(mode) + "]";
    std::vector<std::atomic<int>> progress(1);
    progress[0].store(0);
    Watchdog watchdog(label, std::chrono::seconds(config.timeout_seconds), progress);

    std::unique_ptr<T> test(new T(mode));
    std::mutex mutex;
    std::condition_variable cv;
    int phase = 0;
    constexpr int kDone = 3;
    // Each phase thread waits for its turn, runs, passes the turn on, and then
    // stays alive until the last phase has finished.
    const auto phase_thread = [&](int turn, const char* role, std::function<void()> work) {
        return std::thread([&, turn, role, work] {
            std::unique_lock<std::mutex> lock(mutex);
            cv.wait(lock, [&] { return phase == turn; });
            lock.unlock();
            {
                SCOPED_TRACE(role);
                work();
            }
            lock.lock();
            ++phase;
            cv.notify_all();
            cv.wait(lock, [&] { return phase == kDone; });
        });
    };
    std::thread threads[] = {
        phase_thread(0, "CrossThread SetUp thread",
                     [&] {
                         RunPhase("SetUp()", [&] {
                             test->ConcurrentSetUp(SQL_NULL_HENV, SQL_NULL_HDBC, 0, 0);
                         });
                     }),
        phase_thread(1, "CrossThread body thread",
                     [&] {
                         if (!::testing::Test::HasFatalFailure() &&
                             !::testing::Test::IsSkipped()) {
                             RunPhase("the test body", [&] { test->ConcurrentBody(); });
                         }
                     }),
        phase_thread(2, "CrossThread TearDown thread",
                     [&] { RunPhase("TearDown()", [&] { test->ConcurrentTearDown(); }); }),
    };
    for (std::thread& thread : threads) {
        thread.join();
    }
    progress[0].store(1);
}

// Returns false with |error| set when a diagnostic read fails or returns a
// malformed record. Records may be cleared between calls by the owner, so
// SQL_NO_DATA before the advertised count is not an error.
inline bool CheckDiagnostics(SQLSMALLINT type, SQLHANDLE handle, std::string& error) {
    SQLINTEGER count = -1;
    SQLRETURN rc = SQLGetDiagField(type, handle, 0, SQL_DIAG_NUMBER, &count, 0, nullptr);
    if (rc != SQL_SUCCESS || count < 0) {
        error = "SQLGetDiagField(SQL_DIAG_NUMBER) rc=" + std::to_string(rc) +
                " count=" + std::to_string(count);
        return false;
    }
    for (SQLSMALLINT record = 1; record <= count; ++record) {
        SQLTCHAR state[6] = {};
        SQLINTEGER native = 0;
        SQLTCHAR text[1024] = {};
        SQLSMALLINT text_len = -1;
        rc = SQLGetDiagRec(type, handle, record, state, &native, text,
                           static_cast<SQLSMALLINT>(sizeof(text) / sizeof(text[0])), &text_len);
        if (rc == SQL_NO_DATA) {
            return true;
        }
        const std::string where = "record " + std::to_string(record) + ": ";
        if (rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO) {
            error = where + "SQLGetDiagRec rc=" + std::to_string(rc);
            return false;
        }
        // SQLSTATE is five characters plus a terminator. Check the terminator
        // before formatting: ToNarrow takes a SqlTString, which scans for a null,
        // so an unterminated buffer would be read past its end — the corruption
        // this validator exists to catch. The characters are formatted with an
        // explicit length so that holds however this is reordered later.
        if (state[5] != 0) {
            error = where + "unterminated SQLSTATE";
            return false;
        }
        for (int i = 0; i < 5; ++i) {
            const SQLTCHAR c = state[i];
            if (!((c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z'))) {
                error = where + "malformed SQLSTATE '" +
                        ODBCTestUtils::ToNarrow(SqlTString(state, 5)) + "'";
                return false;
            }
        }
        if (text_len < 0) {
            error = where + "negative message length " + std::to_string(text_len);
            return false;
        }
        if (rc == SQL_SUCCESS) {
            size_t length = 0;
            while (length < sizeof(text) / sizeof(text[0]) && text[length] != 0) {
                ++length;
            }
            if (length != static_cast<size_t>(text_len)) {
                error = where + "message length " + std::to_string(length) +
                        " != reported " + std::to_string(text_len);
                return false;
            }
        }
    }
    return true;
}

// SharedStmt: observers read diagnostics while the owner runs the body. They
// see the owner's handles only between SetUp and TearDown; the owner retracts
// them under an exclusive lock, so no observer is inside a call on a handle
// when TearDown frees it. Before retracting, the owner waits for at least one
// observer read, so a short body cannot leave the mode observing nothing.
// Observers keep their first error, including an exception, and the main
// thread reports it, so no thread writes the test's result while the owner
// reads it. MSSQL_ODBC_MT_THREADS=1 runs the owner alone, which is how a
// SharedStmt failure is told apart from a plain bug in the body.
template <typename T>
void RunSharedStmt(T& coordinator, Mode mode, const char* name) {
    const Config config = LoadConfig();
    ASSERT_FALSE(::testing::Test::HasFailure());
    const SQLHENV env = coordinator.ConcurrentEnv();
    ASSERT_TRUE(env != SQL_NULL_HENV) << name << ": fixture SetUp() allocated no ENV";

    const int observers = config.threads - 1;
    const std::string label = std::string(name) + " [" + ModeName(mode) + "]";
    std::cout << label << ": observers=" << observers << " iterations=" << config.iterations
              << std::endl;

    std::shared_mutex handles_mutex;
    SQLHSTMT observed_stmt = SQL_NULL_HSTMT;
    SQLHDBC observed_dbc = SQL_NULL_HDBC;
    std::atomic<bool> retracting{false};
    std::atomic<bool> stop{false};
    std::atomic<bool> observer_failed{false};
    std::mutex observer_error_mutex;
    std::string observer_error;
    const auto record_observer_error = [&](int observer, const std::string& error) {
        std::lock_guard<std::mutex> guard(observer_error_mutex);
        if (observer_error.empty()) {
            observer_error = "observer=" + std::to_string(observer) + " " + error;
        }
        observer_failed.store(true);
    };
    std::atomic<long long> reads{0};
    int observed_iterations = 0;
    std::vector<std::atomic<int>> progress(1);
    progress[0].store(0);
    const auto started = std::chrono::steady_clock::now();
    {
        Watchdog watchdog(label, std::chrono::seconds(config.timeout_seconds), progress);
        std::vector<std::thread> threads;
        // Joinable threads must not outlive this scope, whatever leaves it.
        struct JoinObservers {
            std::atomic<bool>& stop;
            std::vector<std::thread>& threads;
            ~JoinObservers() {
                stop.store(true);
                for (std::thread& thread : threads) {
                    if (thread.joinable()) {
                        thread.join();
                    }
                }
            }
        } join_observers{stop, threads};
        threads.reserve(static_cast<size_t>(observers));
        for (int observer = 0; observer < observers; ++observer) {
            threads.emplace_back([&, observer] {
                const auto observe = [&] {
                    while (!stop.load()) {
                        if (retracting.load()) {
                            std::this_thread::yield();
                            continue;
                        }
                        std::string error;
                        {
                            std::shared_lock<std::shared_mutex> lock(handles_mutex);
                            if (observed_stmt != SQL_NULL_HSTMT) {
                                if (!CheckDiagnostics(SQL_HANDLE_STMT, observed_stmt, error) ||
                                    !CheckDiagnostics(SQL_HANDLE_DBC, observed_dbc, error)) {
                                    record_observer_error(observer, error);
                                    return;
                                }
                                reads.fetch_add(1);
                            }
                        }
                        std::this_thread::yield();
                    }
                };
                if (!GTEST_FLAG_GET(catch_exceptions)) {
                    observe();
                    return;
                }
                try {
                    observe();
                } catch (const std::exception& e) {
                    record_observer_error(observer, std::string("threw \"") + e.what() + "\"");
                } catch (...) {
                    record_observer_error(observer, "threw an unknown exception");
                }
            });
        }

        for (int iteration = 0; iteration < config.iterations; ++iteration) {
            SCOPED_TRACE(std::string(ModeName(mode)) + " iteration=" + std::to_string(iteration));
            std::unique_ptr<T> test;
            if (!RunPhase("the test fixture's constructor", [&] { test.reset(new T(mode)); })) {
                break;
            }
            RunPhase("SetUp()", [&] { test->ConcurrentSetUp(env, SQL_NULL_HDBC, 0, iteration); });
            if (!::testing::Test::HasFatalFailure() && !::testing::Test::IsSkipped()) {
                if (test->ConcurrentStmt() == SQL_NULL_HSTMT ||
                    test->ConcurrentDbc() == SQL_NULL_HDBC) {
                    // Observers count a read only against a published, non-null
                    // STMT, so the wait below could never be satisfied: it would
                    // spin until the watchdog killed the process. Fail here
                    // instead; TearDown still runs.
                    ADD_FAILURE()
                        << label
                        << ": SharedStmt needs a fixture that calls Connect() in SetUp(); "
                           "a fixture that allocates no statement must leave the mode out "
                           "with ODBC_TEST_F_MODES";
                } else {
                    long long reads_before = 0;
                    {
                        std::unique_lock<std::shared_mutex> lock(handles_mutex);
                        observed_stmt = test->ConcurrentStmt();
                        observed_dbc = test->ConcurrentDbc();
                        reads_before = reads.load();
                    }
                    RunPhase("the test body", [&] { test->ConcurrentBody(); });
                    while (observers > 0 && reads.load() == reads_before &&
                           !observer_failed.load()) {
                        std::this_thread::yield();
                    }
                    ++observed_iterations;
                    retracting.store(true);
                    {
                        std::unique_lock<std::shared_mutex> lock(handles_mutex);
                        observed_stmt = SQL_NULL_HSTMT;
                        observed_dbc = SQL_NULL_HDBC;
                    }
                    retracting.store(false);
                }
            }
            RunPhase("TearDown()", [&] { test->ConcurrentTearDown(); });
            progress[0].store(iteration + 1);
            if (::testing::Test::HasFailure() || ::testing::Test::IsSkipped() ||
                observer_failed.load()) {
                break;
            }
        }
    }
    const long long elapsed_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                                     std::chrono::steady_clock::now() - started)
                                     .count();
    std::cout << label << " completed: iterations=" << progress[0].load()
              << " diagnostic_reads=" << reads.load() << " elapsed_ms=" << elapsed_ms
              << std::endl;

    if (!observer_error.empty()) {
        ADD_FAILURE() << label << " " << observer_error;
        return;
    }
    if (observers > 0 && observed_iterations > 0 && !::testing::Test::HasFailure()) {
        EXPECT_GE(reads.load(), observed_iterations)
            << label << ": observers did not read the owner's diagnostics every iteration";
    }
}

}  // namespace detail

template <typename T>
void RunTest(T& test, Mode mode, const char* name) {
    switch (mode) {
        case kSingle:
            test.ConcurrentBody();
            return;
        case kCrossThread:
            detail::RunCrossThread<T>(mode, name);
            return;
        case kSharedEnv:
        case kSharedDbc:
            detail::RunConcurrent(test, mode, name);
            return;
        case kSharedStmt:
            detail::RunSharedStmt(test, mode, name);
            return;
    }
    ADD_FAILURE() << name << ": unhandled mode " << static_cast<unsigned>(mode);
}

// The factory returns Fixture*, so gtest files these tests under the same
// fixture id as TEST_F and both macros can share a suite.
template <typename Fixture, typename T>
bool Register(const char* suite, const char* name, const char* file, int line, unsigned modes) {
    for (Mode mode : kModes) {
        if ((modes & mode) == 0) {
            continue;
        }
        const std::string test_name =
            mode == kSingle ? std::string(name) : std::string(name) + "/" + ModeName(mode);
        const std::string label = std::string(suite) + "." + test_name;
        ::testing::RegisterTest(suite, test_name.c_str(), nullptr, nullptr, file, line,
                                [mode, label]() -> Fixture* {
                                    T* test = new T(mode);
                                    if (mode != kSingle) {
                                        test->WatchCoordinator(label);
                                    }
                                    return test;
                                });
    }
    return true;
}

}  // namespace odbc_concurrency

#define ODBC_CONCURRENT_TEST_CLASS_(Fixture, Name) Fixture##_##Name##_OdbcConcurrentTest

#define ODBC_TEST_F_MODES(Fixture, Name, modes)                                                \
    class ODBC_CONCURRENT_TEST_CLASS_(Fixture, Name) final                                     \
        : public Fixture,                                                                      \
          public ::odbc_concurrency::WorkerScope,                                              \
          public ::odbc_concurrency::detail::CoordinatorWatch {                                \
       public:                                                                                 \
        explicit ODBC_CONCURRENT_TEST_CLASS_(Fixture, Name)(::odbc_concurrency::Mode mode)     \
            : mode_(mode) {}                                                                   \
        void TestBody() override {                                                             \
            RunWatched([this] {                                                                \
                ::odbc_concurrency::RunTest(*this, mode_, #Fixture "." #Name);                 \
            });                                                                                \
        }                                                                                      \
        void ConcurrentBody();                                                                 \
        void ConcurrentSetUp(SQLHENV env, SQLHDBC dbc, int worker, int iteration) {            \
            LendHandles(env, dbc);                                                             \
            SetWorker(worker, iteration);                                                      \
            SetUp();                                                                           \
        }                                                                                      \
        void ConcurrentTearDown() { TearDown(); }                                              \
        SQLHENV ConcurrentEnv() const { return env_; }                                         \
        SQLHDBC ConcurrentDbc() const { return dbc_; }                                         \
        SQLHSTMT ConcurrentStmt() const { return stmt_; }                                      \
                                                                                               \
       private:                                                                                \
        ::odbc_concurrency::Mode mode_;                                                        \
    };                                                                                         \
    [[maybe_unused]] static const bool Fixture##_##Name##_odbc_concurrent_registered =         \
        ::odbc_concurrency::Register<Fixture, ODBC_CONCURRENT_TEST_CLASS_(Fixture, Name)>(     \
            #Fixture, #Name, __FILE__, __LINE__, (modes));                                     \
    void ODBC_CONCURRENT_TEST_CLASS_(Fixture, Name)::ConcurrentBody()

#define ODBC_TEST_F(Fixture, Name) \
    ODBC_TEST_F_MODES(Fixture, Name, ::odbc_concurrency::kDefaultModes)
