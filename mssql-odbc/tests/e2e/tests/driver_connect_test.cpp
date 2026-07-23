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
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, hdbc, "08001");

    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// SQLDisconnect on a handle that was never connected returns error.
TEST_F(ODBCTest, Disconnect_NotConnected) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    rc = SQLDisconnect(hdbc);
    EXPECT_SQL_ERROR(rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, hdbc, "08003");

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
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
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
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, hdbc, "28000");

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
    // Truncation must return SQL_SUCCESS_WITH_INFO (SQLSTATE 01004).
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    // Scan every record: a live login interleaves the server's own 01000
    // ENVCHANGE info messages (database/language context) ahead of the DM's
    // 01004 truncation warning, so reading only record #1 would miss it.
    EXPECT_TRUE(ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, hdbc, "01004"));
    // outLen reports the FULL length (not truncated)
    EXPECT_GT(outLen, 7);

    SQLDisconnect(hdbc);
    SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
}

// Malformed tokens in connection string → SQL_SUCCESS_WITH_INFO on successful connect.
TEST_F(DriverConnectLiveTest, MalformedTokenReturnsSuccessWithInfo) {
    auto& cfg = ODBCTestConfig::Instance();
    // These parity cases build/corrupt a SQL-auth login string from scratch
    // (Server+UID+PWD), so they must skip cleanly when the suite is configured
    // via ODBC_TEST_CONNSTR, a DSN, or integrated auth (UID/PWD legitimately
    // empty). Tactical guard; capability-based gating tracked in a follow-up issue.
    if (!cfg.HasSqlAuth()) {
        GTEST_SKIP() << "Requires SQL auth (ODBC_TEST_SERVER + ODBC_TEST_UID + "
                        "ODBC_TEST_PWD); see follow-up issue for capability gating";
    }
    std::string base = "Driver={" + cfg.Driver() + "}"
                       ";Server=" + cfg.Server() +
                       ";UID=" + cfg.Uid() +
                       ";PWD=" + cfg.Pwd() +
                       ";TrustServerCertificate=" + cfg.TrustCert();

    struct ConnResult {
        SQLRETURN rc;
        bool has01S00;
        bool has28000;
    };

    // Connect with |cs| and report the return code plus whether the parser's
    // 01S00 warning and/or a 28000 login-failure SQLSTATE appear on ANY
    // diagnostic record. We scan every record (not just record #1): a
    // successful login interleaves the server's own 01000 info messages, and
    // msodbcsql 18 appends its 01S00 parse warning AFTER them (record #3),
    // whereas mssql-odbc posts it first. Reading only record #1 would miss it.
    auto tryConnect = [&](const std::string& cs) -> ConnResult {
        SQLHDBC hdbc = SQL_NULL_HDBC;
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
        EXPECT_EQ(SQL_SUCCESS, rc);
        if (rc != SQL_SUCCESS) return {rc, false, false};

        SqlTString connstr = ODBCTestUtils::ToSqlTStr(cs);
        SQLTCHAR outStr[1024] = {};
        SQLSMALLINT outLen = 0;

        rc = SQLDriverConnect(hdbc, nullptr,
                              const_cast<SQLTCHAR*>(connstr.c_str()),
                              static_cast<SQLSMALLINT>(connstr.size()),
                              outStr, 1024, &outLen,
                              SQL_DRIVER_NOPROMPT);

        bool has01S00 = false;
        bool has28000 = false;
        if (rc == SQL_SUCCESS_WITH_INFO || rc == SQL_ERROR) {
            has01S00 = ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, hdbc, "01S00");
            has28000 = ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, hdbc, "28000");
        }

        if (SQL_SUCCEEDED(rc)) SQLDisconnect(hdbc);
        SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
        return {rc, has01S00, has28000};
    };

    // A trailing token with no '=' is malformed: the key scan reaches
    // end-of-string without finding '=', so the parser posts 01S00 and keeps
    // whatever it parsed already. The connection still succeeds.
    {
        auto r = tryConnect(base + ";garbage");
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, r.rc);
        EXPECT_TRUE(r.has01S00);
    }

    // Empty key ("=value"): zero-length key name -> 01S00, connection proceeds.
    {
        auto r = tryConnect(base + ";=orphan");
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, r.rc);
        EXPECT_TRUE(r.has01S00);
    }

    // Several malformed tokens after a complete, valid attribute set.
    {
        auto r = tryConnect(base + ";garbage;=orphan;junk");
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, r.rc);
        EXPECT_TRUE(r.has01S00);
    }

    // Malformed token buried between valid keys -- matches msodbcsql exactly.
    //
    // The key scan stops ONLY at '=', never at ';'. So `;noequals;UID=<uid>`
    // is read as the single key name "noequals;UID", which is unknown and
    // discarded -- the real UID is never applied. The driver then tries to log
    // in as an empty user and the server rejects it -> SQL_ERROR + 28000.
    // (Previously flagged as a KNOWN DIVERGENCE; the rewritten single-pass
    // parser now reproduces msodbcsql byte-for-byte, verified against ODBC
    // Driver 18.)
    {
        auto r = tryConnect(
            "Driver={" + cfg.Driver() + "}"
            ";Server=" + cfg.Server() +
            ";noequals;UID=" + cfg.Uid() +
            ";PWD=" + cfg.Pwd() +
            ";TrustServerCertificate=" + cfg.TrustCert());
        EXPECT_EQ(SQL_ERROR, r.rc);
        EXPECT_TRUE(r.has28000);
    }

    // Extra separators around valid keys -- matches msodbcsql exactly. Leading
    // and between-key separator runs are consumed cleanly (each parse iteration
    // starts by skipping a run of whitespace and ';', then reads a real key).
    // A *trailing* run of 2+ separators is different: msodbcsql consumes exactly
    // one separator after the final value, then re-enters its parse loop, skips
    // the rest of the run, and finds a degenerate empty key at end-of-string,
    // which posts 01S00 (SQLSTATE record is appended AFTER the server's login
    // info messages, so HasDiagState -- which scans every record -- is required
    // to observe it). Direct probing of ODBC Driver 18 confirms: baseline and a
    // single trailing ';' emit no 01S00, but a trailing ';;'/';;;' does. Our
    // rewritten single-pass parser reproduces this exactly.
    {
        auto r = tryConnect("Driver={" + cfg.Driver() + "}"
            ";;;Server=" + cfg.Server() +
            ";;;UID=" + cfg.Uid() +
            ";;PWD=" + cfg.Pwd() +
            ";TrustServerCertificate=" + cfg.TrustCert() + ";;;");
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, r.rc);
        EXPECT_TRUE(r.has01S00);
    }

    // Unknown keys are ignored with a 01S00 warning; the connection succeeds.
    {
        auto r = tryConnect(base + ";FooBar=xyz;Bogus=123");
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, r.rc);
        EXPECT_TRUE(r.has01S00);
    }
}

