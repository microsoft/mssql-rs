// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Handle churn (plan scenarios 7 and 10, AB#49261): threads repeatedly
// allocate, connect, query, disconnect, and free whole handle trees, some of
// them through logins that fail, then check that the process released what
// those trees used. Sized by MSSQL_ODBC_MT_THREADS and MSSQL_ODBC_MT_ITERS and
// bounded by the MSSQL_ODBC_MT_TIMEOUT_SECONDS watchdog, like the ODBC_TEST_F
// concurrent variants (odbc_concurrent_test.h). The ctest entry is
// handle_lifecycle_test_mt, labelled mt_weekly, so only the weekly profile
// runs it.
//
// Out of scope until handle lifetimes are reference-counted or generation-
// checked: freeing or disconnecting a handle while another thread uses it.

#include "lifecycle_support.h"

#include <functional>

namespace {

using namespace lifecycle;

// An ENV this tree owns (each of our ENVs owns a Tokio runtime).
class Env {
   public:
    Env() = default;
    ~Env() {
        if (env_ != SQL_NULL_HENV) {
            SQLFreeHandle(SQL_HANDLE_ENV, env_);
        }
    }
    Env(const Env&) = delete;
    Env& operator=(const Env&) = delete;

    bool Alloc(std::string& error) {
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env_);
        if (!SQL_SUCCEEDED(rc)) {
            error = "SQLAllocHandle(ENV) returned " + std::to_string(rc);
            env_ = SQL_NULL_HENV;
            return false;
        }
        if (!SQL_SUCCEEDED(rc = SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                                              reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0))) {
            error = Describe("SQLSetEnvAttr(ODBC_VERSION)", rc, SQL_HANDLE_ENV, env_);
            return false;
        }
        return true;
    }

    bool Free(std::string& error) {
        const SQLRETURN rc = SQLFreeHandle(SQL_HANDLE_ENV, env_);
        if (!SQL_SUCCEEDED(rc)) {
            error = Describe("SQLFreeHandle(ENV)", rc, SQL_HANDLE_ENV, env_);
            return false;
        }
        env_ = SQL_NULL_HENV;
        return true;
    }

    SQLHENV get() const { return env_; }

   private:
    SQLHENV env_ = SQL_NULL_HENV;
};

// One tree: an ENV of its own (|shared_env| null) or the shared one, a DBC
// that connects, a STMT that selects |token|, then everything freed, each step
// checked.
bool RunTree(SQLHENV shared_env, long long token, std::string& error) {
    Env own;
    if (shared_env == SQL_NULL_HENV && !own.Alloc(error)) {
        return false;
    }
    {
        Connection connection(shared_env != SQL_NULL_HENV ? shared_env : own.get());
        if (!connection.Open(error) || !CheckToken(connection.stmt(), token, error) ||
            !connection.Close(error)) {
            return false;
        }
    }
    return shared_env != SQL_NULL_HENV || own.Free(error);
}

// Churn is cheap (16 x 50 trees take under a second locally) and its races
// need volume, so each test runs rounds of the configured size, each under its
// own watchdog so the hang timeout still bounds a normal-sized phase. Counts
// sampled after the first round must be back after the last.
constexpr int kChurnRounds = 20;

class HandleLifecycleTest : public LifecycleTest {
   protected:
    void Churn(bool shared_env);
};

// Allocates a STMT on |dbc|, sets an attribute, and frees it, until
// keep_going() is false. Through a Driver Manager a DBC or ENV allocation only
// reaches the driver at connect (the Driver Manager does not know the driver
// before then), but a STMT on a connected DBC is the driver's own, so this
// reaches the driver's handle registry in microseconds rather than a login's
// milliseconds.
bool SpinStatements(SQLHDBC dbc, const std::function<bool()>& keep_going,
                    std::atomic<long long>& spins, std::string& error) {
    while (keep_going()) {
        SQLHSTMT stmt = SQL_NULL_HSTMT;
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc, &stmt);
        if (!SQL_SUCCEEDED(rc)) {
            error = Describe("SQLAllocHandle(STMT)", rc, SQL_HANDLE_DBC, dbc);
            return false;
        }
        rc = SQLSetStmtAttr(stmt, SQL_ATTR_QUERY_TIMEOUT, reinterpret_cast<SQLPOINTER>(5), 0);
        if (!SQL_SUCCEEDED(rc)) {
            error = Describe("SQLSetStmtAttr(QUERY_TIMEOUT)", rc, SQL_HANDLE_STMT, stmt);
        }
        const SQLRETURN freed = SQLFreeHandle(SQL_HANDLE_STMT, stmt);
        if (!SQL_SUCCEEDED(freed) && error.empty()) {
            error = Describe("SQLFreeHandle(STMT)", freed, SQL_HANDLE_STMT, stmt);
        }
        if (!error.empty()) {
            return false;
        }
        ++spins;
    }
    return true;
}

