// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Shared pieces of the multithreaded-only lifecycle and fault tests
// (handle_lifecycle_test.cpp, fault_recovery_test.cpp): failure collection,
// connection helpers, process resource sampling, and local sockets that stand
// in for unreachable or unresponsive servers.

#pragma once

#ifdef _WIN32
#include <winsock2.h>
#include <ws2tcpip.h>
#else
#include <netinet/in.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <unistd.h>
#endif

#include "odbc_concurrent_test.h"

#if defined(_WIN32)
#include <tlhelp32.h>
#elif defined(__linux__)
#include <dirent.h>
#endif

#include <algorithm>
#include <atomic>
#include <cctype>
#include <chrono>
#include <functional>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <utility>
#include <vector>

namespace lifecycle {

using odbc_concurrency::detail::Config;
using odbc_concurrency::detail::LoadConfig;
using odbc_concurrency::detail::StartGate;
using odbc_concurrency::detail::Watchdog;
using Clock = std::chrono::steady_clock;

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

inline std::string DiagText(SQLSMALLINT type, SQLHANDLE handle) {
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

inline std::string Describe(const char* call, SQLRETURN rc, SQLSMALLINT type, SQLHANDLE handle) {
    return std::string(call) + " returned " + std::to_string(rc) + ": " + DiagText(type, handle);
}

inline std::string TestLabel() {
    const auto* info = ::testing::UnitTest::GetInstance()->current_test_info();
    return std::string(info->test_suite_name()) + "." + info->name();
}

inline long long MillisecondsSince(Clock::time_point start) {
    return std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - start).count();
}

// --- Connection strings -----------------------------------------------------

inline std::string BaseConnStr() {
    return ODBCTestUtils::ToNarrow(ODBCTestUtils::BuildConnectionString());
}

inline std::string Lower(std::string text) {
    std::transform(text.begin(), text.end(), text.begin(),
                   [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    return text;
}

inline std::string Trim(const std::string& text) {
    static const char* const kSpace = " \t\n\v\f\r";
    const auto first = text.find_first_not_of(kSpace);
    if (first == std::string::npos) {
        return "";
    }
    return text.substr(first, text.find_last_not_of(kSpace) - first + 1);
}

// Keywords that name the same attribute, so an override replaces all of them.
inline std::vector<std::string> Synonyms(const std::string& key) {
    static const std::vector<std::vector<std::string>> groups = {
        {"server", "address", "addr", "data source"},
        {"database", "initial catalog"},
        {"uid", "user id"},
        {"pwd", "password"},
        {"trusted_connection", "integrated security"},
    };
    const std::string lower = Lower(key);
    for (const auto& group : groups) {
        if (std::find(group.begin(), group.end(), lower) != group.end()) {
            return group;
        }
    }
    return {lower};
}

// |base| with each override's keyword (and its synonyms) replaced by the
// override, or removed when the override's value is empty. Splits values as
// the driver's parser does (src/connection/connection_string_parser.rs): a
// value is braced only when it starts with '{', may then contain ';', and ends
// at the first single '}' ("}}" is a literal '}'). Values are kept verbatim.
inline std::string ConnStrWith(const std::string& base,
                               const std::vector<std::pair<std::string, std::string>>& overrides) {
    std::vector<std::pair<std::string, std::string>> attributes;
    size_t i = 0;
    while (i < base.size()) {
        const size_t equals = base.find('=', i);
        const size_t separator = base.find(';', i);
        if (equals == std::string::npos || (separator != std::string::npos && separator < equals)) {
            i = separator == std::string::npos ? base.size() : separator + 1;
            continue;
        }
        size_t end = equals + 1;
        if (end < base.size() && base[end] == '{') {
            for (++end; end < base.size(); ++end) {
                if (base[end] == '}') {
                    if (end + 1 < base.size() && base[end + 1] == '}') {
                        ++end;
                        continue;
                    }
                    ++end;
                    break;
                }
            }
        }
        end = (std::min)(base.find(';', end), base.size());
        attributes.emplace_back(Trim(base.substr(i, equals - i)),
                                base.substr(equals + 1, end - equals - 1));
        i = end + 1;
    }
    for (const auto& item : overrides) {
        const auto names = Synonyms(item.first);
        attributes.erase(std::remove_if(attributes.begin(), attributes.end(),
                                        [&](const std::pair<std::string, std::string>& a) {
                                            return std::find(names.begin(), names.end(),
                                                             Lower(a.first)) != names.end();
                                        }),
                         attributes.end());
        if (!item.second.empty()) {
            attributes.push_back(item);
        }
    }
    std::string text;
    for (const auto& attribute : attributes) {
        text += attribute.first + "=" + attribute.second + ";";
    }
    return text;
}

inline SQLRETURN DriverConnect(SQLHDBC dbc, const std::string& connstr) {
    SqlTString text = ODBCTestUtils::ToSqlTStr(connstr);
    SQLTCHAR out[1024] = {};
    SQLSMALLINT out_length = 0;
    return SQLDriverConnect(dbc, nullptr, const_cast<SQLTCHAR*>(text.c_str()),
                            static_cast<SQLSMALLINT>(text.size()), out,
                            static_cast<SQLSMALLINT>(sizeof(out) / sizeof(SQLTCHAR)),
                            &out_length, SQL_DRIVER_NOPROMPT);
}

inline SQLRETURN ExecDirect(SQLHSTMT stmt, const std::string& sql) {
    SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
    return SQLExecDirect(stmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
}

// Runs |sql| on |stmt|, reads its single row's first column as text, and
// closes the cursor.
inline bool QueryScalar(SQLHSTMT stmt, const std::string& sql, std::string& value,
                        std::string& error) {
    SQLRETURN rc;
    SQLTCHAR buffer[128] = {};
    SQLLEN length = 0;
    if (!SQL_SUCCEEDED(rc = ExecDirect(stmt, sql))) {
        error = Describe("SQLExecDirect", rc, SQL_HANDLE_STMT, stmt);
    } else if (!SQL_SUCCEEDED(rc = SQLFetch(stmt))) {
        error = Describe("SQLFetch", rc, SQL_HANDLE_STMT, stmt);
    } else if (!SQL_SUCCEEDED(
                   rc = SQLGetData(stmt, 1, SQL_C_TCHAR, buffer, sizeof(buffer), &length))) {
        error = Describe("SQLGetData", rc, SQL_HANDLE_STMT, stmt);
    } else if ((rc = SQLFetch(stmt)) != SQL_NO_DATA) {
        error = Describe("second SQLFetch", rc, SQL_HANDLE_STMT, stmt);
    } else {
        value = length == SQL_NULL_DATA ? "NULL" : ODBCTestUtils::ToNarrow(SqlTString(buffer));
    }
    SQLFreeStmt(stmt, SQL_CLOSE);
    if (!error.empty()) {
        error += " (running " + sql + ")";
    }
    return error.empty();
}

// Selects |token| on |stmt|: a row delivered to the wrong connection or a
// result left over from an earlier call cannot match.
inline bool CheckToken(SQLHSTMT stmt, long long token, std::string& error) {
    const std::string expected = std::to_string(token);
    std::string value;
    if (!QueryScalar(stmt, "SELECT CAST(" + expected + " AS bigint)", value, error)) {
        return false;
    }
    if (value != expected) {
        error = "SELECT " + expected + " returned " + value;
        return false;
    }
    return true;
}

// A DBC and one STMT on |env|, freed unchecked on destruction.
class Connection {
   public:
    explicit Connection(SQLHENV env) : env_(env) {}
    ~Connection() {
        if (stmt_ != SQL_NULL_HSTMT) {
            SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
        }
        if (connected_) {
            SQLDisconnect(dbc_);
        }
        if (dbc_ != SQL_NULL_HDBC) {
            SQLFreeHandle(SQL_HANDLE_DBC, dbc_);
        }
    }

    Connection(const Connection&) = delete;
    Connection& operator=(const Connection&) = delete;

    bool Open(std::string& error) { return Open(BaseConnStr(), error); }

    bool Open(const std::string& connstr, std::string& error) {
        SQLRETURN rc;
        if (!SQL_SUCCEEDED(rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc_))) {
            error = Describe("SQLAllocHandle(DBC)", rc, SQL_HANDLE_ENV, env_);
            dbc_ = SQL_NULL_HDBC;
            return false;
        }
        if (!SQL_SUCCEEDED(rc = DriverConnect(dbc_, connstr))) {
            error = Describe("SQLDriverConnect", rc, SQL_HANDLE_DBC, dbc_);
            return false;
        }
        connected_ = true;
        if (!SQL_SUCCEEDED(rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_))) {
            error = Describe("SQLAllocHandle(STMT)", rc, SQL_HANDLE_DBC, dbc_);
            stmt_ = SQL_NULL_HSTMT;
            return false;
        }
        return true;
    }

    // Frees the STMT, disconnects, and frees the DBC, checking each step.
    bool Close(std::string& error) {
        SQLRETURN rc;
        if (stmt_ != SQL_NULL_HSTMT) {
            if (!SQL_SUCCEEDED(rc = SQLFreeHandle(SQL_HANDLE_STMT, stmt_))) {
                error = Describe("SQLFreeHandle(STMT)", rc, SQL_HANDLE_STMT, stmt_);
                return false;
            }
            stmt_ = SQL_NULL_HSTMT;
        }
        if (connected_) {
            if (!SQL_SUCCEEDED(rc = SQLDisconnect(dbc_))) {
                error = Describe("SQLDisconnect", rc, SQL_HANDLE_DBC, dbc_);
                return false;
            }
            connected_ = false;
        }
        if (dbc_ != SQL_NULL_HDBC) {
            if (!SQL_SUCCEEDED(rc = SQLFreeHandle(SQL_HANDLE_DBC, dbc_))) {
                error = Describe("SQLFreeHandle(DBC)", rc, SQL_HANDLE_DBC, dbc_);
                return false;
            }
            dbc_ = SQL_NULL_HDBC;
        }
        return true;
    }

    SQLHDBC dbc() const { return dbc_; }
    SQLHSTMT stmt() const { return stmt_; }

   private:
    SQLHENV env_;
    SQLHDBC dbc_ = SQL_NULL_HDBC;
    SQLHSTMT stmt_ = SQL_NULL_HSTMT;
    bool connected_ = false;
};

// --- Workers ----------------------------------------------------------------

// A run's position across all rounds: 0, 1, 2, ... in (round, worker,
// iteration) order. Scenarios pick their case from it, so the mix of cases
// does not depend on the profile's thread and iteration counts.
inline long long RunIndex(int round, const Config& config, int worker, int iteration) {
    return (static_cast<long long>(round) * config.threads + worker) * config.iterations +
           iteration;
}

// At least |minimum| rounds, and enough that |cases| consecutive run indexes
// occur even at the smallest profile size (the msodbcsql leg runs 2 x 1).
inline int RoundsCovering(int minimum, int cases, const Config& config) {
    const int per_round = config.threads * config.iterations;
    return (std::max)(minimum, (cases + per_round - 1) / per_round);
}

// Runs |work(worker, iteration, error)| on config.threads threads released
// together, each for config.iterations iterations, under the harness watchdog,
// plus |spinners| threads that run |spin(spinner, keep_going, error)| from the
// same start. |spin| loops while keep_going() - until every worker finishes or
// one fails. The first failure stops everyone. Returns the iterations
// completed.
template <typename Work, typename Spin>
int RunWorkers(const std::string& label, const Config& config, Failures& failures, Work work,
               int spinners, Spin spin) {
    std::vector<std::atomic<int>> progress(static_cast<size_t>(config.threads));
    std::atomic<bool> stop{false};
    std::atomic<int> working{config.threads};
    StartGate gate(config.threads + spinners);
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
                --working;
            });
        }
        for (int s = 0; s < spinners; ++s) {
            threads.emplace_back([&, s] {
                gate.ArriveAndWait();
                std::string error;
                if (!spin(s, [&] { return working > 0 && !stop; }, error)) {
                    failures.Add(label + ": spinner " + std::to_string(s) + ": " + error);
                    stop = true;
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

template <typename Work>
int RunWorkers(const std::string& label, const Config& config, Failures& failures, Work work) {
    return RunWorkers(label, config, failures, work, 0,
                      [](int, const std::function<bool()>&, std::string&) { return true; });
}

// --- Process resources ------------------------------------------------------

// Live OS threads and open handles (Windows) or file descriptors (Linux) in
// this process; -1 where the platform has no reader.
struct ProcessCounts {
    long threads = -1;
    long handles = -1;
};

inline ProcessCounts SampleProcess() {
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

// Thread and handle counts may wobble by a few (Windows thread-pool workers,
// the Driver Manager's caches); a leak of one per connection grows them by
// hundreds.
constexpr long kThreadSlack = 8;
constexpr long kHandleSlack = 64;

// Records both samples and fails if the later one grew past the slack, or
// could not be taken although the first could (descriptor exhaustion can make
// /proc unreadable). Call it only for a run that otherwise passed: a failed
// run may legitimately leave things behind.
inline void ExpectNoGrowth(const ProcessCounts& baseline, const ProcessCounts& after) {
    ::testing::Test::RecordProperty(
        "threads_after_round0_and_last",
        std::to_string(baseline.threads) + " " + std::to_string(after.threads));
    ::testing::Test::RecordProperty(
        "handles_after_round0_and_last",
        std::to_string(baseline.handles) + " " + std::to_string(after.handles));
    if (baseline.threads >= 0) {
        EXPECT_GE(after.threads, 0) << "the final thread count could not be sampled";
        EXPECT_LE(after.threads, baseline.threads + kThreadSlack)
            << "threads grew from " << baseline.threads << " after round 0 to " << after.threads;
    }
    if (baseline.handles >= 0) {
        EXPECT_GE(after.handles, 0) << "the final handle count could not be sampled";
        EXPECT_LE(after.handles, baseline.handles + kHandleSlack)
            << "handles grew from " << baseline.handles << " after round 0 to " << after.handles;
    }
}

// --- Local sockets ----------------------------------------------------------

#ifdef _WIN32
using Socket = SOCKET;
constexpr Socket kInvalidSocket = INVALID_SOCKET;
inline void CloseSocket(Socket socket) { closesocket(socket); }
inline void EnsureSockets() {
    static const bool started = [] {
        WSADATA data;
        return WSAStartup(MAKEWORD(2, 2), &data) == 0;
    }();
    if (!started) {
        throw std::runtime_error("WSAStartup failed");
    }
}
#else
using Socket = int;
constexpr Socket kInvalidSocket = -1;
inline void CloseSocket(Socket socket) { close(socket); }
inline void EnsureSockets() {}
#endif

// A TCP socket bound to an ephemeral loopback port.
inline Socket BindLoopback(unsigned& port) {
    EnsureSockets();
    const Socket bound = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (bound == kInvalidSocket) {
        throw std::runtime_error("cannot create a loopback socket");
    }
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
#ifdef _WIN32
    int length = sizeof(address);
#else
    socklen_t length = sizeof(address);
#endif
    if (bind(bound, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0 ||
        getsockname(bound, reinterpret_cast<sockaddr*>(&address), &length) != 0) {
        CloseSocket(bound);
        throw std::runtime_error("cannot bind a loopback socket");
    }
    port = ntohs(address.sin_port);
    return bound;
}

// A loopback port that refuses connections: bound, so nothing else takes it,
// but not listening.
class ClosedPort {
   public:
    ClosedPort() : socket_(BindLoopback(port_)) {}
    ~ClosedPort() { CloseSocket(socket_); }
    ClosedPort(const ClosedPort&) = delete;
    ClosedPort& operator=(const ClosedPort&) = delete;
    unsigned Port() const { return port_; }

   private:
    unsigned port_ = 0;
    Socket socket_;
};

// A loopback server that accepts connections and never answers, so a login
// against it can end only by the login timeout. Accepted sockets stay open
// (the client must give up, not see a reset) until CloseAccepted().
class SilentServer {
   public:
    SilentServer() : listener_(BindLoopback(port_)) {
        if (listen(listener_, SOMAXCONN) != 0) {
            CloseSocket(listener_);
            throw std::runtime_error("cannot listen on a loopback socket");
        }
        thread_ = std::thread([this] { AcceptUntilStopped(); });
    }

    ~SilentServer() {
        stop_ = true;
        thread_.join();
        CloseAccepted();
        CloseSocket(listener_);
    }

    SilentServer(const SilentServer&) = delete;
    SilentServer& operator=(const SilentServer&) = delete;

    unsigned Port() const { return port_; }

    void CloseAccepted() {
        std::lock_guard<std::mutex> lock(mutex_);
        for (const Socket accepted : accepted_) {
            CloseSocket(accepted);
        }
        accepted_.clear();
    }

   private:
    void AcceptUntilStopped() {
        while (!stop_) {
            fd_set ready;
            FD_ZERO(&ready);
            FD_SET(listener_, &ready);
            timeval wait{0, 50000};
#ifdef _WIN32
            const int count = select(0, &ready, nullptr, nullptr, &wait);
#else
            const int count = select(listener_ + 1, &ready, nullptr, nullptr, &wait);
#endif
            if (count > 0) {
                const Socket accepted = accept(listener_, nullptr, nullptr);
                if (accepted != kInvalidSocket) {
                    std::lock_guard<std::mutex> lock(mutex_);
                    accepted_.push_back(accepted);
                }
            }
        }
    }

    unsigned port_ = 0;
    Socket listener_;
    std::atomic<bool> stop_{false};
    std::mutex mutex_;
    std::vector<Socket> accepted_;
    std::thread thread_;
};

// Base fixture: SetUp and TearDown (which frees the ENV and joins its runtime)
// run under the harness's coordinator watchdogs, like an ODBC_TEST_F
// coordinator's; each test body runs inside RunWatched.
class LifecycleTest : public ODBCTest, public odbc_concurrency::detail::CoordinatorWatch {
   protected:
    void SetUp() override {
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
            << "Set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        WatchCoordinator(TestLabel());
        ASSERT_NO_FATAL_FAILURE(ODBCTest::SetUp());
        config_ = LoadConfig();
    }

    Config config_;
};

}  // namespace lifecycle
