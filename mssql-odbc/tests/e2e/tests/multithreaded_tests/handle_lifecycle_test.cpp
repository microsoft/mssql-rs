// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Handle churn (plan scenario 7, AB#49261): threads repeatedly allocate,
// connect, query, disconnect, and free whole handle trees, then check that the
// process released what those trees used. Sized by MSSQL_ODBC_MT_THREADS and
// MSSQL_ODBC_MT_ITERS and bounded by the MSSQL_ODBC_MT_TIMEOUT_SECONDS watchdog,
// like the ODBC_TEST_F concurrent variants (odbc_concurrent_test.h). The ctest
// entry is handle_lifecycle_test_mt, labelled mt_weekly too, so only the
// weekly profile runs it.
//
// Out of scope until handle lifetimes are reference-counted or generation-
// checked: freeing or disconnecting a handle while another thread uses it.

#include "odbc_concurrent_test.h"

#if defined(_WIN32)
#include <tlhelp32.h>
#elif defined(__linux__)
#include <dirent.h>
#endif

#include <algorithm>
#include <atomic>
#include <chrono>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace {

using odbc_concurrency::detail::Config;
using odbc_concurrency::detail::LoadConfig;
using odbc_concurrency::detail::StartGate;
using odbc_concurrency::detail::Watchdog;

class Failures {
   public:
    void Add(std::string what) {
        std::lock_guard<std::mutex> lock(mutex_);
        failures_.push_back(std::move(what));
    }

    std::string Summary() const {
        std::lock_guard<std::mutex> lock(mutex_);
        std::string text = std::to_string(failures_.size()) + " failure(s)";
        const size_t shown = (std::min)(failures_.size(), size_t{10});
        for (size_t i = 0; i < shown; ++i) {
            text += "\n  " + failures_[i];
        }
        return text;
    }

    bool Empty() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return failures_.empty();
    }

   private:
    mutable std::mutex mutex_;
    std::vector<std::string> failures_;
};

std::string DiagText(SQLSMALLINT type, SQLHANDLE handle) {
    std::string text;
    for (SQLSMALLINT record = 1;; ++record) {
        SQLTCHAR state[8] = {};
        SQLTCHAR message[1024] = {};
        SQLINTEGER native = 0;
        SQLSMALLINT length = 0;
        if (!SQL_SUCCEEDED(SQLGetDiagRec(
                type, handle, record, state, &native, message,
                static_cast<SQLSMALLINT>(sizeof(message) / sizeof(SQLTCHAR)), &length))) {
            break;
        }
        text += (record > 1 ? " | [" : "[") + ODBCTestUtils::ToNarrow(SqlTString(state)) + "] " +
                ODBCTestUtils::ToNarrow(SqlTString(message));
    }
    return text;
}

std::string Describe(const char* call, SQLRETURN rc, SQLSMALLINT type, SQLHANDLE handle) {
    return std::string(call) + " returned " + std::to_string(rc) + ": " + DiagText(type, handle);
}

SQLRETURN DriverConnect(SQLHDBC dbc) {
    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR out[1024] = {};
    SQLSMALLINT out_length = 0;
    return SQLDriverConnect(dbc, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                            static_cast<SQLSMALLINT>(connstr.size()), out,
                            static_cast<SQLSMALLINT>(sizeof(out) / sizeof(SQLTCHAR)),
                            &out_length, SQL_DRIVER_NOPROMPT);
}

// Fetches |token| through a fresh STMT, so a row delivered to the wrong
// connection or a statement left over from an earlier tree cannot match.
bool QueryToken(SQLHDBC dbc, long long token, std::string& error) {
    SQLHSTMT stmt = SQL_NULL_HSTMT;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc, &stmt);
    if (!SQL_SUCCEEDED(rc)) {
        error = Describe("SQLAllocHandle(STMT)", rc, SQL_HANDLE_DBC, dbc);
        return false;
    }
    const std::string expected = std::to_string(token);
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT CAST(" + expected + " AS bigint)");
    SQLTCHAR value[64] = {};
    SQLLEN length = 0;
    if (!SQL_SUCCEEDED(rc = SQLExecDirect(stmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS))) {
        error = Describe("SQLExecDirect", rc, SQL_HANDLE_STMT, stmt);
    } else if (!SQL_SUCCEEDED(rc = SQLFetch(stmt))) {
        error = Describe("SQLFetch", rc, SQL_HANDLE_STMT, stmt);
    } else if (!SQL_SUCCEEDED(
                   rc = SQLGetData(stmt, 1, SQL_C_TCHAR, value, sizeof(value), &length))) {
        error = Describe("SQLGetData", rc, SQL_HANDLE_STMT, stmt);
    } else if (ODBCTestUtils::ToNarrow(SqlTString(value)) != expected) {
        error = "SELECT " + expected + " returned " + ODBCTestUtils::ToNarrow(SqlTString(value));
    } else if ((rc = SQLFetch(stmt)) != SQL_NO_DATA) {
        error = Describe("second SQLFetch", rc, SQL_HANDLE_STMT, stmt);
    }
    const bool fetched = error.empty();
    if (!SQL_SUCCEEDED(rc = SQLFreeHandle(SQL_HANDLE_STMT, stmt)) && fetched) {
        error = Describe("SQLFreeHandle(STMT)", rc, SQL_HANDLE_STMT, stmt);
    }
    return error.empty();
}