// Each worker builds and tears down a whole tree per iteration, either under
// its own ENV or under the fixture's shared ENV. On the shared ENV, a quarter
// as many spinner threads hold a connection of their own and churn STMTs on it
// throughout, so cheap handle registration overlaps the workers' DBC
// allocation, connects, and frees instead of only their logins overlapping.
void HandleLifecycleTest::Churn(bool shared_env) {
    const SQLHENV env = shared_env ? env_ : SQL_NULL_HENV;
    const int spinners = shared_env ? (std::max)(1, config_.threads / 4) : 0;
    Failures failures;
    std::atomic<long long> spins{0};
    long long completed = 0;
    ProcessCounts baseline;
    for (int round = 0; round < kChurnRounds && failures.Empty(); ++round) {
        completed += RunWorkers(
            TestLabel() + " round " + std::to_string(round), config_, failures,
            [&, round](int worker, int iteration, std::string& error) {
                return RunTree(env, 1 + RunIndex(round, config_, worker, iteration), error);
            },
            spinners,
            [&](int, const std::function<bool()>& keep_going, std::string& error) {
                Connection connection(env_);
                return connection.Open(error) &&
                       SpinStatements(connection.dbc(), keep_going, spins, error) &&
                       connection.Close(error);
            });
        if (round == 0) {
            baseline = SampleProcess();
        }
    }
    const ProcessCounts after = SampleProcess();
    EXPECT_TRUE(failures.Empty()) << failures.Summary();
    EXPECT_EQ(static_cast<long long>(kChurnRounds) * config_.threads * config_.iterations,
              completed);
    if (spinners > 0) {
        RecordProperty("statement_spins", std::to_string(spins.load()));
        EXPECT_GT(spins.load(), 0) << "the spinners never overlapped the churn";
    }
    if (failures.Empty()) {
        ExpectNoGrowth(baseline, after);
    }
}

// Plan scenario 7: every thread repeatedly allocates its own ENV, DBC, and
// STMT, connects, queries, disconnects, and frees them, so ENV creation and
// teardown race other trees' I/O.
TEST_F(HandleLifecycleTest, ChurnPrivateEnvTrees) {
    RunWatched([&] { Churn(false); });
}

// The same under one ENV: DBC allocation and freeing race on the ENV's shared
// child list and runtime while other DBCs connect and query, and the spinners'
// STMT churn races both on the driver's handle registry.
TEST_F(HandleLifecycleTest, ChurnDbcTreesOnSharedEnv) {
    RunWatched([&] { Churn(true); });
}

// --- Plan scenario 10: failed-connect churn ----------------------------------

enum ConnectKind { kConnects, kUnknownLogin, kUnknownDatabase, kRefused, kNoAnswer, kKinds };

const char* KindName(int kind) {
    static const char* const names[kKinds] = {"connects", "unknown_login", "unknown_database",
                                              "refused", "no_answer"};
    return names[kind];
}

// A SQLSTATE that must appear among each failing login's records; drivers
// order records differently (msodbcsql puts 28000 before an unknown
// database's 42000).
//
// A refused port: on Windows a refused TCP connect is retried for about 2 s
// (see each_extra_connect_has_its_own_connect_timeout in mssql-tds
// network_transport.rs), longer than this test's 1 s login timeout, so both
// drivers end at the login timeout. mssql-odbc reports only HYT00; msodbcsql
// reports 08001 ("TCP Provider: The wait operation timed out") first, then
// HYT00, then another 08001 - measured on msodbcsql 18 here, and the same for
// the silent server. Elsewhere a refused connect fails at once (08001). An
// application reading only the first record sees different SQLSTATEs; that is
// undecided, not a deliberate deviation, so it is recorded here and in the plan
// rather than in parity-deviations.md.
std::vector<std::string> ExpectedStates(int kind) {
    switch (kind) {
        case kUnknownLogin:
            return {"28000"};
        case kUnknownDatabase:
            return {"42000"};
        case kRefused:
            return {"08001", "HYT00"};
        case kNoAnswer:
            return {"HYT00"};
        default:
            return {};
    }
}

