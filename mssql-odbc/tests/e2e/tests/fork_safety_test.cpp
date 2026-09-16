// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Regression coverage for #566 and SQLAlchemy's fork-based memory tests.
// Fork outside application ODBC calls, while the ENV's Tokio worker is alive.
// Driver calls run in bounded subprocesses so a regression fails, not hangs.

#include "odbc_test_fixture.h"

#include <atomic>
#include <cstring>
#include <chrono>
#include <iostream>
#include <string>
#include <thread>
#include <vector>

#ifndef _WIN32
#include <csignal>
#include <cerrno>
#include <ctime>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#endif

#ifdef _WIN32

// fork() has no Windows equivalent; CreateProcess starts a fresh image and so
// never inherits the runtime state this guards.
TEST(ForkSafetyTest, SkippedOnWindows) {
    GTEST_SKIP() << "fork() is POSIX-only";
}

#else

namespace {

// The scenario's own budget has to exceed the inner child's, or a child
// deadlock would surface as the less specific "scenario timed out".
constexpr unsigned kChildBudgetSec = 20;
constexpr int kScenarioBudgetSec = 60;

// What the forked child does with the driver.
enum class ChildAction {
    kNothing,        // control: never calls the driver
    kOwnEnv,         // allocates a fresh HENV of its own
    kInheritedEnv,   // connects on the HENV inherited from the parent
    kCleanup,
    kDeadConnection,
    kConcurrentConnections,
    kSecondFork,
    kDataAtExecution,
    kOpenCursor,
    kTransaction,
};

// Reported by the scenario process through its exit status.
enum ScenarioExit : int {
    kScenarioOk = 0,
    kWarmupFailed = 11,
    kForkFailed = 12,
    kParentAfterForkFailed = 13,
    kChildDeadlocked = 30,
    kChildFailed = 31,
};

// Allocate an HENV and set the ODBC version. Returns SQL_NULL_HENV on failure.
// Deliberately free of gtest assertions: this runs in forked children, where a
// failed assertion would be reported against a process gtest is not tracking.
SQLHENV AllocEnv() {
    SQLHENV env = SQL_NULL_HENV;
    if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env))) {
        return SQL_NULL_HENV;
    }
    if (!SQL_SUCCEEDED(SQLSetEnvAttr(env, SQL_ATTR_ODBC_VERSION,
                                     reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0))) {
        SQLFreeHandle(SQL_HANDLE_ENV, env);
        return SQL_NULL_HENV;
    }
    return env;
}

// Run SELECT 1 on an already-connected HDBC.
bool Query(SQLHDBC dbc, const std::string& text = "SELECT 1") {
    SQLHSTMT stmt = SQL_NULL_HSTMT;
    if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &stmt))) {
        return false;
    }
    SqlTString sql = ODBCTestUtils::ToSqlTStr(text);
    SQLINTEGER value = 0;
    SQLLEN ind = 0;
    const bool ok =
        SQL_SUCCEEDED(SQLExecDirect(stmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS)) &&
        SQL_SUCCEEDED(SQLFetch(stmt)) &&
        SQL_SUCCEEDED(SQLGetData(stmt, 1, SQL_C_SLONG, &value, 0, &ind)) && value == 1;
    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    return ok;
}

bool InheritedConnectionIsDead(SQLHDBC dbc) {
    SQLUINTEGER dead = SQL_CD_FALSE;
    if (!SQL_SUCCEEDED(SQLGetConnectAttr(dbc, SQL_ATTR_CONNECTION_DEAD, &dead,
                                        sizeof(dead), nullptr)) || dead != SQL_CD_TRUE) {
        return false;
    }
    SQLHSTMT stmt = SQL_NULL_HSTMT;
    if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &stmt))) {
        return false;
    }
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    const SQLRETURN rc = SQLExecDirect(stmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    const bool rejected = rc == SQL_ERROR &&
        ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt) == "08003";
    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    return rejected;
}

// Allocate an HDBC on |env| and connect it. Returns SQL_NULL_HDBC on failure.
SQLHDBC Connect(SQLHENV env) {
    if (env == SQL_NULL_HENV) {
        return SQL_NULL_HDBC;
    }
    SQLHDBC dbc = SQL_NULL_HDBC;
    if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_DBC, env, &dbc))) {
        return SQL_NULL_HDBC;
    }
    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR out[1024] = {};
    SQLSMALLINT out_len = 0;
    if (!SQL_SUCCEEDED(SQLDriverConnect(
            dbc, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
            static_cast<SQLSMALLINT>(connstr.size()), out,
            static_cast<SQLSMALLINT>(sizeof(out) / sizeof(SQLTCHAR)), &out_len,
            SQL_DRIVER_NOPROMPT))) {
        SQLFreeHandle(SQL_HANDLE_DBC, dbc);
        return SQL_NULL_HDBC;
    }
    return dbc;
}