// End-to-end coverage of the rewritten connection-string parser's observable
// behavior, exercised through the public SQLDriverConnect entry point against a
// live server. Fine-grained cases (`}}` escaping, verbatim value whitespace,
// oversized values) are covered exhaustively by the Rust unit tests in
// connection_string_parser.rs; here we assert the behaviors that are visible in
// a real login and that must stay in lock-step with msodbcsql.
TEST_F(DriverConnectLiveTest, ConnectionStringParserParityBehaviors) {
    auto& cfg = ODBCTestConfig::Instance();
    // These parity cases build/corrupt a SQL-auth login string from scratch
    // (Server+UID+PWD), so they must skip cleanly when the suite is configured
    // via ODBC_TEST_CONNSTR, a DSN, or integrated auth (UID/PWD legitimately
    // empty). Tactical guard; capability-based gating tracked in a follow-up issue.
    if (!cfg.HasSqlAuth()) {
        GTEST_SKIP() << "Requires SQL auth (ODBC_TEST_SERVER + ODBC_TEST_UID + "
                        "ODBC_TEST_PWD); see follow-up issue for capability gating";
    }

    struct ConnResult {
        SQLRETURN rc;
        bool has01S00;
        bool has28000;
    };

    auto tryConnect = [&](const std::string& cs) -> ConnResult {
        SQLHDBC hdbc = SQL_NULL_HDBC;
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
        EXPECT_EQ(SQL_SUCCESS, rc);
        if (rc != SQL_SUCCESS) return {rc, false, false};

        SqlTString connstr = ODBCTestUtils::ToSqlTStr(cs);
        SQLTCHAR outStr[1024] = {};
        SQLSMALLINT outLen = 0;

        rc = SQLDriverConnect(hdbc, nullptr,
                              const_cast<SQLTCHAR*>(connstr.c_str()),
                              static_cast<SQLSMALLINT>(connstr.size()),
                              outStr, 1024, &outLen,
                              SQL_DRIVER_NOPROMPT);

        bool has01S00 = false;
        bool has28000 = false;
        if (rc == SQL_SUCCESS_WITH_INFO || rc == SQL_ERROR) {
            has01S00 = ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, hdbc, "01S00");
            has28000 = ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, hdbc, "28000");
        }

        if (SQL_SUCCEEDED(rc)) SQLDisconnect(hdbc);
        SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
        return {rc, has01S00, has28000};
    };

    // Braced values: a leading '{' switches value scanning to "read until the
    // matching '}'", and the braces are stripped from the stored value. Wrapping
    // every value in braces must yield the same successful login as the plain
    // form, with no parse warning.
    {
        auto r = tryConnect("Driver={" + cfg.Driver() + "}"
            ";Server={" + cfg.Server() + "}"
            ";UID={" + cfg.Uid() + "}"
            ";PWD={" + cfg.Pwd() + "}"
            ";TrustServerCertificate={" + cfg.TrustCert() + "}");
        EXPECT_TRUE(SQL_SUCCEEDED(r.rc))
            << "rc=" << r.rc;
        EXPECT_FALSE(r.has01S00);
    }

    // First-wins duplicates: on a repeated key the FIRST occurrence is kept and
    // later ones are ignored. A valid UID followed by a bogus UID keeps the
    // valid one -> the login succeeds.
    {
        auto r = tryConnect(
            "Driver={" + cfg.Driver() + "}"
            ";Server=" + cfg.Server() +
            ";UID=" + cfg.Uid() +
            ";UID=bogus_user_should_be_ignored" +
            ";PWD=" + cfg.Pwd() +
            ";TrustServerCertificate=" + cfg.TrustCert());
        EXPECT_TRUE(SQL_SUCCEEDED(r.rc))
            << "rc=" << r.rc;
    }

    // First-wins, negative: a bogus UID BEFORE the valid one wins, so the login
    // is attempted as the bogus user and the server rejects it -> 28000.
    {
        auto r = tryConnect(
            "Driver={" + cfg.Driver() + "}"
            ";Server=" + cfg.Server() +
            ";UID=bogus_user_should_win" +
            ";UID=" + cfg.Uid() +
            ";PWD=" + cfg.Pwd() +
            ";TrustServerCertificate=" + cfg.TrustCert());
        EXPECT_EQ(SQL_ERROR, r.rc);
        EXPECT_TRUE(r.has28000);
    }

    // Keys are matched verbatim -- they are NOT trimmed. A space before '='
    // makes the key "UID " (with a trailing space), which does not match any
    // known keyword, so the real UID is never set. The driver logs in as an
    // empty user and the server rejects it -> 28000 (and the unknown key also
    // raises 01S00).
    {
        auto r = tryConnect(
            "Driver={" + cfg.Driver() + "}"
            ";Server=" + cfg.Server() +
            ";UID =" + cfg.Uid() +
            ";PWD=" + cfg.Pwd() +
            ";TrustServerCertificate=" + cfg.TrustCert());
        EXPECT_EQ(SQL_ERROR, r.rc);
        EXPECT_TRUE(r.has28000);
    }
}
