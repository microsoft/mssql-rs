// Copyright (c) Microsoft Corporation. All rights reserved.
// driver_connect_test.cpp  –  E2E tests for SQLDriverConnectW / SQLDisconnect.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

// SQLDriverConnect with SQL_NULL_HDBC — the DM rejects this before the driver sees it.
TEST_F(ODBCTest, DriverConnect_NullDbc) {
    SqlTString connStr = ODBCTestUtils::ToSqlTStr("Server=h;UID=u;PWD=p");
    SQLTCHAR outStr[256] = {};
    SQLSMALLINT outLen = 0;

    SQLRETURN rc = SQLDriverConnect(SQL_NULL_HDBC, nullptr,
                                    const_cast<SQLTCHAR*>(connStr.c_str()), SQL_NTS,
                                    outStr, 256, &outLen,
                                    SQL_DRIVER_NOPROMPT);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// SQLDriverConnect with a non-NOPROMPT completion mode returns error.
TEST_F(ODBCTest, DriverConnect_UnsupportedCompletion) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    auto& cfg = ODBCTestConfig::Instance();
    std::string cs = "Driver={" + cfg.Driver() + "};Server=h;UID=u;PWD=p";
    SqlTString connStr = ODBCTestUtils::ToSqlTStr(cs);
    SQLTCHAR outStr[256] = {};
    SQLSMALLINT outLen = 0;

    // SQL_DRIVER_COMPLETE (1)
    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connStr.c_str()), SQL_NTS,
                          outStr, 256, &outLen,
                          SQL_DRIVER_COMPLETE);
    EXPECT_SQL_ERROR(rc);

    // SQL_DRIVER_PROMPT (2)
    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connStr.c_str()), SQL_NTS,
                          outStr, 256, &outLen,
                          SQL_DRIVER_PROMPT);
    EXPECT_SQL_ERROR(rc);

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// SQLDriverConnect with missing Server returns error.
TEST_F(ODBCTest, DriverConnect_MissingServer) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    auto& cfg = ODBCTestConfig::Instance();
    std::string cs = "Driver={" + cfg.Driver() + "};UID=u;PWD=p";
    SqlTString connStr = ODBCTestUtils::ToSqlTStr(cs);
    SQLTCHAR outStr[256] = {};
    SQLSMALLINT outLen = 0;

    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connStr.c_str()), SQL_NTS,
                          outStr, 256, &outLen,
                          SQL_DRIVER_NOPROMPT);
    EXPECT_SQL_ERROR(rc);

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// SQLDisconnect on a handle that was never connected returns error.
TEST_F(ODBCTest, Disconnect_NotConnected) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    rc = SQLDisconnect(hdbc);
    EXPECT_SQL_ERROR(rc);

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// SQLDisconnect with SQL_NULL_HDBC — the DM rejects this before the driver sees it.
TEST_F(ODBCTest, Disconnect_NullHandle) {
    SQLRETURN rc = SQLDisconnect(SQL_NULL_HDBC);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class DriverConnectLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
    }
};