bool Disconnect(SQLHDBC dbc) {
    if (dbc != SQL_NULL_HDBC) {
        if (!SQL_SUCCEEDED(SQLDisconnect(dbc))) return false;
        return SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_DBC, dbc));
    }
    return true;
}

// Connect on |env|, run SELECT 1, and drop the connection. The HENV is left
// alone: callers own it, and in the child it is the parent's.
bool ConnectAndQuery(SQLHENV env) {
    SQLHDBC dbc = Connect(env);
    if (dbc == SQL_NULL_HDBC) {
        return false;
    }
    const bool ok = Query(dbc);
    return Disconnect(dbc) && ok;
}

// Reap |pid|, giving up after |timeout_sec|. On expiry the process is killed
// and reaped so no deadlocked child outlives the run, and false is returned.
bool WaitFor(pid_t pid, int timeout_sec, int* status) {
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(timeout_sec);
    for (;;) {
        const pid_t reaped = ::waitpid(pid, status, WNOHANG);
        if (reaped == pid) {
            return true;
        }
        if (reaped < 0 && errno != EINTR) {
            break;
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            break;
        }
        timespec pause{0, 50 * 1000 * 1000};
        ::nanosleep(&pause, nullptr);
    }
    ::kill(pid, SIGKILL);
    while (::waitpid(pid, status, 0) < 0 && errno == EINTR) {}
    return false;
}

