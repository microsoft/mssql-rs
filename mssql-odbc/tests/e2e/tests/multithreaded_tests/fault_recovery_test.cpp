// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Faults under load (plan scenarios 8 and 9): one connection is killed, timed
// out, or cancelled while other connections on the same ENV keep working.
// Every ENV of ours runs all its connections on one Tokio worker thread, so a
// connection that blocks or fails badly can stall or corrupt the others.
// Sized and watched like handle_lifecycle_test.cpp; the ctest entry is
// fault_recovery_test_mt, labelled mt_weekly, so only the weekly profile runs
// it. Needs VIEW SERVER STATE and ALTER ANY CONNECTION (KILL), which the CI
// login (sa) and a local administrator have.

#include "lifecycle_support.h"

#include <future>
#include <iterator>
#include <memory>

namespace {

using namespace lifecycle;

const char* const kWaitfor = "WAITFOR DELAY '00:00:30'";

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

class FaultRecoveryTest : public LifecycleTest {
   protected:
    bool KillIdleVictim(Connection& admin, long long token, std::string& error);
    bool KillExecutingVictim(Connection& admin, std::string& error);
};

bool Kill(Connection& admin, const std::string& spid, std::string& error) {
    const SQLRETURN rc = ExecDirect(admin.stmt(), "KILL " + spid);
    SQLFreeStmt(admin.stmt(), SQL_CLOSE);
    if (!SQL_SUCCEEDED(rc)) {
        error = Describe("KILL", rc, SQL_HANDLE_STMT, admin.stmt());
        return false;
    }
    return true;
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

// Plan scenario 8: THREADS - 1 workers query their own tokens nonstop on the
// shared ENV while a victim connection on the same ENV is killed again and
// again, alternately idle and mid-execute. Healthy workers must never fail
// and never stall longer than kMaxStallMs; the victim must fail or recover
// cleanly each time.
TEST_F(FaultRecoveryTest, KilledConnectionDoesNotDisturbOthers) {
    constexpr long long kMaxStallMs = 10000;
    RunWatched([&] {
        const int healthy = (std::max)(1, config_.threads - 1);
        const int cycles = (std::max)(2, config_.iterations);
        Failures failures;
        std::atomic<bool> stop{false};
        std::vector<std::atomic<int>> progress(static_cast<size_t>(healthy));
        std::vector<long long> slowest_ms(static_cast<size_t>(healthy), 0);
        StartGate gate(healthy + 1);
        std::vector<std::thread> workers;
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
                    slowest_ms[static_cast<size_t>(w)] = (std::max)(
                        slowest_ms[static_cast<size_t>(w)],
                        static_cast<long long>(
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

        Connection admin(env_);
        std::string error;
        const bool admin_opened = admin.Open(error);
        gate.ArriveAndWait();
        if (!admin_opened) {
            failures.Add("admin connection: " + error);
        }
        int completed = 0;
        for (int cycle = 0; cycle < cycles && failures.Empty(); ++cycle) {
            Watchdog watchdog(TestLabel() + " cycle " + std::to_string(cycle),
                              std::chrono::seconds(config_.timeout_seconds), progress);
            const bool executing = cycle % 2 == 1;
            if (!(executing ? KillExecutingVictim(admin, error)
                            : KillIdleVictim(admin, -1 - cycle, error))) {
                failures.Add("cycle " + std::to_string(cycle) +
                             (executing ? " (executing victim): " : " (idle victim): ") + error);
                break;
            }
            ++completed;
        }
        stop = true;
        {
            Watchdog watchdog(TestLabel() + " stopping workers",
                              std::chrono::seconds(config_.timeout_seconds), progress);
            for (auto& worker : workers) {
                worker.join();
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
        EXPECT_LE(slowest, kMaxStallMs) << "a healthy connection stalled while another was killed";
    });
}

// --- Plan scenario 9: timeout and cancel churn --------------------------------

constexpr int kCancelRounds = 3;

// Each worker's connection and a monitor connection that watches it.
struct WorkerConnections {
    explicit WorkerConnections(SQLHENV env) : main(env), monitor(env) {}
    Connection main;
    Connection monitor;
    std::string spid;
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
    rc = ExecDirect(stmt, kWaitfor);
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
// Waiting for the server to show the request keeps the cancel from landing on
// an idle statement, which would close nothing and clear diagnostics instead.
bool CancelFromAnotherThread(WorkerConnections& c, std::string& error) {
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
    const SQLRETURN rc = ExecDirect(stmt, kWaitfor);
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
    if ((rc = ExecDirect(stmt, "SELECT DATALENGTH(?)")) != SQL_NEED_DATA) {
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

// Plan scenario 9: every worker keeps one connection on the shared ENV and, on
// each iteration, times out a query, has another thread cancel one, or cancels
// a data-at-execution stream, then selects its own token on the same STMT.
// Nothing may stay busy or leak into the next call. Rounds reconnect, and the
// process's counts after the last round must match those after the first.
TEST_F(FaultRecoveryTest, TimeoutAndCancelChurn) {
    RunWatched([&] {
        Failures failures;
        long long completed = 0;
        ProcessCounts baseline;
        for (int round = 0; round < kCancelRounds && failures.Empty(); ++round) {
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
                    bool faulted = false;
                    switch ((worker + iteration) % 3) {
                        case 0:
                            faulted = TimeOut(c, error);
                            break;
                        case 1:
                            faulted = CancelFromAnotherThread(c, error);
                            break;
                        default:
                            faulted = CancelDataAtExecution(c, error);
                            break;
                    }
                    if (!faulted) {
                        return false;
                    }
                    SQLFreeStmt(c.main.stmt(), SQL_CLOSE);
                    const long long token =
                        1 + (static_cast<long long>(round) * config_.threads + worker) *
                                config_.iterations +
                        iteration;
                    if (!CheckToken(c.main.stmt(), token, error)) {
                        error = "after the fault: " + error;
                        return false;
                    }
                    return true;
                });
            for (size_t w = 0; w < connections.size() && failures.Empty(); ++w) {
                std::string error;
                if (connections[w] &&
                    !(connections[w]->main.Close(error) && connections[w]->monitor.Close(error))) {
                    failures.Add("round " + std::to_string(round) + " worker " +
                                 std::to_string(w) + " close: " + error);
                }
            }
            connections.clear();
            if (round == 0) {
                baseline = SampleProcess();
            }
        }
        const ProcessCounts after = SampleProcess();
        EXPECT_TRUE(failures.Empty()) << failures.Summary();
        EXPECT_EQ(static_cast<long long>(kCancelRounds) * config_.threads * config_.iterations,
                  completed);
        if (failures.Empty()) {
            ExpectNoGrowth(baseline, after);
        }
    });
}

}  // namespace
