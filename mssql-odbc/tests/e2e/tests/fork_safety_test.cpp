// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// fork_safety_test  –  POSIX fork() safety of an ODBC environment that is still
// live in the parent process.
//
// An HENV that has driven a connection owns a Tokio runtime and its worker
// threads. fork() duplicates only the calling thread, so in the child those
// workers do not exist while every lock they held at fork time is still
// observed as locked. A driver call made in the child on that inherited HENV
// then waits on a futex nothing will ever post, and the child deadlocks.
//
// Whether this is reachable depends on where the child's HENV comes from, which
// is why the cases below separate the two:
//
//   * A child that allocates a fresh HENV builds a new runtime and is fine.
//   * A child that connects on the HENV it inherited deadlocks.
//
// The second is the shape real callers have. mssql-python allocates one
// process-wide HENV at import and every connection hangs off it, so any
// fork()-based worker inherits it. That is not hypothetical: SQLAlchemy's
// test/aaa_profiling/test_memusage.py wraps its cases in @profile_memory, which
// runs each body in a `multiprocessing.get_context("fork")` child. Running that
// file through mssql-python against this driver hangs; msodbcsql18 completes it
// in 17s. msodbcsql18 passes every case here.
//
// Every driver call that can deadlock runs inside a forked, time-bounded
// process, and the scenario process is reaped with a deadline, so a deadlock is
// reported as a failure instead of hanging the suite.
//
// Tests that require a live SQL Server are gated by
// ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <cstring>
#include <string>

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
bool Query(SQLHDBC dbc) {
    SQLHSTMT stmt = SQL_NULL_HSTMT;
    if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &stmt))) {
        return false;
    }
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLINTEGER value = 0;
    SQLLEN ind = 0;
    const bool ok =
        SQL_SUCCEEDED(SQLExecDirect(stmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS)) &&
        SQL_SUCCEEDED(SQLFetch(stmt)) &&
        SQL_SUCCEEDED(SQLGetData(stmt, 1, SQL_C_SLONG, &value, 0, &ind)) && value == 1;
    SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    return ok;
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

void Disconnect(SQLHDBC dbc) {
    if (dbc != SQL_NULL_HDBC) {
        SQLDisconnect(dbc);
        SQLFreeHandle(SQL_HANDLE_DBC, dbc);
    }
}

// Connect on |env|, run SELECT 1, and drop the connection. The HENV is left
// alone: callers own it, and in the child it is the parent's.
bool ConnectAndQuery(SQLHENV env) {
    SQLHDBC dbc = Connect(env);
    if (dbc == SQL_NULL_HDBC) {
        return false;
    }
    const bool ok = Query(dbc);
    Disconnect(dbc);
    return ok;
}

// Reap |pid|, giving up after |timeout_sec|. On expiry the process is killed
// and reaped so no deadlocked child outlives the run, and false is returned.
bool WaitFor(pid_t pid, int timeout_sec, int* status) {
    const time_t deadline = ::time(nullptr) + timeout_sec;
    for (;;) {
        const pid_t reaped = ::waitpid(pid, status, WNOHANG);
        if (reaped == pid) {
            return true;
        }
        if (reaped < 0 && errno != EINTR) {
            break;
        }
        if (::time(nullptr) >= deadline) {
            break;
        }
        timespec pause{0, 50 * 1000 * 1000};
        ::nanosleep(&pause, nullptr);
    }
    ::kill(pid, SIGKILL);
    ::waitpid(pid, status, 0);
    return false;
}

// Hold a live HENV, fork, have the child act on it, then optionally reuse the
// parent's HENV. Runs as its own process so a deadlock anywhere in here is
// bounded by the caller.
[[noreturn]] void RunScenario(ChildAction action, bool check_parent_after) {
    // Both the HENV and a connection on it are kept open across the fork. The
    // connection is what pins the runtime: unixODBC unloads the driver once the
    // last connection on it goes away, which would join the worker threads and
    // leave the child nothing hazardous to inherit. Callers that hold a pool
    // open, as mssql-python does, are in exactly this state.
    SQLHENV env = AllocEnv();
    SQLHDBC dbc = Connect(env);
    if (dbc == SQL_NULL_HDBC || !Query(dbc)) {
        ::_exit(kWarmupFailed);
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
    if (WEXITSTATUS(status) != 0) {
        ::_exit(kChildFailed);
    }

    if (check_parent_after && (!Query(dbc) || !ConnectAndQuery(env))) {
        ::_exit(kParentAfterForkFailed);
    }
    Disconnect(dbc);
    SQLFreeHandle(SQL_HANDLE_ENV, env);
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
        ASSERT_FALSE(WIFSIGNALED(status))
            << "scenario was killed by signal " << WTERMSIG(status);

        const int code = WEXITSTATUS(status);
        EXPECT_EQ(kScenarioOk, code) << DescribeExit(code);
    }
};

// Control: forking is harmless as long as the child leaves the driver alone.
// Establishes that the fixture, the connection settings, and fork() itself are
// not the cause of the failures below.
TEST_F(ForkSafetyTest, ForkWithChildNotUsingDriverIsSafe) {
    ExpectScenarioSucceeds(ChildAction::kNothing, /*check_parent_after=*/true);
}

// Boundary: a child that builds its own HENV gets a fresh runtime and works.
// Pinning this down separates "fork() plus threads is inherently undefined"
// from the actual defect, which is specific to the inherited handle.
TEST_F(ForkSafetyTest, ChildCanConnectOnItsOwnEnvAfterFork) {
    ExpectScenarioSucceeds(ChildAction::kOwnEnv, /*check_parent_after=*/false);
}

// The reported bug: connecting on the inherited HENV never returns in the child.
TEST_F(ForkSafetyTest, ChildCanConnectOnInheritedEnvAfterFork) {
    ExpectScenarioSucceeds(ChildAction::kInheritedEnv, /*check_parent_after=*/false);
}

// The damage is not confined to the child. Once one has hung and been reaped,
// the forking process can no longer use the HENV it kept open either, which is
// what turns a single profiled test case into a hung test run. Today this stops
// at the same child deadlock as the case above; it starts covering the parent
// once that is fixed.
TEST_F(ForkSafetyTest, ParentRemainsUsableAfterChildUsesInheritedEnv) {
    ExpectScenarioSucceeds(ChildAction::kInheritedEnv, /*check_parent_after=*/true);
}

}  // namespace

#endif  // _WIN32
