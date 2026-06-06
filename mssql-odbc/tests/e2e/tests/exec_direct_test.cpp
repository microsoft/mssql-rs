// Copyright (c) Microsoft Corporation. All rights reserved.
// exec_direct_test.cpp  –  E2E tests for SQLExecDirectW.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

// SQL_NULL_HSTMT — the DM rejects this before the driver sees it.
TEST(ExecDirectTest, NullHandle) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLRETURN rc = SQLExecDirect(SQL_NULL_HSTMT,
                                 const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

TEST(ExecDirectTest, FetchNullHandle) {
    SQLRETURN rc = SQLFetch(SQL_NULL_HSTMT);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class ExecDirectLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// NULL SQL text pointer returns SQL_ERROR.
TEST_F(ExecDirectLiveTest, NullSqlText) {
    SQLRETURN rc = SQLExecDirect(stmt_, nullptr, SQL_NTS);
    EXPECT_SQL_ERROR(rc);
}

// Simple scalar query returns SQL_SUCCESS.
TEST_F(ExecDirectLiveTest, SelectScalar) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// Query that produces no rows still returns SQL_SUCCESS.
// Callers discover "no rows" via SQLFetch -> SQL_NO_DATA.
TEST_F(ExecDirectLiveTest, EmptyResultSet) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1 WHERE 1 = 0");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);

    // SQL_NO_DATA does not implicitly close cursor state.
    // Re-exec requires explicit SQLCloseCursor / SQLFreeStmt(SQL_CLOSE).
    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    rc = SQLCloseCursor(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// Syntactically invalid SQL returns SQL_ERROR.
TEST_F(ExecDirectLiveTest, InvalidSql) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("NOT VALID SQL @@##");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_ERROR(rc);
}

// Re-executing on the same STMT requires closing the cursor first.
// SQLExecDirectW leaves an open cursor for result-bearing queries; the caller
// must call SQLCloseCursor (or SQLFreeStmt(SQL_CLOSE)) before re-executing.
TEST_F(ExecDirectLiveTest, ReExecute) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // Re-executing while the cursor is still open must fail (SQLSTATE 24000).
    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    rc = SQLCloseCursor(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

TEST_F(ExecDirectLiveTest, DmlDoesNotOpenCursor) {
    SqlTString dml = ODBCTestUtils::ToSqlTStr("CREATE TABLE #t(i int); INSERT INTO #t VALUES (1);");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(dml.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // DML/DDL path should not open a cursor.
    rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    // Re-execute should succeed without explicit close for no-resultset path.
    SqlTString select_one = ODBCTestUtils::ToSqlTStr("SELECT 1");
    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(select_one.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

TEST_F(ExecDirectLiveTest, FetchOnFreshStatementReturnsHy010) {
    SQLRETURN rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

TEST_F(ExecDirectLiveTest, FreeStmtCloseAfterNoDataAllowsReExecute) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    rc = SQLFreeStmt(stmt_, SQL_CLOSE);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

TEST_F(ExecDirectLiveTest, CloseVsFreeStmtWhenNoCursorOpen) {
    SQLRETURN rc = SQLCloseCursor(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    rc = SQLFreeStmt(stmt_, SQL_CLOSE);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

TEST_F(ExecDirectLiveTest, DoubleFetchAtEndReturnsNoData) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

TEST_F(ExecDirectLiveTest, GetDataBasicChar) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 42");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, ind);
    EXPECT_STREQ("42", reinterpret_cast<const char*>(buf));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}
