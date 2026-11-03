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

#include <map>
#include <set>

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
constexpr int kFailedConnectRounds = 3;

long long Token(int round, const Config& config, int worker, int iteration) {
    return 1 + (static_cast<long long>(round) * config.threads + worker) * config.iterations +
           iteration;
}

class HandleLifecycleTest : public LifecycleTest {
   protected:
    void Churn(bool shared_env);
};

// Each worker builds and tears down a whole tree per iteration, either under
// its own ENV or under the fixture's shared ENV.
void HandleLifecycleTest::Churn(bool shared_env) {
    const SQLHENV env = shared_env ? env_ : SQL_NULL_HENV;
    Failures failures;
    long long completed = 0;
    ProcessCounts baseline;
    for (int round = 0; round < kChurnRounds && failures.Empty(); ++round) {
        completed += RunWorkers(TestLabel() + " round " + std::to_string(round), config_, failures,
                                [&, round](int worker, int iteration, std::string& error) {
                                    return RunTree(env, Token(round, config_, worker, iteration),
                                                   error);
                                });
        if (round == 0) {
            baseline = SampleProcess();
        }
    }
    const ProcessCounts after = SampleProcess();
    EXPECT_TRUE(failures.Empty()) << failures.Summary();
    EXPECT_EQ(static_cast<long long>(kChurnRounds) * config_.threads * config_.iterations,
              completed);
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
// child list and runtime while other DBCs connect and query.
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

// A SQLSTATE that must appear among each failing login's records. Drivers
// order records differently (msodbcsql puts 28000 before an unknown
// database's 42000), and mssql-odbc reports a refused port as HYT00 after the
// login timeout where msodbcsql reports 08001.
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

// Short, for volume: only the failing logins use it.
constexpr SQLULEN kLoginTimeoutSeconds = 1;
// A failed login must return within the login timeout plus this.
constexpr long long kFailureSlackMs = 10000;

class FailedConnects {
   public:
    void Record(int kind, const std::string& state, long long elapsed_ms) {
        std::lock_guard<std::mutex> lock(mutex_);
        states_[kind].insert(state);
        slowest_ms_[kind] = (std::max)(slowest_ms_[kind], elapsed_ms);
        ++count_[kind];
    }

    void Report() const {
        std::lock_guard<std::mutex> lock(mutex_);
        for (int kind = kUnknownLogin; kind < kKinds; ++kind) {
            std::string states;
            for (const auto& state : states_.count(kind) ? states_.at(kind) : std::set<std::string>{}) {
                states += state + " ";
            }
            ::testing::Test::RecordProperty(
                std::string(KindName(kind)),
                std::to_string(count_.count(kind) ? count_.at(kind) : 0) + " attempts; states " +
                    states + "; slowest " +
                    std::to_string(slowest_ms_.count(kind) ? slowest_ms_.at(kind) : 0) + " ms");
        }
    }

   private:
    mutable std::mutex mutex_;
    std::map<int, std::set<std::string>> states_;
    std::map<int, long long> slowest_ms_;
    std::map<int, long long> count_;
};

// Connects with |connstr|, which must fail with |kind|'s SQLSTATE within the
// login timeout plus slack, then frees the DBC (and the tree's own ENV) with
// each step checked.
bool FailConnect(SQLHENV shared_env, int kind, const std::string& connstr, FailedConnects& seen,
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
    if (!SQL_SUCCEEDED(rc = SQLSetConnectAttr(dbc, SQL_ATTR_LOGIN_TIMEOUT,
                                              reinterpret_cast<SQLPOINTER>(kLoginTimeoutSeconds),
                                              0))) {
        error = Describe("SQLSetConnectAttr(LOGIN_TIMEOUT)", rc, SQL_HANDLE_DBC, dbc);
    } else {
        const Clock::time_point start = Clock::now();
        rc = DriverConnect(dbc, connstr);
        const long long elapsed_ms = MillisecondsSince(start);
        const std::string state = ODBCTestUtils::GetDiagState(SQL_HANDLE_DBC, dbc);
        if (SQL_SUCCEEDED(rc)) {
            error = std::string(KindName(kind)) + " login connected";
            SQLDisconnect(dbc);
        } else {
            seen.Record(kind, state, elapsed_ms);
            if (rc != SQL_ERROR || !HasExpectedState(kind, dbc)) {
                error = std::string(KindName(kind)) + " login expected SQL_ERROR with [" +
                        ExpectedStates(kind)[0] + "]; " +
                        Describe("SQLDriverConnect", rc, SQL_HANDLE_DBC, dbc);
            } else if (elapsed_ms >
                       static_cast<long long>(kLoginTimeoutSeconds) * 1000 + kFailureSlackMs) {
                error = std::string(KindName(kind)) + " login took " +
                        std::to_string(elapsed_ms) + " ms";
            } else if (kind == kNoAnswer &&
                       elapsed_ms < static_cast<long long>(kLoginTimeoutSeconds) * 1000 - 500) {
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
// Failed logins must report their SQLSTATE in bounded time and release what
// they allocated; the real logins between them must still succeed.
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

        Failures failures;
        FailedConnects seen;
        long long completed = 0;
        ProcessCounts baseline;
        for (int round = 0; round < kFailedConnectRounds && failures.Empty(); ++round) {
            completed += RunWorkers(
                TestLabel() + " round " + std::to_string(round), config_, failures,
                [&, round](int worker, int iteration, std::string& error) {
                    const int kind = (worker + iteration) % kKinds;
                    const SQLHENV env = iteration % 2 == 0 ? SQL_NULL_HENV : env_;
                    if (kind == kConnects) {
                        return RunTree(env, Token(round, config_, worker, iteration), error);
                    }
                    return FailConnect(env, kind, connstrs[kind], seen, error);
                });
            silent.CloseAccepted();
            if (round == 0) {
                baseline = SampleProcess();
            }
        }
        const ProcessCounts after = SampleProcess();
        seen.Report();
        EXPECT_TRUE(failures.Empty()) << failures.Summary();
        EXPECT_EQ(static_cast<long long>(kFailedConnectRounds) * config_.threads *
                      config_.iterations,
                  completed);
        if (failures.Empty()) {
            ExpectNoGrowth(baseline, after);
        }
    });
}

}  // namespace
