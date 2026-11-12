// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"

#include <functional>
#include <future>
#include <pthread.h>

// Keep this in its own executable so the first connection also exercises cold
// TLS initialization. Unlike a direct Rust call, unixODBC consumes stack too.
class SmallStackConnectTest : public ODBCTest {
protected:
#ifdef ODBC_E2E_RELEASE_DRIVER
    static constexpr size_t kStackBytes = 128 * 1024;
#else
    static constexpr size_t kStackBytes = 512 * 1024;
#endif

    // Runs `body` to completion on a thread whose whole stack is kStackBytes.
    static void RunOnSmallStack(std::function<void()> body) {
        std::packaged_task<void()> operation(std::move(body));
        auto result = operation.get_future();
        pthread_attr_t attributes;
        ASSERT_EQ(0, pthread_attr_init(&attributes));
        const int configured = pthread_attr_setstacksize(&attributes, kStackBytes);
        pthread_t worker;
        const int started = configured == 0
            ? pthread_create(&worker, &attributes, [](void* argument) -> void* {
                  (*static_cast<std::packaged_task<void()>*>(argument))();
                  return nullptr;
              }, &operation)
            : configured;
        EXPECT_EQ(0, pthread_attr_destroy(&attributes));
        ASSERT_EQ(0, started);
        ASSERT_EQ(0, pthread_join(worker, nullptr));
        result.get();
    }
};

// Benefits-from-mock-tds: a TLS mock could verify the handshake and login packets;
// this regression measures the complete application/Driver Manager/driver stack.
TEST_F(SmallStackConnectTest, ConnectAndQueryOnSmallStack) {
    ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
        << "Set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";

    RunOnSmallStack([this] {
        ASSERT_NO_FATAL_FAILURE(Connect());
        ASSERT_NO_FATAL_FAILURE(ExecDirect("SELECT 1"));
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER value = 0;
        SQLLEN length = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_LONG, &value, sizeof(value), &length),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(1, value);
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(value)), length);
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    });
}

// A rejected login runs TLS and LOGIN7, then turns the server's error into
// diagnostics, all on the application's stack.
// Benefits-from-mock-tds: a mock could reject at each login stage; a live server
// only rejects after LOGIN7.
TEST_F(SmallStackConnectTest, RejectedLoginOnSmallStack) {
    const auto& cfg = ODBCTestConfig::Instance();
    if (cfg.HasConnStr() || cfg.HasDSN()) {
        GTEST_SKIP() << "Builds its own connection string from ODBC_TEST_SERVER";
    }
    ASSERT_TRUE(cfg.HasConnection()) << "Set ODBC_TEST_SERVER";
    std::string bad = "Driver={" + cfg.Driver() + "};Server=" + cfg.Server() +
                      ";UID=nonexistent_user_xyz;PWD=not_the_password" +
                      ";TrustServerCertificate=" + cfg.TrustCert();
    if (!cfg.Encrypt().empty()) {
        bad += ";Encrypt=" + cfg.Encrypt();
    }
    const SqlTString connstr = ODBCTestUtils::ToSqlTStr(bad);

    RunOnSmallStack([&] {
        SQLHDBC dbc = SQL_NULL_HDBC;
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc), SQL_HANDLE_ENV, env_);
        const SQLRETURN rc = SQLDriverConnect(
            dbc, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
            static_cast<SQLSMALLINT>(connstr.size()), nullptr, 0, nullptr,
            SQL_DRIVER_NOPROMPT);
        EXPECT_SQL_ERROR(rc);
        EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc, "28000");
        SQLFreeHandle(SQL_HANDLE_DBC, dbc);
    });
}