// Hold a live HENV, fork, have the child act on it, then optionally reuse the
// parent's HENV. Runs as its own process so a deadlock anywhere in here is
// bounded by the caller.
[[noreturn]] void RunScenario(ChildAction action, bool check_parent_after) {
    // Keep a DBC open so unixODBC retains the driver's ENV/runtime at fork.
    SQLHENV env = AllocEnv();
    SQLHDBC dbc = Connect(env);
    if (dbc == SQL_NULL_HDBC || !Query(dbc)) {
        ::_exit(kWarmupFailed);
    }
    if (action == ChildAction::kTransaction &&
        (!SQL_SUCCEEDED(SQLSetConnectAttr(dbc, SQL_ATTR_AUTOCOMMIT,
            reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_OFF), 0)) ||
         !Query(dbc, "SET NOCOUNT ON; CREATE TABLE #fork_preserved(v int); "
                     "INSERT INTO #fork_preserved VALUES (1); SELECT 1"))) {
        ::_exit(kWarmupFailed);
    }
    SQLHSTMT dae_stmt = SQL_NULL_HSTMT;
    SQLHSTMT open_stmt = SQL_NULL_HSTMT;
    SQLLEN dae_length = SQL_DATA_AT_EXEC;
    char dae_token = 0;
    if (action == ChildAction::kDataAtExecution) {
        SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT ?");
        SQLPOINTER token = nullptr;
        std::vector<char> chunk(20000, 'x');
        if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &dae_stmt)) ||
            !SQL_SUCCEEDED(SQLPrepare(dae_stmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS)) ||
            !SQL_SUCCEEDED(SQLBindParameter(dae_stmt, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                SQL_VARCHAR, 0, 0, &dae_token, 0, &dae_length)) ||
            SQLExecute(dae_stmt) != SQL_NEED_DATA ||
            SQLParamData(dae_stmt, &token) != SQL_NEED_DATA ||
            !SQL_SUCCEEDED(SQLPutData(dae_stmt, chunk.data(), chunk.size()))) {
            ::_exit(kWarmupFailed);
        }
    }
    if (action == ChildAction::kOpenCursor) {
        SqlTString sql = ODBCTestUtils::ToSqlTStr(
            "SELECT TOP (2000) 1 FROM sys.all_objects a CROSS JOIN sys.all_objects b");
        if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &open_stmt)) ||
            !SQL_SUCCEEDED(SQLExecDirect(open_stmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS)) ||
            !SQL_SUCCEEDED(SQLFetch(open_stmt))) {
            ::_exit(kWarmupFailed);
        }
    }

    const pid_t child = ::fork();
    if (child < 0) {
        ::_exit(kForkFailed);
    }
    if (child == 0) {
        // SIGALRM converts a deadlock into a signalled exit the parent can name.
        ::alarm(kChildBudgetSec);
        switch (action) {
            case ChildAction::kNothing:
                ::_exit(0);
            case ChildAction::kOwnEnv:
                ::_exit(ConnectAndQuery(AllocEnv()) ? 0 : 1);
            case ChildAction::kInheritedEnv:
                ::_exit(ConnectAndQuery(env) ? 0 : 1);
            case ChildAction::kTransaction:
            case ChildAction::kCleanup: {
                const bool ok = ConnectAndQuery(env) && Disconnect(dbc) &&
                    SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_ENV, env));
                ::_exit(ok ? 0 : 1);
            }
            case ChildAction::kDataAtExecution:
                ::_exit(ConnectAndQuery(env) &&
                    SQL_SUCCEEDED(SQLCancel(dae_stmt)) &&
                    SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_STMT, dae_stmt)) &&
                    Disconnect(dbc) &&
                    SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_ENV, env)) ? 0 : 1);
            case ChildAction::kOpenCursor:
                ::_exit(SQL_SUCCEEDED(SQLFreeStmt(open_stmt, SQL_CLOSE)) &&
                    SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_STMT, open_stmt)) &&
                    Disconnect(dbc) && ConnectAndQuery(env) &&
                    SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_ENV, env)) ? 0 : 1);
            case ChildAction::kDeadConnection:
                ::_exit(InheritedConnectionIsDead(dbc) && Disconnect(dbc) &&
                    ConnectAndQuery(env) &&
                    SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_ENV, env)) ? 0 : 1);
            case ChildAction::kConcurrentConnections: {
                // All threads enter the driver for the first time after fork.
                std::vector<std::thread> threads;
                std::atomic<int> ready{0};
                std::atomic<bool> start{false};
                int results[4] = {};
                for (int i = 0; i < 4; ++i) {
                    threads.emplace_back([&, i] {
                        ++ready;
                        while (!start.load()) std::this_thread::yield();
                        results[i] = ConnectAndQuery(env) ? 1 : 0;
                    });
                }
                while (ready.load() != 4) std::this_thread::yield();
                start.store(true);
                for (auto& thread : threads) thread.join();
                for (int result : results) {
                    if (result != 1) ::_exit(1);
                }
                ::_exit(0);
            }
            case ChildAction::kSecondFork: {
                SQLHDBC child_dbc = Connect(env);
                if (child_dbc == SQL_NULL_HDBC || !Query(child_dbc)) ::_exit(1);
                const pid_t grandchild = ::fork();
                if (grandchild < 0) ::_exit(1);
                if (grandchild == 0) {
                    ::alarm(kChildBudgetSec / 2);
                    ::_exit(ConnectAndQuery(env) ? 0 : 1);
                }
                int nested_status = 0;
                const bool ok = WaitFor(grandchild, kChildBudgetSec / 2 + 2, &nested_status) &&
                    WIFEXITED(nested_status) && WEXITSTATUS(nested_status) == 0 &&
                    Query(child_dbc) && Disconnect(child_dbc);
                ::_exit(ok ? 0 : 1);
            }
        }
        ::_exit(1);
    }

    int status = 0;
    if (!WaitFor(child, static_cast<int>(kChildBudgetSec) + 10, &status)) {
        ::_exit(kChildDeadlocked);
    }
    if (WIFSIGNALED(status)) {
        ::_exit(WTERMSIG(status) == SIGALRM ? kChildDeadlocked : kChildFailed);
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        ::_exit(kChildFailed);
    }
    if (dae_stmt != SQL_NULL_HSTMT &&
        (!SQL_SUCCEEDED(SQLCancel(dae_stmt)) ||
         !SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_STMT, dae_stmt)))) {
        ::_exit(kParentAfterForkFailed);
    }
    if (open_stmt != SQL_NULL_HSTMT &&
        (!SQL_SUCCEEDED(SQLFetch(open_stmt)) ||
         !SQL_SUCCEEDED(SQLFreeStmt(open_stmt, SQL_CLOSE)) ||
         !SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_STMT, open_stmt)))) {
        ::_exit(kParentAfterForkFailed);
    }

    if (check_parent_after && (!Query(dbc) || !ConnectAndQuery(env))) {
        ::_exit(kParentAfterForkFailed);
    }
    if (action == ChildAction::kTransaction &&
        (!Query(dbc, "SELECT COUNT(*) FROM #fork_preserved") ||
         !SQL_SUCCEEDED(SQLEndTran(SQL_HANDLE_DBC, dbc, SQL_ROLLBACK)))) {
        ::_exit(kParentAfterForkFailed);
    }
    if (!Disconnect(dbc) || !SQL_SUCCEEDED(SQLFreeHandle(SQL_HANDLE_ENV, env))) {
        ::_exit(kParentAfterForkFailed);
    }
    ::_exit(kScenarioOk);
}

