// Copyright (c) Microsoft Corporation. All rights reserved.
// more_results_test.cpp  –  E2E tests for SQLMoreResults.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_concurrent_test.h"
#include "odbc_test_fixture.h"
#include <cstring>

class MoreResultsLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// SQLMoreResults closes any open cursor (matches msodbcsql) and reports no
// more result sets. This is the path sqlcmd uses between batches.
ODBC_TEST_F_MODES(MoreResultsLiveTest, ClosesCursorAfterFetchEof, odbc_concurrency::kAllModes) {
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
ODBC_TEST_F_MODES(MoreResultsLiveTest, OnNoCursor, odbc_concurrency::kAllModes) {
    SQLRETURN rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// Multi-resultset batch: SELECT 1; SELECT 2; - SQLMoreResults advances to rs2.
ODBC_TEST_F_MODES(MoreResultsLiveTest, MultipleSelectBatchAdvances, odbc_concurrency::kAllModes) {
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

// An informational message emitted between two result sets (here a PRINT)
// surfaces as a diagnostic on the SQLMoreResults call that advances past it,
// and that call returns SQL_SUCCESS_WITH_INFO so the application is told to read
// it. This is the boundary where end-of-rowset INFO is surfaced with a
// return-code hint: SQLFetch returns SQL_NO_DATA at end of rows and cannot carry
// a "read diagnostics" hint, so the driver defers such messages to the next
// boundary-reporting call (SQLMoreResults advance, or SQLCloseCursor) rather than
// posting them under SQL_NO_DATA where many applications never read them.
ODBC_TEST_F_MODES(MoreResultsLiveTest, TrailingInfoBetweenResultSetsSurfacesWithHint,
                  odbc_concurrency::kAllModes) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SELECT 1 AS a; PRINT N'between result sets info'; SELECT 2 AS b;");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // Drain the first result set.
    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    // Advancing lands on the PRINT statement's own (0-column) result and
    // surfaces its message with a SQL_SUCCESS_WITH_INFO hint (msodbcsql parity).
    rc = SQLMoreResults(stmt_);
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, rc);

    SQLTCHAR state[8] = {};
    SQLINTEGER native = 0;
    SQLTCHAR message[512] = {};
    SQLSMALLINT messageLen = 0;
    SQLRETURN diagRc = SQLGetDiagRec(
        SQL_HANDLE_STMT, stmt_, 1, state, &native, message,
        static_cast<SQLSMALLINT>(sizeof(message) / sizeof(SQLTCHAR)), &messageLen);
    ASSERT_TRUE(diagRc == SQL_SUCCESS || diagRc == SQL_SUCCESS_WITH_INFO);
    std::string text = ODBCTestUtils::ToNarrow(SqlTString(message));
    EXPECT_NE(std::string::npos, text.find("between result sets info"));

    // The PRINT result has zero columns, so fetching from it is a cursor-state
    // error (24000), exactly like msodbcsql.
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    // Advance again to reach the second SELECT.
    rc = SQLMoreResults(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // The second result set is positioned and readable.
    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("2", reinterpret_cast<const char*>(buf));
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// SQLMoreResults called before consuming rows of rs1 drains and advances.
ODBC_TEST_F_MODES(MoreResultsLiveTest, BeforeFetchDrainsAndAdvances, odbc_concurrency::kAllModes) {
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
ODBC_TEST_F_MODES(MoreResultsLiveTest, ReExecAfterExhausted, odbc_concurrency::kAllModes) {
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


// Every result and row count is derived from the run, so under the concurrent
// modes a count or result set delivered to another worker's statement, or out
// of order, fails here. The batch interleaves DML counts with result sets; the
// order (count, rows, count, rows) matches msodbcsql's statement-wise
// navigation, see row_count_test.cpp.
ODBC_TEST_F_MODES(MoreResultsLiveTest, RunUniqueResultsAndRowCountsStayInOrder,
                  odbc_concurrency::kAllModes) {
    const int inserted = 2 + Worker() * 3 + Iteration() % 3;
    const int negated = 1 + (Worker() + Iteration()) % inserted;
    const std::string token = WorkerToken("results");
    const std::string batch =
        "DECLARE @t TABLE (n INT); "
        "INSERT @t SELECT TOP (" + std::to_string(inserted) +
        ") ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) FROM sys.all_objects; "
        "SELECT '" + token + "-a', COUNT(*) FROM @t; "
        "UPDATE @t SET n = -n WHERE n <= " + std::to_string(negated) + "; "
        "SELECT '" + token + "-b', SUM(n) FROM @t;";
    SqlTString sql = ODBCTestUtils::ToSqlTStr(batch);
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    auto expect_count = [&](SQLLEN expected) {
        SQLLEN rows = -999;
        ASSERT_SQL_OK(SQLRowCount(stmt_, &rows), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, rows);
    };
    auto expect_row = [&](const std::string& label, SQLINTEGER value) {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLCHAR text[64] = {};
        SQLLEN ind = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, text, sizeof(text), &ind),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(label, reinterpret_cast<const char*>(text));
        SQLINTEGER actual = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &actual, 0, nullptr), SQL_HANDLE_STMT,
                      stmt_);
        EXPECT_EQ(value, actual);
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    };

    expect_count(inserted);
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    expect_row(token + "-a", inserted);
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    expect_count(negated);
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    expect_row(token + "-b",
               inserted * (inserted + 1) / 2 - negated * (negated + 1));
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
}