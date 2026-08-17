// Copyright (c) Microsoft Corporation. All rights reserved.
// fetch_scroll_test.cpp  –  E2E tests for SQLFetchScroll.
//
// These cover the rowset machinery that does not depend on bound columns:
// cursor advance, *rows_fetched_ptr, the row status array, end-of-set, and the
// forward-only orientation rule. The bound-column fill loop needs SQLBindCol
// (AB#47359) before it can be driven from here; those cases arrive with it.
//
// Verifies:
//   1. NullHandle                          - SQL_NULL_HSTMT → SQL_INVALID_HANDLE
//   2. FreshStatementIsASequenceError      - never executed → HY010 (from the DM)
//   3. OnlyFetchNextIsSupported            - forward-only cursor → HY106
//   4. AdvancesTheCursorLikeFetch          - rowset of 1, then SQLGetData
//   5. ReportsRowsFetched                  - *rows_fetched_ptr per call
//   6. FillsTheRowStatusArray              - SQL_ROW_SUCCESS / SQL_ROW_NOROW
//   7. ReturnsNoDataAtEndOfResultSet
//   8. PartialRowsetAtEndOfResultSet       - fewer rows than the array size

#include "odbc_test_fixture.h"

#include <string>
#include <vector>

class FetchScrollLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    // Three rows, so a rowset larger than the result set can be exercised.
    void ExecThreeRows() {
        ExecDirect(
            "SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 ORDER BY n");
    }
};

TEST(FetchScrollTest, NullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE, SQLFetchScroll(SQL_NULL_HSTMT, SQL_FETCH_NEXT, 0));
}

// A statement that has never been executed has no result set, and the Driver
// Manager answers this one itself with HY010 without reaching the driver. The
// driver's own 24000 for an executed-but-closed cursor is covered by its unit
// tests, which call it directly.
TEST_F(FetchScrollLiveTest, FreshStatementIsASequenceError) {
    EXPECT_EQ(SQL_ERROR, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

// The cursor is forward-only, so a scrolling orientation is rejected rather
// than quietly treated as SQL_FETCH_NEXT.
TEST_F(FetchScrollLiveTest, OnlyFetchNextIsSupported) {
    ExecThreeRows();
    for (SQLSMALLINT orientation :
         {SQL_FETCH_PRIOR, SQL_FETCH_FIRST, SQL_FETCH_LAST, SQL_FETCH_ABSOLUTE,
          SQL_FETCH_RELATIVE}) {
        EXPECT_EQ(SQL_ERROR, SQLFetchScroll(stmt_, orientation, 0))
            << "orientation " << orientation;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY106");
    }
    SQLCloseCursor(stmt_);
}

// With the default rowset of one and no bound columns, SQLFetchScroll is
// SQLFetch: it positions the cursor and SQLGetData reads the row.
TEST_F(FetchScrollLiveTest, AdvancesTheCursorLikeFetch) {
    ExecThreeRows();
    for (int expected = 1; expected <= 3; ++expected) {
        ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER value = 0;
        SQLLEN indicator = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, value);
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicator);
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ReportsRowsFetched) {
    ExecThreeRows();
    SQLULEN rowsFetched = 999;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    for (int i = 0; i < 3; ++i) {
        rowsFetched = 999;
        ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(1u, rowsFetched) << "row " << i;
    }
    rowsFetched = 999;
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(0u, rowsFetched) << "the end-of-set call still reports a count";
    SQLCloseCursor(stmt_);
}

// The rows the fetch did not fill have to be marked, or the application reads
// stale statuses left by a previous, longer rowset.
TEST_F(FetchScrollLiveTest, FillsTheRowStatusArray) {
    ExecThreeRows();
    std::vector<SQLUSMALLINT> status(4, 0xFFFF);
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(4), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status.data(), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[0]);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[1]);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[2]);
    EXPECT_EQ(SQL_ROW_NOROW, status[3]);
    SQLCloseCursor(stmt_);
}

// A rowset wider than what is left returns the partial block, and the call
// after it reports end of set.
TEST_F(FetchScrollLiveTest, PartialRowsetAtEndOfResultSet) {
    ExecThreeRows();
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2u, rowsFetched);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1u, rowsFetched) << "the trailing partial rowset";

    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(0u, rowsFetched);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ReturnsNoDataAtEndOfResultSet) {
    ExecDirect("SELECT 1 AS n WHERE 1 = 0");
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    SQLCloseCursor(stmt_);
}