// One ENV/DBC/STMT tree. Each step is checked on the way through; on failure
// the destructor releases whatever is left without checking.
class HandleTree {
   public:
    explicit HandleTree(SQLHENV shared_env)
        : env_(shared_env), owns_env_(shared_env == SQL_NULL_HENV) {}

    ~HandleTree() {
        if (connected_) {
            SQLDisconnect(dbc_);
        }
        if (dbc_ != SQL_NULL_HDBC) {
            SQLFreeHandle(SQL_HANDLE_DBC, dbc_);
        }
        if (owns_env_ && env_ != SQL_NULL_HENV) {
            SQLFreeHandle(SQL_HANDLE_ENV, env_);
        }
    }

    HandleTree(const HandleTree&) = delete;
    HandleTree& operator=(const HandleTree&) = delete;

    bool Run(long long token, std::string& error) {
        SQLRETURN rc;
        if (owns_env_) {
            if (!SQL_SUCCEEDED(rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env_))) {
                error = "SQLAllocHandle(ENV) returned " + std::to_string(rc);
                env_ = SQL_NULL_HENV;
                return false;
            }
            if (!SQL_SUCCEEDED(rc = SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                                                  reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80),
                                                  0))) {
                error = Describe("SQLSetEnvAttr(ODBC_VERSION)", rc, SQL_HANDLE_ENV, env_);
                return false;
            }
        }
        if (!SQL_SUCCEEDED(rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc_))) {
            error = Describe("SQLAllocHandle(DBC)", rc, SQL_HANDLE_ENV, env_);
            dbc_ = SQL_NULL_HDBC;
            return false;
        }
        if (!SQL_SUCCEEDED(rc = DriverConnect(dbc_))) {
            error = Describe("SQLDriverConnect", rc, SQL_HANDLE_DBC, dbc_);
            return false;
        }
        connected_ = true;
        if (!QueryToken(dbc_, token, error)) {
            return false;
        }
        if (!SQL_SUCCEEDED(rc = SQLDisconnect(dbc_))) {
            error = Describe("SQLDisconnect", rc, SQL_HANDLE_DBC, dbc_);
            return false;
        }
        connected_ = false;
        if (!SQL_SUCCEEDED(rc = SQLFreeHandle(SQL_HANDLE_DBC, dbc_))) {
            error = Describe("SQLFreeHandle(DBC)", rc, SQL_HANDLE_DBC, dbc_);
            return false;
        }
        dbc_ = SQL_NULL_HDBC;
        if (owns_env_) {
            if (!SQL_SUCCEEDED(rc = SQLFreeHandle(SQL_HANDLE_ENV, env_))) {
                error = Describe("SQLFreeHandle(ENV)", rc, SQL_HANDLE_ENV, env_);
                return false;
            }
            env_ = SQL_NULL_HENV;
        }
        return true;
    }

   private:
    SQLHENV env_;
    bool owns_env_;
    SQLHDBC dbc_ = SQL_NULL_HDBC;
    bool connected_ = false;
};

// Live OS threads and open handles (Windows) or file descriptors (Linux) in
// this process; -1 where the platform has no reader.
struct ProcessCounts {
    long threads = -1;
    long handles = -1;
};

ProcessCounts SampleProcess() {
    ProcessCounts counts;
#if defined(_WIN32)
    DWORD handles = 0;
    if (GetProcessHandleCount(GetCurrentProcess(), &handles)) {
        counts.handles = static_cast<long>(handles);
    }
    const HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if (snapshot != INVALID_HANDLE_VALUE) {
        THREADENTRY32 entry = {};
        entry.dwSize = sizeof(entry);
        long threads = 0;
        for (BOOL more = Thread32First(snapshot, &entry); more;
             more = Thread32Next(snapshot, &entry)) {
            threads += entry.th32OwnerProcessID == GetCurrentProcessId() ? 1 : 0;
        }
        CloseHandle(snapshot);
        counts.threads = threads;
    }
#elif defined(__linux__)
    auto entries = [](const char* path) -> long {
        DIR* dir = opendir(path);
        if (dir == nullptr) {
            return -1;
        }
        long count = 0;
        while (const dirent* entry = readdir(dir)) {
            count += entry->d_name[0] != '.' ? 1 : 0;
        }
        closedir(dir);
        return count;
    };
    counts.threads = entries("/proc/self/task");
    counts.handles = entries("/proc/self/fd");
#endif
    return counts;
}

