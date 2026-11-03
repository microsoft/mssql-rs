// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Faults under load (plan scenarios 8 and 9): one connection is killed,
// stalled, timed out, or cancelled while other connections on the same ENV
// keep working. Every call runs on its caller's thread (`block_on`), but an
// ENV's sockets and timers are all driven by its runtime's single worker
// thread, so anything that blocks that worker - or a connection that hangs or
// fails badly - stalls or corrupts every connection on the ENV. Sized and
// watched like handle_lifecycle_test.cpp; the ctest entry is
// fault_recovery_test_mt, labelled mt_weekly, so only the weekly profile runs
// it. Needs VIEW SERVER STATE and ALTER ANY CONNECTION (KILL), which the CI
// login (sa) and a local administrator have.

#include "lifecycle_support.h"
#include "tcp_pause_proxy.h"

#include <future>
#include <iterator>
#include <memory>
#include <random>

namespace {

using namespace lifecycle;

const char* const kWaitfor = "WAITFOR DELAY '00:00:30'";

// A healthy query may take at most kMaxStallMs; the stalled victim is held for
// kStallHold, so a stall that spreads to healthy connections shows up as a
// wait close to the hold. The gap absorbs agent noise: on a 16-core dev box
// with a local server, healthy queries occasionally took 2-5 s with no fault
// at all (a victim that only slept), on both drivers, and the server showed
// those queries as running, not waiting. Blocking the ENV runtime's worker
// on a stalled read (a mutation) made healthy queries wait 40 s.
constexpr long long kMaxStallMs = 8000;
constexpr std::chrono::seconds kStallHold{15};

// Polls |sql| on |admin| until it returns |want|, for at most |limit|.
bool WaitForScalar(SQLHSTMT admin, const std::string& sql, const std::string& want,
                   std::chrono::milliseconds limit, std::string& error) {
    const Clock::time_point deadline = Clock::now() + limit;
    std::string value;
    while (true) {
        if (!QueryScalar(admin, sql, value, error)) {
            return false;
        }
        if (value == want) {
            return true;
        }
        if (Clock::now() >= deadline) {
            error = sql + " still returned " + value + " after " +
                    std::to_string(limit.count()) + " ms";
            return false;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
}

std::string RunningWaitforSql(const std::string& spid) {
    return "SELECT COUNT(*) FROM sys.dm_exec_requests WHERE session_id = " + spid +
           " AND wait_type = 'WAITFOR'";
}

const char* const kConnectTimeSql =
    "SELECT CONVERT(varchar(30), connect_time, 121) FROM sys.dm_exec_connections "
    "WHERE session_id = @@SPID";

bool Kill(Connection& admin, const std::string& spid, std::string& error) {
    const SQLRETURN rc = ExecDirect(admin.stmt(), "KILL " + spid);
    SQLFreeStmt(admin.stmt(), SQL_CLOSE);
    if (!SQL_SUCCEEDED(rc)) {
        error = Describe("KILL", rc, SQL_HANDLE_STMT, admin.stmt());
        return false;
    }
    return true;
}

// The stall victim connects through TcpPauseProxy, which needs the server as
// a TCP endpoint (ODBC_TEST_SERVER), as in cancel_test.cpp.
bool ProxyUsable() {
    const auto& config = ODBCTestConfig::Instance();
    return !config.HasConnStr() && !config.HasDSN() &&
           config.Server().find('\\') == std::string::npos;
}

class FaultRecoveryTest : public LifecycleTest {
   protected:
    // Runs THREADS - 1 healthy workers that select their own tokens nonstop on
    // the shared ENV while |victim(cycle, admin, error)| faults another
    // connection on that ENV |cycles| times. Healthy workers must never fail
    // and never go kMaxStallMs between two completed queries.
    template <typename Victim>
    void AlongsideHealthyWorkers(int cycles, Victim victim);

    bool KillIdleVictim(Connection& admin, long long token, std::string& error);
    bool KillExecutingVictim(Connection& admin, std::string& error);
    bool StallVictimMidResult(std::string& error);
};

template <typename Victim>
void FaultRecoveryTest::AlongsideHealthyWorkers(int cycles, Victim victim) {
    const auto timeout = std::chrono::seconds(config_.timeout_seconds);
    const int healthy = (std::max)(1, config_.threads - 1);
    Failures failures;
    std::atomic<bool> stop{false};
    std::vector<std::atomic<int>> progress(static_cast<size_t>(healthy));
    std::vector<long long> slowest_ms(static_cast<size_t>(healthy), 0);
    StartGate gate(healthy + 1);
    std::vector<std::thread> workers;
    Connection admin(env_);
    int completed = 0;
    {
        // Covers the workers' and the admin's connects and the gate.
        std::unique_ptr<Watchdog> startup(new Watchdog(TestLabel() + " startup", timeout, progress));
        for (int w = 0; w < healthy; ++w) {
            workers.emplace_back([&, w] {
                Connection connection(env_);
                std::string error;
                const bool opened = connection.Open(error);
                gate.ArriveAndWait();
                if (!opened) {
                    failures.Add("worker " + std::to_string(w) + ": " + error);
                    stop = true;
                    return;
                }
                Clock::time_point last = Clock::now();
                for (long long n = 0; !stop; ++n) {
                    if (!CheckToken(connection.stmt(), (static_cast<long long>(w) << 32) + n,
                                    error)) {
                        failures.Add("worker " + std::to_string(w) + " query " +
                                     std::to_string(n) + ": " + error);
                        stop = true;
                        return;
                    }
                    const Clock::time_point now = Clock::now();
                    auto& slowest = slowest_ms[static_cast<size_t>(w)];
                    slowest = (std::max)(
                        slowest, static_cast<long long>(
                                     std::chrono::duration_cast<std::chrono::milliseconds>(now - last)
                                         .count()));
                    last = now;
                    ++progress[static_cast<size_t>(w)];
                }
                if (!connection.Close(error)) {
                    failures.Add("worker " + std::to_string(w) + " close: " + error);
                }
            });
        }
        std::string error;
        if (!admin.Open(error)) {
            failures.Add("admin connection: " + error);
        }
        gate.ArriveAndWait();
        startup.reset();

        for (int cycle = 0; cycle < cycles && failures.Empty(); ++cycle) {
            Watchdog watchdog(TestLabel() + " cycle " + std::to_string(cycle), timeout, progress);
            if (!victim(cycle, admin, error)) {
                failures.Add("cycle " + std::to_string(cycle) + ": " + error);
                break;
            }
            ++completed;
        }
        stop = true;
        Watchdog stopping(TestLabel() + " stopping workers", timeout, progress);
        for (auto& worker : workers) {
            worker.join();
        }
        if (!admin.Close(error)) {
            failures.Add("admin close: " + error);
        }
    }

    long long slowest = 0;
    for (int w = 0; w < healthy; ++w) {
        EXPECT_GT(progress[static_cast<size_t>(w)].load(), 0) << "worker " << w;
        slowest = (std::max)(slowest, slowest_ms[static_cast<size_t>(w)]);
    }
    RecordProperty("victim_cycles", std::to_string(completed));
    RecordProperty("slowest_healthy_query_ms", std::to_string(slowest));
    EXPECT_TRUE(failures.Empty()) << failures.Summary();
    EXPECT_EQ(cycles, completed);
    EXPECT_LE(slowest, kMaxStallMs) << "a healthy connection stalled while another was faulted";
}

// A victim on the shared ENV is killed while idle. Its next query must
// recover the session (Idle Connection Resiliency) on a new physical
// connection and return its own token.
bool FaultRecoveryTest::KillIdleVictim(Connection& admin, long long token, std::string& error) {
    Connection victim(env_);
    std::string spid;
    std::string connect_time;
    if (!victim.Open(error) || !QueryScalar(victim.stmt(), "SELECT @@SPID", spid, error) ||
        !QueryScalar(victim.stmt(), kConnectTimeSql, connect_time, error) ||
        !Kill(admin, spid, error) ||
        !WaitForScalar(admin.stmt(),
                       "SELECT COUNT(*) FROM sys.dm_exec_connections WHERE session_id = " + spid +
                           " AND CONVERT(varchar(30), connect_time, 121) = '" + connect_time + "'",
                       "0", std::chrono::seconds(10), error)) {
        return false;
    }
    // As in session_recovery_test.cpp: give the client's socket time to see the
    // close. msodbcsql recovers only a connection it already knows is dead; a
    // request sent before the reset arrives fails with 08S01 instead.
    std::this_thread::sleep_for(std::chrono::seconds(1));
    std::string recovered_time;
    if (!CheckToken(victim.stmt(), token, error)) {
        error = "after KILL of the idle session: " + error;
        return false;
    }
    if (!QueryScalar(victim.stmt(), kConnectTimeSql, recovered_time, error)) {
        return false;
    }
    if (recovered_time == connect_time) {
        error = "the killed session answered again (connect_time " + connect_time + ")";
        return false;
    }
    return victim.Close(error);
}

// A victim on the shared ENV is killed while executing. Its call must fail
// promptly, and the connection must still disconnect and free cleanly.
bool FaultRecoveryTest::KillExecutingVictim(Connection& admin, std::string& error) {
    Connection victim(env_);
    std::string spid;
    if (!victim.Open(error) || !QueryScalar(victim.stmt(), "SELECT @@SPID", spid, error)) {
        return false;
    }
    auto running = std::async(std::launch::async, [&victim] {
        return lifecycle::ExecDirect(victim.stmt(), kWaitfor);
    });
    std::string kill_error;
    const bool killed =
        WaitForScalar(admin.stmt(), RunningWaitforSql(spid), "1", std::chrono::seconds(10),
                      kill_error) &&
        Kill(admin, spid, kill_error);
    const Clock::time_point kill_time = Clock::now();
    if (!killed) {
        // Unblock the victim anyway so the cycle ends within the watchdog.
        SQLCancel(victim.stmt());
    }
    const SQLRETURN rc = running.get();
    const long long after_kill_ms = MillisecondsSince(kill_time);
    if (!killed) {
        error = kill_error;
        return false;
    }
    if (rc != SQL_ERROR) {
        error = "the killed WAITFOR returned " + std::to_string(rc);
        return false;
    }
    if (after_kill_ms > 10000) {
        error = "the killed WAITFOR returned " + std::to_string(after_kill_ms) +
                " ms after the KILL";
        return false;
    }
    return victim.Close(error);
}

// Plan scenario 8: a victim on the shared ENV is killed again and again,
// alternately idle and mid-execute, while healthy workers keep querying.
TEST_F(FaultRecoveryTest, KilledConnectionDoesNotDisturbOthers) {
    RunWatched([&] {
        AlongsideHealthyWorkers((std::max)(2, config_.iterations),
                                [&](int cycle, Connection& admin, std::string& error) {
                                    const bool executing = cycle % 2 == 1;
                                    const bool ok = executing ? KillExecutingVictim(admin, error)
                                                              : KillIdleVictim(admin, -1 - cycle,
                                                                               error);
                                    if (!ok) {
                                        error = (executing ? "executing victim: "
                                                           : "idle victim: ") +
                                                error;
                                    }
                                    return ok;
                                });
    });
}

struct StalledRead {
    SQLRETURN rc = SQL_ERROR;
    long long truncated_chunks = 0;
    SQLLEN last_length = 0;
};

// A victim on the shared ENV reads an 8 MB value through a proxy that stops
// relaying the server's response partway through and holds it for kStallHold,
// without closing anything: a peer that has stopped responding. The victim's
// read must stay blocked for the hold, then deliver every byte once released.
bool FaultRecoveryTest::StallVictimMidResult(std::string& error) {
    constexpr long long kValueBytes = 8388608;
    std::unique_ptr<TcpPauseProxy> proxy;
    try {
        proxy.reset(new TcpPauseProxy(ODBCTestConfig::Instance().Server()));
    } catch (const std::exception& e) {
        error = std::string("proxy: ") + e.what();
        return false;
    }
    Connection victim(env_);
    if (!victim.Open(ConnStrWith(BaseConnStr(),
                                 {{"Server", "127.0.0.1," + std::to_string(proxy->Port())}}),
                     error)) {
        return false;
    }
    const SQLHSTMT stmt = victim.stmt();
    char buffer[4096];
    SQLLEN length = 0;
    SQLRETURN rc = lifecycle::ExecDirect(
        stmt, "SELECT REPLICATE(CAST('p' AS varchar(max)), " + std::to_string(kValueBytes) + ")");
    if (!SQL_SUCCEEDED(rc)) {
        error = Describe("SQLExecDirect", rc, SQL_HANDLE_STMT, stmt);
        return false;
    }
    if (!SQL_SUCCEEDED(rc = SQLFetch(stmt))) {
        error = Describe("SQLFetch", rc, SQL_HANDLE_STMT, stmt);
        return false;
    }
    if ((rc = SQLGetData(stmt, 1, SQL_C_CHAR, buffer, sizeof(buffer), &length)) !=
        SQL_SUCCESS_WITH_INFO) {
        error = Describe("first SQLGetData", rc, SQL_HANDLE_STMT, stmt);
        return false;
    }

    proxy->PauseResponses();
    auto reading = std::async(std::launch::async, [stmt] {
        StalledRead read;
        char chunk[4096];
        do {
            read.rc = SQLGetData(stmt, 1, SQL_C_CHAR, chunk, sizeof(chunk), &read.last_length);
            read.truncated_chunks += read.rc == SQL_SUCCESS_WITH_INFO ? 1 : 0;
        } while (read.rc == SQL_SUCCESS_WITH_INFO);
        return read;
    });
    const bool blocked = reading.wait_for(kStallHold) == std::future_status::timeout;
    // Always release the response before waiting on the reader.
    proxy->Resume();
    const StalledRead read = reading.get();

    // Each truncated SQL_C_CHAR chunk holds sizeof(buffer) - 1 bytes.
    const long long received =
        (1 + read.truncated_chunks) * static_cast<long long>(sizeof(buffer) - 1) +
        (read.rc == SQL_SUCCESS ? read.last_length : 0);
    if (!blocked) {
        error = "the victim's read was not blocked by the stalled response";
    } else if (read.rc != SQL_SUCCESS) {
        error = Describe("SQLGetData after the stall", read.rc, SQL_HANDLE_STMT, stmt);
    } else if (received != kValueBytes) {
        error = "the stalled value delivered " + std::to_string(received) + " of " +
                std::to_string(kValueBytes) + " bytes";
    } else if (proxy->Failed()) {
        error = "the proxy failed";
    }
    SQLFreeStmt(stmt, SQL_CLOSE);
    return error.empty() && victim.Close(error);
}

// Plan scenario 8: a peer that stops responding without closing. Healthy
// workers on the same ENV must keep their pace while the victim waits.
TEST_F(FaultRecoveryTest, StalledConnectionDoesNotDisturbOthers) {
    if (!ProxyUsable()) {
        GTEST_SKIP() << "The stall proxy requires ODBC_TEST_SERVER as a direct TCP endpoint";
    }
    RunWatched([&] {
        AlongsideHealthyWorkers((std::max)(2, (std::min)(config_.iterations, 5)),
                                [&](int, Connection&, std::string& error) {
                                    return StallVictimMidResult(error);
                                });
    });
}

// --- Plan scenario 9: timeout and cancel churn --------------------------------

enum FaultKind { kTimeout, kCancelWhenRunning, kCancelDataAtExecution, kCancelAtRandom, kFaultKinds };

const char* FaultName(int kind) {
    static const char* const names[kFaultKinds] = {"timeout", "cancel_when_running",
                                                   "cancel_data_at_execution", "cancel_at_random"};
    return names[kind];
}

// Each worker's connection and a monitor connection that watches it.
struct WorkerConnections {
    explicit WorkerConnections(SQLHENV env) : main(env), monitor(env) {}
    Connection main;
    Connection monitor;
    std::string spid;
    // How long kRowsQuery takes on |main| uncancelled; 0 until measured.
    long long rows_query_us = 0;
};

std::string StmtState(SQLHSTMT stmt) { return ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt); }

// SQL_ATTR_QUERY_TIMEOUT expires on a WAITFOR: HYT00 within a few seconds.
bool TimeOut(WorkerConnections& c, std::string& error) {
    const SQLHSTMT stmt = c.main.stmt();
    SQLRETURN rc = SQLSetStmtAttr(stmt, SQL_ATTR_QUERY_TIMEOUT, reinterpret_cast<SQLPOINTER>(1), 0);
    if (!SQL_SUCCEEDED(rc)) {
        error = Describe("SQLSetStmtAttr(QUERY_TIMEOUT)", rc, SQL_HANDLE_STMT, stmt);
        return false;
    }
    const Clock::time_point start = Clock::now();
    rc = lifecycle::ExecDirect(stmt, kWaitfor);
    const long long elapsed_ms = MillisecondsSince(start);
    const std::string state = StmtState(stmt);
    const std::string diag = DiagText(SQL_HANDLE_STMT, stmt);
    const SQLRETURN reset = SQLSetStmtAttr(stmt, SQL_ATTR_QUERY_TIMEOUT, nullptr, 0);
    if (rc != SQL_ERROR || state != "HYT00") {
        error = "timed-out WAITFOR returned " + std::to_string(rc) + ": " + diag;
    } else if (elapsed_ms > 10000) {
        error = "a 1 s query timeout took " + std::to_string(elapsed_ms) + " ms";
    } else if (!SQL_SUCCEEDED(reset)) {
        error = Describe("SQLSetStmtAttr(QUERY_TIMEOUT, 0)", reset, SQL_HANDLE_STMT, stmt);
    }
    return error.empty();
}

// Another thread cancels the WAITFOR once the monitor sees it running: HY008.
// This is the deterministic case; CancelAtRandomPoint covers the rest.
bool CancelWhenRunning(WorkerConnections& c, std::string& error) {
    const SQLHSTMT stmt = c.main.stmt();
    std::string watch_error;
    SQLRETURN cancel_rc = SQL_SUCCESS;
    std::thread canceller([&] {
        const bool seen = WaitForScalar(c.monitor.stmt(), RunningWaitforSql(c.spid), "1",
                                        std::chrono::seconds(10), watch_error);
        cancel_rc = SQLCancel(stmt);
        if (!seen && watch_error.empty()) {
            watch_error = "the WAITFOR never appeared";
        }
    });
    const Clock::time_point start = Clock::now();
    const SQLRETURN rc = lifecycle::ExecDirect(stmt, kWaitfor);
    const long long elapsed_ms = MillisecondsSince(start);
    canceller.join();
    const std::string state = StmtState(stmt);
    if (!watch_error.empty()) {
        error = "monitor: " + watch_error;
    } else if (!SQL_SUCCEEDED(cancel_rc)) {
        error = "SQLCancel returned " + std::to_string(cancel_rc);
    } else if (rc != SQL_ERROR || state != "HY008") {
        error = "cancelled WAITFOR returned " + std::to_string(rc) + ": " +
                DiagText(SQL_HANDLE_STMT, stmt);
    } else if (elapsed_ms > 20000) {
        error = "cancelled WAITFOR took " + std::to_string(elapsed_ms) + " ms";
    }
    return error.empty();
}

// SQLCancel abandons a data-at-execution parameter mid-stream.
bool CancelDataAtExecution(WorkerConnections& c, std::string& error) {
    const SQLHSTMT stmt = c.main.stmt();
    SQLLEN indicator = SQL_LEN_DATA_AT_EXEC(0);
    SQLRETURN rc = SQLBindParameter(stmt, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR, 8000, 0,
                                    reinterpret_cast<SQLPOINTER>(1), 0, &indicator);
    if (!SQL_SUCCEEDED(rc)) {
        error = Describe("SQLBindParameter", rc, SQL_HANDLE_STMT, stmt);
        return false;
    }
    SQLPOINTER token = nullptr;
    char chunk[100];
    std::fill(std::begin(chunk), std::end(chunk), 'x');
    if ((rc = lifecycle::ExecDirect(stmt, "SELECT DATALENGTH(?)")) != SQL_NEED_DATA) {
        error = Describe("SQLExecDirect (expected SQL_NEED_DATA)", rc, SQL_HANDLE_STMT, stmt);
    } else if ((rc = SQLParamData(stmt, &token)) != SQL_NEED_DATA) {
        error = Describe("SQLParamData (expected SQL_NEED_DATA)", rc, SQL_HANDLE_STMT, stmt);
    } else if (!SQL_SUCCEEDED(rc = SQLPutData(stmt, chunk, sizeof(chunk)))) {
        error = Describe("SQLPutData", rc, SQL_HANDLE_STMT, stmt);
    } else if (!SQL_SUCCEEDED(rc = SQLCancel(stmt))) {
        error = Describe("SQLCancel", rc, SQL_HANDLE_STMT, stmt);
    }
    if (!SQL_SUCCEEDED(rc = SQLFreeStmt(stmt, SQL_RESET_PARAMS)) && error.empty()) {
        error = Describe("SQLFreeStmt(RESET_PARAMS)", rc, SQL_HANDLE_STMT, stmt);
    }
    return error.empty();
}

const char* const kRowsQuery =
    "SELECT TOP (20000) CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS int) "
    "FROM sys.all_columns a CROSS JOIN sys.all_columns b";
constexpr long long kRows = 20000;

enum RandomOutcome { kCompleted, kExecuteCancelled, kFetchCancelled, kCursorClosed, kOutcomes };

class CancelOutcomes {
   public:
    void Add(int outcome) { ++counts_[outcome]; }
    void Timed(long long execute_us, long long total_us) {
        std::lock_guard<std::mutex> lock(mutex_);
        max_execute_us_ = (std::max)(max_execute_us_, execute_us);
        max_total_us_ = (std::max)(max_total_us_, total_us);
    }
    std::string Summary() const {
        static const char* const names[kOutcomes] = {"completed", "execute_cancelled",
                                                     "fetch_cancelled", "cursor_closed"};
        std::string text;
        for (int i = 0; i < kOutcomes; ++i) {
            text += std::string(i ? ", " : "") + names[i] + "=" + std::to_string(counts_[i].load());
        }
        std::lock_guard<std::mutex> lock(mutex_);
        return text + "; slowest execute " + std::to_string(max_execute_us_) +
               " us, slowest execute and fetch " + std::to_string(max_total_us_) + " us";
    }

   private:
    std::atomic<long long> counts_[kOutcomes] = {};
    mutable std::mutex mutex_;
    long long max_execute_us_ = 0;
    long long max_total_us_ = 0;
};

// Reads kRowsQuery in full and returns how long it took, or -1.
long long TimeRowsQuery(SQLHSTMT stmt, std::string& error) {
    const Clock::time_point start = Clock::now();
    SQLRETURN rc = lifecycle::ExecDirect(stmt, kRowsQuery);
    long long rows = 0;
    if (SQL_SUCCEEDED(rc)) {
        while ((rc = SQLFetch(stmt)) == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) {
            ++rows;
        }
    }
    const long long elapsed_us =
        std::chrono::duration_cast<std::chrono::microseconds>(Clock::now() - start).count();
    if (rc != SQL_NO_DATA || rows != kRows) {
        error = "uncancelled rows query returned " + std::to_string(rc) + " after " +
                std::to_string(rows) + " rows: " + DiagText(SQL_HANDLE_STMT, stmt);
        return -1;
    }
    SQLFreeStmt(stmt, SQL_CLOSE);
    return elapsed_us;
}

// Another thread cancels at a random point in 0-120% of the time the query
// takes uncancelled on this connection (measured once per connection, so the
// spread suits each driver's speed), seeded by the run index: before the
// request is sent (an idle cancel, after which the query runs in full),
// during execute, during the fetch loop, between fetches (an idle cancel that
// closes the cursor), or after the last row. Any of those is a valid outcome;
// a short read reported as SQL_NO_DATA is not.
bool CancelAtRandomPoint(WorkerConnections& c, long long run, CancelOutcomes& outcomes,
                         std::string& error) {
    const SQLHSTMT stmt = c.main.stmt();
    if (c.rows_query_us == 0 && (c.rows_query_us = TimeRowsQuery(stmt, error)) < 0) {
        return false;
    }
    std::mt19937 random(static_cast<std::mt19937::result_type>(run));
    const auto delay = std::chrono::microseconds(
        std::uniform_int_distribution<long long>(0, c.rows_query_us * 6 / 5)(random));
    StartGate gate(2);
    SQLRETURN cancel_rc = SQL_SUCCESS;
    std::thread canceller([&] {
        gate.ArriveAndWait();
        std::this_thread::sleep_for(delay);
        cancel_rc = SQLCancel(stmt);
    });
    gate.ArriveAndWait();
    const Clock::time_point start = Clock::now();
    SQLRETURN rc = lifecycle::ExecDirect(stmt, kRowsQuery);
    const Clock::time_point executed_at = Clock::now();
    const bool executed = SQL_SUCCEEDED(rc);
    long long rows = 0;
    if (executed) {
        while ((rc = SQLFetch(stmt)) == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) {
            ++rows;
        }
    }
    const Clock::time_point finished_at = Clock::now();
    canceller.join();
    outcomes.Timed(
        std::chrono::duration_cast<std::chrono::microseconds>(executed_at - start).count(),
        std::chrono::duration_cast<std::chrono::microseconds>(finished_at - start).count());
    const std::string where = " (cancel after " + std::to_string(delay.count()) + " us, " +
                              std::to_string(rows) + " rows)";
    const std::string state = rc == SQL_ERROR ? StmtState(stmt) : "";
    if (!SQL_SUCCEEDED(cancel_rc)) {
        error = "SQLCancel returned " + std::to_string(cancel_rc) + where;
    } else if (rc == SQL_NO_DATA && rows == kRows) {
        outcomes.Add(kCompleted);
    } else if (rc == SQL_ERROR && state == "HY008") {
        outcomes.Add(executed ? kFetchCancelled : kExecuteCancelled);
    } else if (rc == SQL_ERROR && executed && (state == "24000" || state == "HY010")) {
        outcomes.Add(kCursorClosed);
    } else {
        error = (executed ? "SQLFetch returned " : "SQLExecDirect returned ") +
                std::to_string(rc) + where + ": " + DiagText(SQL_HANDLE_STMT, stmt);
    }
    return error.empty();
}

// Plan scenario 9: every worker keeps one connection on the shared ENV and, on
// each iteration, faults it one of four ways, then selects its own token on
// the same STMT. Nothing may stay busy or leak into the next call. The run
// index picks the fault, so all four run at any profile size; rounds
// reconnect, and the process's counts after the last round must match those
// after the first.
TEST_F(FaultRecoveryTest, TimeoutAndCancelChurn) {
    RunWatched([&] {
        const auto timeout = std::chrono::seconds(config_.timeout_seconds);
        const int rounds = RoundsCovering(3, kFaultKinds, config_);
        Failures failures;
        CancelOutcomes outcomes;
        std::atomic<long long> ran[kFaultKinds] = {};
        long long completed = 0;
        ProcessCounts baseline;
        for (int round = 0; round < rounds && failures.Empty(); ++round) {
            std::vector<std::unique_ptr<WorkerConnections>> connections(
                static_cast<size_t>(config_.threads));
            completed += RunWorkers(
                TestLabel() + " round " + std::to_string(round), config_, failures,
                [&, round](int worker, int iteration, std::string& error) {
                    auto& slot = connections[static_cast<size_t>(worker)];
                    if (!slot) {
                        slot.reset(new WorkerConnections(env_));
                        if (!slot->main.Open(error) || !slot->monitor.Open(error) ||
                            !QueryScalar(slot->main.stmt(), "SELECT @@SPID", slot->spid, error)) {
                            return false;
                        }
                    }
                    WorkerConnections& c = *slot;
                    const long long run = RunIndex(round, config_, worker, iteration);
                    const int kind = static_cast<int>(run % kFaultKinds);
                    ++ran[kind];
                    bool faulted = false;
                    switch (kind) {
                        case kTimeout:
                            faulted = TimeOut(c, error);
                            break;
                        case kCancelWhenRunning:
                            faulted = CancelWhenRunning(c, error);
                            break;
                        case kCancelDataAtExecution:
                            faulted = CancelDataAtExecution(c, error);
                            break;
                        default:
                            faulted = CancelAtRandomPoint(c, run, outcomes, error);
                            break;
                    }
                    if (!faulted) {
                        error = std::string(FaultName(kind)) + ": " + error;
                        return false;
                    }
                    SQLFreeStmt(c.main.stmt(), SQL_CLOSE);
                    if (!CheckToken(c.main.stmt(), 1 + run, error)) {
                        error = std::string("after ") + FaultName(kind) + ": " + error;
                        return false;
                    }
                    return true;
                });
            {
                std::vector<std::atomic<int>> no_workers;
                Watchdog cleanup(TestLabel() + " round " + std::to_string(round) + " cleanup",
                                 timeout, no_workers);
                for (size_t w = 0; w < connections.size() && failures.Empty(); ++w) {
                    std::string error;
                    if (connections[w] && !(connections[w]->main.Close(error) &&
                                            connections[w]->monitor.Close(error))) {
                        failures.Add("round " + std::to_string(round) + " worker " +
                                     std::to_string(w) + " close: " + error);
                    }
                }
                connections.clear();
            }
            if (round == 0) {
                baseline = SampleProcess();
            }
        }
        const ProcessCounts after = SampleProcess();
        RecordProperty("cancel_at_random_outcomes", outcomes.Summary());
        EXPECT_TRUE(failures.Empty()) << failures.Summary();
        EXPECT_EQ(static_cast<long long>(rounds) * config_.threads * config_.iterations,
                  completed);
        if (failures.Empty()) {
            for (int kind = 0; kind < kFaultKinds; ++kind) {
                EXPECT_GT(ran[kind].load(), 0) << FaultName(kind) << " never ran";
            }
            ExpectNoGrowth(baseline, after);
        }
    });
}

}  // namespace
