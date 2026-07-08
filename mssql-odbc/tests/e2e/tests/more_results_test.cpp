// Copyright (c) Microsoft Corporation. All rights reserved.
// more_results_test.cpp  –  E2E tests for SQLMoreResults.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"
#include <cstring>

class MoreResultsLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// SQLMoreResults closes any open cursor (matches msodbcsql) and reports no
// more result sets. This is the path sqlcmd uses between batches.
TEST_F(MoreResultsLiveTest, ClosesCursorAfterFetchEof) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);

    // Cursor was closed by SQLMoreResults - re-exec succeeds without an
    // explicit SQLCloseCursor.
    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// SQLMoreResults on a statement with no cursor returns SQL_NO_DATA (not error).
TEST_F(MoreResultsLiveTest, OnNoCursor) {
    SQLRETURN rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// Multi-resultset batch: SELECT 1; SELECT 2; - SQLMoreResults advances to rs2.
TEST_F(MoreResultsLiveTest, MultipleSelectBatchAdvances) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1; SELECT 2;");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // First result set.
    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("1", reinterpret_cast<const char*>(buf));
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    // Advance to second result set.
    rc = SQLMoreResults(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    std::memset(buf, 0, sizeof(buf));
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("2", reinterpret_cast<const char*>(buf));
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    // No more result sets.
    rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// SQLMoreResults called before consuming rows of rs1 drains and advances.
TEST_F(MoreResultsLiveTest, BeforeFetchDrainsAndAdvances) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3; SELECT 99;");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // Without fetching any rows from rs1, jump straight to rs2.
    rc = SQLMoreResults(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("99", reinterpret_cast<const char*>(buf));

    rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// Re-exec is allowed after SQLMoreResults returns SQL_NO_DATA (cursor closed).
TEST_F(MoreResultsLiveTest, ReExecAfterExhausted) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1; SELECT 2;");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // Walk through both rowsets.
    rc = SQLMoreResults(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLMoreResults(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    // Cursor is now closed - re-exec must succeed.
    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}