bool HasExpectedState(int kind, SQLHDBC dbc) {
    for (const auto& state : ExpectedStates(kind)) {
        if (ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, dbc, state)) {
            return true;
        }
    }
    return false;
}

// Short, for volume, where only the timeout can end the login (a refused port
// is retried until it on Windows). Logins the server rejects keep a normal
// timeout: 16 of them at once on a loaded CI agent missed 1 s and reported
// HYT00 instead of 42000 (stress build 188696).
SQLULEN LoginTimeoutSeconds(int kind) {
    return kind == kRefused || kind == kNoAnswer ? 1 : 15;
}
// A failed login must return within its login timeout plus this.
constexpr long long kFailureSlackMs = 10000;

class ConnectCases {
   public:
    void Ran(int kind, bool shared_env) {
        std::lock_guard<std::mutex> lock(mutex_);
        ++ran_[kind][shared_env ? 1 : 0];
    }

    void Failed(int kind, const std::string& diagnostic, long long elapsed_ms) {
        std::lock_guard<std::mutex> lock(mutex_);
        if (first_diagnostic_[kind].empty()) {
            first_diagnostic_[kind] = diagnostic;
        }
        slowest_ms_[kind] = (std::max)(slowest_ms_[kind], elapsed_ms);
    }

    // Records what each kind did, and fails if a kind/ENV case never ran.
    void Check() const {
        std::lock_guard<std::mutex> lock(mutex_);
        for (int kind = 0; kind < kKinds; ++kind) {
            ::testing::Test::RecordProperty(
                KindName(kind), std::to_string(ran_[kind][0]) + " private-ENV, " +
                                    std::to_string(ran_[kind][1]) + " shared-ENV; slowest " +
                                    std::to_string(slowest_ms_[kind]) + " ms; first: " +
                                    first_diagnostic_[kind]);
            EXPECT_GT(ran_[kind][0], 0) << KindName(kind) << " never ran on a private ENV";
            EXPECT_GT(ran_[kind][1], 0) << KindName(kind) << " never ran on the shared ENV";
        }
    }

   private:
    mutable std::mutex mutex_;
    long long ran_[kKinds][2] = {};
    long long slowest_ms_[kKinds] = {};
    std::string first_diagnostic_[kKinds];
};

// Connects with |connstr|, which must fail with |kind|'s SQLSTATE within the
// login timeout plus slack, then frees the DBC (and the tree's own ENV) with
// each step checked.
bool FailConnect(SQLHENV shared_env, int kind, const std::string& connstr, ConnectCases& cases,
                 std::string& error) {
    Env own;
    if (shared_env == SQL_NULL_HENV && !own.Alloc(error)) {
        return false;
    }
    const SQLHENV env = shared_env != SQL_NULL_HENV ? shared_env : own.get();
    SQLHDBC dbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env, &dbc);
    if (!SQL_SUCCEEDED(rc)) {
        error = Describe("SQLAllocHandle(DBC)", rc, SQL_HANDLE_ENV, env);
        return false;
    }
    const SQLULEN timeout_seconds = LoginTimeoutSeconds(kind);
    const long long timeout_ms = static_cast<long long>(timeout_seconds) * 1000;
    if (!SQL_SUCCEEDED(rc = SQLSetConnectAttr(dbc, SQL_ATTR_LOGIN_TIMEOUT,
                                              reinterpret_cast<SQLPOINTER>(timeout_seconds), 0))) {
        error = Describe("SQLSetConnectAttr(LOGIN_TIMEOUT)", rc, SQL_HANDLE_DBC, dbc);
    } else {
        const Clock::time_point start = Clock::now();
        rc = DriverConnect(dbc, connstr);
        const long long elapsed_ms = MillisecondsSince(start);
        if (SQL_SUCCEEDED(rc)) {
            error = std::string(KindName(kind)) + " login connected";
            SQLDisconnect(dbc);
        } else {
            cases.Failed(kind, DiagText(SQL_HANDLE_DBC, dbc), elapsed_ms);
            if (rc != SQL_ERROR || !HasExpectedState(kind, dbc)) {
                error = std::string(KindName(kind)) + " login expected SQL_ERROR with [" +
                        ExpectedStates(kind)[0] + "]; " +
                        Describe("SQLDriverConnect", rc, SQL_HANDLE_DBC, dbc);
            } else if (elapsed_ms > timeout_ms + kFailureSlackMs) {
                error = std::string(KindName(kind)) + " login took " +
                        std::to_string(elapsed_ms) + " ms";
            } else if (kind == kNoAnswer && elapsed_ms < timeout_ms - 500) {
                error = "no_answer login gave up after " + std::to_string(elapsed_ms) +
                        " ms, before its login timeout";
            }
        }
    }
    if (!SQL_SUCCEEDED(rc = SQLFreeHandle(SQL_HANDLE_DBC, dbc)) && error.empty()) {
        error = Describe("SQLFreeHandle(DBC)", rc, SQL_HANDLE_DBC, dbc);
    }
    if (!error.empty()) {
        return false;
    }
    return shared_env != SQL_NULL_HENV || own.Free(error);
}

