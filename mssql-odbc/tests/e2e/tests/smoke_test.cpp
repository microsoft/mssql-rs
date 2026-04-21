// Copyright (c) Microsoft Corporation. All rights reserved.
// smoke_test.cpp  –  Minimal tests to verify the gtest + ODBC infrastructure.
//
// These tests validate that:
//   1. Environment allocation works (no server needed)
//   2. Connection to a SQL Server works (requires ODBC_TEST_SERVER)
//   3. A basic query returns data
//
// Run with:
//   export ODBC_TEST_SERVER=myserver
//   ctest --test-dir build   (or just:  build/smoke_test)

#include "odbc_test_fixture.h"

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

// Verify ODBC version attribute was set correctly.
TEST_F(ODBCTest, OdbcVersionIsSet) {
    SQLINTEGER version = 0;
    SQLRETURN rc = SQLGetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                                 &version, sizeof(version), nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);
    EXPECT_EQ(static_cast<SQLINTEGER>(SQL_OV_ODBC3_80), version);
}

// Allocate a DBC without connecting — should succeed.
TEST_F(ODBCTest, AllocDbcWithoutConnect) {
    SQLHDBC hdbc = SQL_NULL_HDBC;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &hdbc);
    EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO);
    if (hdbc != SQL_NULL_HDBC) {
        SQLFreeHandle(SQL_HANDLE_DBC, hdbc);
    }
}

// ===================================================================
// Tests that require a live SQL Server  (skipped if no server set)
// ===================================================================

class SmokeConnectedTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        auto& cfg = ODBCTestConfig::Instance();
        if (!cfg.HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_DSN, ODBC_TEST_SERVER, or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// Verify we can connect and run SELECT 1.
TEST_F(SmokeConnectedTest, SelectOne) {
    ExecDirect("SELECT 1");

    SQLINTEGER value = 0;
    SQLLEN ind = 0;
    SQLRETURN rc = SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, 0, &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, value);

    // No more rows
    rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// Verify @@VERSION returns a non-empty string.
TEST_F(SmokeConnectedTest, ServerVersion) {
    ExecDirect("SELECT @@VERSION");

    char buf[512] = {};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_GT(ind, 0) << "@@VERSION returned empty";
    std::string version(buf);
    bool found = version.find("Microsoft SQL Server") != std::string::npos ||
                 version.find("Microsoft Azure SQL Edge") != std::string::npos;
    EXPECT_TRUE(found) << "Unexpected version string: " << buf;
}

// Verify that an invalid query produces SQL_ERROR and a valid SQLSTATE.
TEST_F(SmokeConnectedTest, InvalidQueryReturnsError) {
    SqlTString invalid_sql = ODBCTestUtils::ToSqlTStr("THIS IS NOT VALID SQL");
    SQLRETURN rc = SQLExecDirect(
        stmt_,
        const_cast<SQLTCHAR*>(invalid_sql.c_str()),
        SQL_NTS);
    EXPECT_EQ(SQL_ERROR, rc);

    std::string state = StmtDiagState();
    EXPECT_FALSE(state.empty()) << "Expected a SQLSTATE on error";
}

// Verify temp table creation and INSERT/SELECT round-trip.
TEST_F(SmokeConnectedTest, TempTableRoundTrip) {
    ExecDirect("CREATE TABLE #gtest_smoke (id INT, name VARCHAR(50))");
    ExecDirect("INSERT INTO #gtest_smoke VALUES (42, 'hello')");

    // Need a fresh statement or close cursor
    SQLCloseCursor(stmt_);
    ExecDirect("SELECT id, name FROM #gtest_smoke");

    SQLINTEGER id = 0;
    char name[64] = {};
    SQLLEN idInd = 0, nameInd = 0;

    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &id, 0, &idInd), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_CHAR, name, sizeof(name), &nameInd), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, id);
    EXPECT_STREQ("hello", name);
}