// Runs |work(worker, iteration, error)| on config.threads threads released
// together, each for config.iterations iterations, under the harness watchdog.
// The first failure stops everyone. Returns the iterations completed.
template <typename Work>
int RunWorkers(const std::string& label, const Config& config, Failures& failures, Work work) {
    std::vector<std::atomic<int>> progress(static_cast<size_t>(config.threads));
    std::atomic<bool> stop{false};
    StartGate gate(config.threads);
    {
        Watchdog watchdog(label, std::chrono::seconds(config.timeout_seconds), progress);
        std::vector<std::thread> threads;
        for (int w = 0; w < config.threads; ++w) {
            threads.emplace_back([&, w] {
                gate.ArriveAndWait();
                for (int i = 0; i < config.iterations && !stop; ++i) {
                    std::string error;
                    if (!work(w, i, error)) {
                        failures.Add(label + ": worker " + std::to_string(w) + " iteration " +
                                     std::to_string(i) + ": " + error);
                        stop = true;
                        break;
                    }
                    ++progress[static_cast<size_t>(w)];
                }
            });
        }
        for (auto& thread : threads) {
            thread.join();
        }
    }
    int completed = 0;
    for (const auto& count : progress) {
        completed += count.load();
    }
    return completed;
}

std::string TestLabel() {
    const auto* info = ::testing::UnitTest::GetInstance()->current_test_info();
    return std::string(info->test_suite_name()) + "." + info->name();
}

// SetUp and TearDown (which frees the ENV and joins its runtime) run under the
// harness's coordinator watchdogs, like an ODBC_TEST_F coordinator's; each test
// body runs inside RunWatched.
class HandleLifecycleTest : public ODBCTest, public odbc_concurrency::detail::CoordinatorWatch {
   protected:
    void SetUp() override {
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
            << "Set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        WatchCoordinator(TestLabel());
        ASSERT_NO_FATAL_FAILURE(ODBCTest::SetUp());
        config_ = LoadConfig();
    }

    void Churn(bool shared_env);

    Config config_;
};

// Thread and handle counts may wobble by a few (Windows thread-pool workers,
// the Driver Manager's caches); a leak of one per tree grows them by hundreds.
constexpr long kThreadSlack = 8;
constexpr long kHandleSlack = 64;

// Each worker builds and tears down a whole tree per iteration, either under
// its own ENV or under the fixture's shared ENV. Churn is cheap (16 x 50 trees
// take under a second locally) and its races need volume, so it runs
// kChurnRounds rounds of the configured size, each under its own watchdog so
// the hang timeout still bounds a normal-sized phase. Thread and handle counts
// sampled after the first round must be back after the last, so a runtime,
// socket, or thread kept past its handle fails.
void HandleLifecycleTest::Churn(bool shared_env) {
    constexpr int kChurnRounds = 20;
    const SQLHENV env = shared_env ? env_ : SQL_NULL_HENV;
    Failures failures;
    long long completed = 0;
    ProcessCounts baseline;
    for (int round = 0; round < kChurnRounds && failures.Empty(); ++round) {
        completed += RunWorkers(
            TestLabel() + " round " + std::to_string(round), config_, failures,
            [&, round](int worker, int iteration, std::string& error) {
                HandleTree tree(env);
                const long long run =
                    (static_cast<long long>(round) * config_.threads + worker) *
                        config_.iterations +
                    iteration;
                return tree.Run(1 + run, error);
            });
        if (round == 0) {
            baseline = SampleProcess();
        }
    }
    const ProcessCounts after = SampleProcess();

    EXPECT_TRUE(failures.Empty()) << failures.Summary();
    EXPECT_EQ(static_cast<long long>(kChurnRounds) * config_.threads * config_.iterations,
              completed);
    RecordProperty("threads_after_round0_and_last",
                   std::to_string(baseline.threads) + " " + std::to_string(after.threads));
    RecordProperty("handles_after_round0_and_last",
                   std::to_string(baseline.handles) + " " + std::to_string(after.handles));
    if (failures.Empty() && baseline.threads >= 0) {
        EXPECT_LE(after.threads, baseline.threads + kThreadSlack)
            << "threads grew from " << baseline.threads << " after round 0 to " << after.threads;
    }
    if (failures.Empty() && baseline.handles >= 0) {
        EXPECT_LE(after.handles, baseline.handles + kHandleSlack)
            << "handles grew from " << baseline.handles << " after round 0 to " << after.handles;
    }
}

// Every thread repeatedly allocates its own ENV, DBC, and STMT, connects,
// queries, disconnects, and frees them, so ENV creation and teardown (each of
// our ENVs owns a Tokio runtime) race other trees' I/O.
TEST_F(HandleLifecycleTest, ChurnPrivateEnvTrees) {
    RunWatched([&] { Churn(false); });
}

// The same under one ENV: DBC allocation and freeing race on the ENV's shared
// child list and runtime while other DBCs connect and query.
TEST_F(HandleLifecycleTest, ChurnDbcTreesOnSharedEnv) {
    RunWatched([&] { Churn(true); });
}

}  // namespace