// Plan scenario 10: churn where four in five logins fail - an unknown login, an
// unknown database, a refused port, and a server that never answers (login
// timeout) - on private and shared ENVs alike, interleaved with real logins.
// Each run's slot picks its kind and ENV, so every kind runs on both kinds of
// ENV at any profile size (at 2 x 1, in five rounds). Failed logins must
// report their SQLSTATE in bounded time and release what they allocated; the
// real logins between them must still succeed.
TEST_F(HandleLifecycleTest, ChurnWithFailedConnects) {
    RunWatched([&] {
        ClosedPort refused;
        SilentServer silent;
        const std::string base = BaseConnStr();
        const std::string unique =
            std::to_string(std::chrono::system_clock::now().time_since_epoch().count() % 1000000);
        const std::string connstrs[kKinds] = {
            base,
            ConnStrWith(base, {{"Trusted_Connection", "No"},
                               {"Authentication", ""},
                               {"UID", "mssql_odbc_mt_no_such_login_" + unique},
                               {"PWD", "x" + unique}}),
            ConnStrWith(base, {{"Database", "mssql_odbc_mt_no_such_db_" + unique}}),
            ConnStrWith(base, {{"Server", "tcp:127.0.0.1," + std::to_string(refused.Port())}}),
            ConnStrWith(base, {{"Server", "tcp:127.0.0.1," + std::to_string(silent.Port())}}),
        };

        const int rounds = RoundsCovering(3, 2 * kKinds, config_);
        Failures failures;
        ConnectCases cases;
        long long completed = 0;
        ProcessCounts baseline;
        for (int round = 0; round < rounds && failures.Empty(); ++round) {
            completed += RunWorkers(
                TestLabel() + " round " + std::to_string(round), config_, failures,
                [&, round](int worker, int iteration, std::string& error) {
                    const long long run = RunIndex(round, config_, worker, iteration);
                    // Iteration-major, so workers running the same iteration
                    // get different kinds instead of all failing alike at once.
                    const long long slot =
                        (static_cast<long long>(round) * config_.iterations + iteration) *
                            config_.threads +
                        worker;
                    const int kind = static_cast<int>(slot % kKinds);
                    const bool shared = (slot / kKinds) % 2 == 1;
                    cases.Ran(kind, shared);
                    const SQLHENV env = shared ? env_ : SQL_NULL_HENV;
                    if (kind == kConnects) {
                        return RunTree(env, 1 + run, error);
                    }
                    return FailConnect(env, kind, connstrs[kind], cases, error);
                });
            silent.CloseAccepted();
            if (round == 0) {
                baseline = SampleProcess();
            }
        }
        const ProcessCounts after = SampleProcess();
        EXPECT_TRUE(failures.Empty()) << failures.Summary();
        EXPECT_EQ(static_cast<long long>(rounds) * config_.threads * config_.iterations,
                  completed);
        if (failures.Empty()) {
            cases.Check();
            ExpectNoGrowth(baseline, after);
        }
    });
}

}  // namespace