std::string DescribeExit(int code) {
    switch (code) {
        case kScenarioOk:            return "ok";
        case kWarmupFailed:          return "the parent's own pre-fork query failed";
        case kForkFailed:            return "fork() failed";
        case kParentAfterForkFailed: return "the parent could not reuse its own HENV after the child exited";
        case kChildDeadlocked:       return "the child deadlocked inside the driver after fork()";
        case kChildFailed:           return "the child's query failed";
        default:                     return "unrecognized exit code";
    }
}

class ForkSafetyTest : public ::testing::Test {
protected:
    void SetUp() override {
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
    }

    // Supervise one scenario. The test binary stays free of driver calls so a
    // deadlock is always reportable.
    void ExpectScenarioSucceeds(ChildAction action, bool check_parent_after) {
        const pid_t pid = ::fork();
        ASSERT_GE(pid, 0) << "fork() failed: " << std::strerror(errno);
        if (pid == 0) {
            RunScenario(action, check_parent_after);
        }

        int status = 0;
        const bool reaped = WaitFor(pid, kScenarioBudgetSec, &status);
        ASSERT_TRUE(reaped) << "scenario did not finish within " << kScenarioBudgetSec
                            << "s; the driver is deadlocked";
        ASSERT_TRUE(WIFEXITED(status)) << "scenario did not exit normally: " << status;

        const int code = WEXITSTATUS(status);
        EXPECT_EQ(kScenarioOk, code) << DescribeExit(code);
    }
};

TEST_F(ForkSafetyTest, ForkWithChildNotUsingDriverIsSafe) {
    ExpectScenarioSucceeds(ChildAction::kNothing, /*check_parent_after=*/true);
}

TEST_F(ForkSafetyTest, ChildCanConnectOnItsOwnEnvAfterFork) {
    ExpectScenarioSucceeds(ChildAction::kOwnEnv, /*check_parent_after=*/false);
}

TEST_F(ForkSafetyTest, ChildCanConnectOnInheritedEnvAfterFork) {
    ExpectScenarioSucceeds(ChildAction::kInheritedEnv, /*check_parent_after=*/false);
}

TEST_F(ForkSafetyTest, ParentRemainsUsableAfterChildUsesInheritedEnv) {
    ExpectScenarioSucceeds(ChildAction::kInheritedEnv, /*check_parent_after=*/true);
}

TEST_F(ForkSafetyTest, ChildCanFreeInheritedHandlesWithoutAffectingParent) {
    // msodbcsql 18.05.0001: the parent's next query receives SIGPIPE.
    SKIP_IF_COMPARING_MSODBCSQL();
    ExpectScenarioSucceeds(ChildAction::kCleanup, true);
}

TEST_F(ForkSafetyTest, InheritedConnectionIsDeadAndRejectsQueries) {
    // msodbcsql 18.05.0001 does not invalidate the inherited connection.
    SKIP_IF_COMPARING_MSODBCSQL();
    ExpectScenarioSucceeds(ChildAction::kDeadConnection, true);
}

TEST_F(ForkSafetyTest, ConcurrentFirstChildCallsShareRecoveredEnvironment) {
    ExpectScenarioSucceeds(ChildAction::kConcurrentConnections, true);
}

TEST_F(ForkSafetyTest, RecoveryWorksAcrossTwoForkGenerations) {
    ExpectScenarioSucceeds(ChildAction::kSecondFork, true);
}

TEST_F(ForkSafetyTest, ChildDiscardsInheritedDataAtExecutionWithoutAffectingParent) {
    // msodbcsql 18.05.0001: child cleanup leaves the parent's query failing.
    SKIP_IF_COMPARING_MSODBCSQL();
    ExpectScenarioSucceeds(ChildAction::kDataAtExecution, true);
}

TEST_F(ForkSafetyTest, ChildClosesInheritedCursorLocally) {
    // msodbcsql 18.05.0001: the parent's fetch/query fails after child close.
    SKIP_IF_COMPARING_MSODBCSQL();
    ExpectScenarioSucceeds(ChildAction::kOpenCursor, true);
}

TEST_F(ForkSafetyTest, ChildCleanupPreservesParentTransaction) {
    // msodbcsql 18.05.0001 fails the child-side inherited-transaction cleanup.
    SKIP_IF_COMPARING_MSODBCSQL();
    ExpectScenarioSucceeds(ChildAction::kTransaction, true);
}

}  // namespace

#endif  // _WIN32