// Connect, verify success, then disconnect.
TEST_F(DriverConnectLiveTest, ConnectAndDisconnect) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;

    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connstr.c_str()),
                          static_cast<SQLSMALLINT>(connstr.size()),
                          outStr, 1024, &outLen,
                          SQL_DRIVER_NOPROMPT);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);
    EXPECT_GT(outLen, 0) << "Output connection string length should be > 0";

    // Disconnect
    rc = SQLDisconnect(hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// Double connect on the same DBC returns error.
TEST_F(DriverConnectLiveTest, DoubleConnect) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;

    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connstr.c_str()),
                          static_cast<SQLSMALLINT>(connstr.size()),
                          outStr, 1024, &outLen,
                          SQL_DRIVER_NOPROMPT);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);

    // Second connect on same handle should fail
    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connstr.c_str()),
                          static_cast<SQLSMALLINT>(connstr.size()),
                          outStr, 1024, &outLen,
                          SQL_DRIVER_NOPROMPT);
    EXPECT_SQL_ERROR(rc);

    SQLDisconnect(hdbc);
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// Connect, disconnect, then reconnect on the same DBC.
TEST_F(DriverConnectLiveTest, ReconnectAfterDisconnect) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;

    // First connection
    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connstr.c_str()),
                          static_cast<SQLSMALLINT>(connstr.size()),
                          outStr, 1024, &outLen,
                          SQL_DRIVER_NOPROMPT);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);

    rc = SQLDisconnect(hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);

    // Reconnect on same handle
    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connstr.c_str()),
                          static_cast<SQLSMALLINT>(connstr.size()),
                          outStr, 1024, &outLen,
                          SQL_DRIVER_NOPROMPT);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, hdbc);

    SQLDisconnect(hdbc);
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// Bad credentials should fail with SQL_ERROR.
TEST_F(DriverConnectLiveTest, BadCredentials) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    auto& cfg = ODBCTestConfig::Instance();
    std::string bad = "Driver={" + cfg.Driver() + "}"
                      ";Server=" + cfg.Server() +
                      ";UID=nonexistent_user_xyz;PWD=wrong_password_123" +
                      ";TrustServerCertificate=Yes";
    SqlTString connstr = ODBCTestUtils::ToSqlTStr(bad);
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;

    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connstr.c_str()),
                          static_cast<SQLSMALLINT>(connstr.size()),
                          outStr, 1024, &outLen,
                          SQL_DRIVER_NOPROMPT);
    EXPECT_SQL_ERROR(rc);

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// Output buffer truncation — small buffer should still succeed.
TEST_F(DriverConnectLiveTest, OutputBufferTruncation) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[8] = {};  // Very small buffer
    SQLSMALLINT outLen = 0;

    rc = SQLDriverConnect(hdbc, nullptr,
                          const_cast<SQLTCHAR*>(connstr.c_str()),
                          static_cast<SQLSMALLINT>(connstr.size()),
                          outStr, 8, &outLen,
                          SQL_DRIVER_NOPROMPT);
    // Truncation must return SQL_SUCCESS_WITH_INFO (SQLSTATE 01004)
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    // outLen reports the FULL length (not truncated)
    EXPECT_GT(outLen, 7);

    SQLDisconnect(hdbc);
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// Malformed tokens in connection string → SQL_SUCCESS_WITH_INFO on successful connect.
TEST_F(DriverConnectLiveTest, MalformedTokenReturnsSuccessWithInfo) {
    auto& cfg = ODBCTestConfig::Instance();
    std::string base = "Driver={" + cfg.Driver() + "}"
                       ";Server=" + cfg.Server() +
                       ";UID=" + cfg.Uid() +
                       ";PWD=" + cfg.Pwd() +
                       ";TrustServerCertificate=" + cfg.TrustCert();

    auto tryConnect = [&](const std::string& cs) {
        SQLHDBC hdbc = SQL_NULL_HDBC;
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
        EXPECT_EQ(SQL_SUCCESS, rc);
        if (rc != SQL_SUCCESS) return rc;

        SqlTString connstr = ODBCTestUtils::ToSqlTStr(cs);
        SQLTCHAR outStr[1024] = {};
        SQLSMALLINT outLen = 0;

        rc = SQLDriverConnect(hdbc, nullptr,
                              const_cast<SQLTCHAR*>(connstr.c_str()),
                              static_cast<SQLSMALLINT>(connstr.size()),
                              outStr, 1024, &outLen,
                              SQL_DRIVER_NOPROMPT);

        if (SQL_SUCCEEDED(rc)) SQLDisconnect(hdbc);
        SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
        return rc;
    };

    // Token without '=' separator
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, tryConnect(base + ";garbage"));

    // Empty key (=value)
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, tryConnect(base + ";=orphan"));

    // Multiple malformed tokens
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, tryConnect(base + ";garbage;=orphan;junk"));

    // Malformed token buried between valid keys
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, tryConnect(
        "Driver={" + cfg.Driver() + "}"
        ";Server=" + cfg.Server() + ";noequals;UID=" + cfg.Uid() +
        ";PWD=" + cfg.Pwd() + ";TrustServerCertificate=" + cfg.TrustCert()));

    // Extra semicolons with valid keys — no warnings, should be plain SUCCESS
    EXPECT_EQ(SQL_SUCCESS, tryConnect("Driver={" + cfg.Driver() + "}"
        ";;;Server=" + cfg.Server() +
        ";;;UID=" + cfg.Uid() + ";;PWD=" + cfg.Pwd() +
        ";TrustServerCertificate=" + cfg.TrustCert() + ";;;"));

    // Unknown keys are ignored but return warning 01S00.
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, tryConnect(base + ";FooBar=xyz;Bogus=123"));
}
