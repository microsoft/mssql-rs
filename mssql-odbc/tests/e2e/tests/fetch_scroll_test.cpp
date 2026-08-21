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
//   9-15. Bound-column fetch: rowset fill, several columns, NULL indicators,
//         truncation, unbind, rebind, and mixed SQLGetData afterwards

#include "odbc_test_fixture.h"

#include <cstring>
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

// ---------------------------------------------------------------------------
// Bound-column fetch. These are the cases that exercise the rowset fill loop;
// until SQLBindCol existed there was no way to reach it from here.
// ---------------------------------------------------------------------------

// One bound int column over a rowset wider than one row: each row must land at
// its own offset in the array, with its own indicator.
TEST_F(FetchScrollLiveTest, BindsAnIntegerColumnAcrossARowset) {
    ExecThreeRows();
    SQLINTEGER values[4] = {-1, -1, -1, -1};
    SQLLEN indicators[4] = {-99, -99, -99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(4), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), indicators),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(3, values[2]);
    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicators[i]) << "row " << i;
    }
    SQLCloseCursor(stmt_);
}

// Two columns of different shapes bound at once, to prove the fill loop walks
// the binding table rather than assuming a single column.
TEST_F(FetchScrollLiveTest, BindsSeveralColumnsOfDifferentTypes) {
    ExecDirect(
        "SELECT 10 AS n, CAST('alpha' AS VARCHAR(20)) AS s"
        " UNION ALL SELECT 20, 'beta' ORDER BY n");
    SQLINTEGER nums[2] = {-1, -1};
    SQLCHAR text[2][32] = {};
    SQLLEN numInd[2] = {-99, -99};
    SQLLEN textInd[2] = {-99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    // Bound out of order on purpose: the fill loop has to visit them ascending.
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_CHAR, text, sizeof(text[0]), textInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, nums, sizeof(SQLINTEGER), numInd),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2u, rowsFetched);
    EXPECT_EQ(10, nums[0]);
    EXPECT_EQ(20, nums[1]);
    EXPECT_STREQ("alpha", reinterpret_cast<const char*>(text[0]));
    EXPECT_STREQ("beta", reinterpret_cast<const char*>(text[1]));
    EXPECT_EQ(5, textInd[0]);
    EXPECT_EQ(4, textInd[1]);
    SQLCloseCursor(stmt_);
}

// NULL is reported through the indicator, and must not disturb the data slot of
// a fixed-width target.
TEST_F(FetchScrollLiveTest, BoundNullIsReportedThroughTheIndicator) {
    // Ordered explicitly so the NULL's position is not left to the plan.
    ExecDirect(
        "SELECT n FROM (VALUES (1, 1), (2, NULL), (3, 3)) AS t(ord, n) ORDER BY ord");
    SQLINTEGER values[3] = {7, 7, 7};
    SQLLEN indicators[3] = {-99, -99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), indicators),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicators[0]);
    EXPECT_EQ(SQL_NULL_DATA, indicators[1]);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicators[2]);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(7, values[1]) << "a NULL must not disturb its data slot";
    EXPECT_EQ(3, values[2]);
    SQLCloseCursor(stmt_);
}

// A bound column gets one shot at a fixed buffer, so an over-long value is
// truncated with 01004 and the indicator reports the untruncated length.
TEST_F(FetchScrollLiveTest, BoundCharacterDataTruncatesWithInfo) {
    ExecDirect("SELECT CAST('abcdefghij' AS VARCHAR(20)) AS s");
    SQLCHAR text[5] = {};
    SQLLEN indicator = -99;
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, text, sizeof(text), &indicator),
                  SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(1u, rowsFetched);
    EXPECT_STREQ("abcd", reinterpret_cast<const char*>(text));
    EXPECT_EQ(10, indicator) << "the indicator reports the untruncated length";
    SQLCloseCursor(stmt_);
}

// SQLFreeStmt(SQL_UNBIND) drops every binding; mssql-python calls it before
// each fetch, so a fetch afterwards must deliver nothing.
TEST_F(FetchScrollLiveTest, UnbindStopsDelivery) {
    ExecThreeRows();
    SQLINTEGER value = -1;
    SQLLEN indicator = -99;
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, value);

    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
    value = -1;
    indicator = -99;
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1u, rowsFetched) << "the row is still fetched, just not delivered";
    EXPECT_EQ(-1, value) << "an unbound column must not be written";
    EXPECT_EQ(-99, indicator);
    SQLCloseCursor(stmt_);
}

// Rebinding a column replaces its entry rather than adding a second one, so the
// value lands in the new buffer only.
TEST_F(FetchScrollLiveTest, RebindingAColumnReplacesTheBinding) {
    ExecThreeRows();
    SQLINTEGER first = -1;
    SQLINTEGER second = -1;
    SQLLEN indicator = -99;

    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &first, sizeof(first), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &second, sizeof(second), &indicator),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, first) << "the replaced binding must not be written";
    EXPECT_EQ(1, second);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicator);
    SQLCloseCursor(stmt_);
}

// Unbinding one column leaves the others delivering.
TEST_F(FetchScrollLiveTest, UnbindingOneColumnLeavesTheOthers) {
    ExecDirect("SELECT 10 AS a, 20 AS b");
    SQLINTEGER a = -1;
    SQLINTEGER b = -1;

    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &a, sizeof(a), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &b, sizeof(b), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    // Both null unbinds column 1.
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, nullptr, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, a) << "column 1 was unbound";
    EXPECT_EQ(20, b);
    SQLCloseCursor(stmt_);
}

// A bound fetch of a single row leaves the cursor positioned, so SQLGetData can
// still read a column the fill loop did not take.
TEST_F(FetchScrollLiveTest, GetDataStillWorksAfterASingleRowBoundFetch) {
    ExecDirect("SELECT 10 AS a, 20 AS b");
    SQLINTEGER a = -1;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &a, sizeof(a), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(10, a);

    SQLINTEGER b = -1;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &b, sizeof(b), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(20, b);
    SQLCloseCursor(stmt_);
}

// ODBC defines SQLFetch as the SQL_FETCH_NEXT form of SQLFetchScroll, so the
// classic SQLBindCol + SQLFetch loop must fill the bound buffers too. Keeping a
// second row-reading path is how that silently stops being true.
TEST_F(FetchScrollLiveTest, SQLFetchFillsBoundColumnsToo) {
    ExecThreeRows();
    SQLINTEGER value = -1;
    SQLLEN indicator = -99;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                  SQL_HANDLE_STMT, stmt_);

    for (int expected = 1; expected <= 3; ++expected) {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, value);
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicator);
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// SQL_ATTR_ROW_ARRAY_SIZE applies to SQLFetch as well, so it returns a rowset
// rather than a single row.
TEST_F(FetchScrollLiveTest, SQLFetchHonoursTheRowsetSize) {
    ExecThreeRows();
    SQLINTEGER values[3] = {-1, -1, -1};
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), nullptr),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(3, values[2]);
    SQLCloseCursor(stmt_);
}

// A bound LOB column cannot be delivered into a fixed buffer yet (AB#47361),
// but its bytes still have to leave the wire. Abandoning the PLP stream mid
// value left the row cursor inside the LOB, so the *next* column parsed payload
// bytes as a length prefix -- which segfaulted the driver rather than failing
// cleanly. The second bound column is the part that matters here.
TEST_F(FetchScrollLiveTest, ABoundLobColumnDoesNotDesyncTheRow) {
    ExecDirect(
        "SELECT REPLICATE(CAST('x' AS NVARCHAR(MAX)), 10000) AS lob, 4242 AS n");

    SQLWCHAR lob[64] = {0};
    SQLLEN lobInd = 0;
    SQLINTEGER n = -1;
    SQLLEN nInd = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, lob, sizeof(lob), &lobInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &n, sizeof(n), &nInd),
                  SQL_HANDLE_STMT, stmt_);

    // The two drivers legitimately differ on the LOB itself: msodbcsql truncates
    // it into the buffer, we report the row as unsupported for now. Neither is
    // allowed to crash or to desynchronise the row, which is what this asserts.
    SQLRETURN rc = SQLFetch(stmt_);
    EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO || rc == SQL_ERROR)
        << "unexpected rc " << rc;

    // The row stream has to be intact afterwards: a clean single-row result set
    // ends here rather than returning garbage or faulting.
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// The same hazard through the block path, and with the LOB last so the drain
// has to happen even when no bound column follows it.
TEST_F(FetchScrollLiveTest, ABoundLobIsDrainedAcrossARowset) {
    ExecDirect(
        "SELECT n, REPLICATE(CAST('y' AS NVARCHAR(MAX)), 5000) AS lob "
        "FROM (VALUES (1),(2),(3)) AS t(n) ORDER BY n");

    SQLINTEGER ns[3] = {-1, -1, -1};
    SQLWCHAR lobs[3][32] = {};
    SQLLEN nInd[3] = {0, 0, 0};
    SQLLEN lobInd[3] = {0, 0, 0};
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, ns, sizeof(SQLINTEGER), nInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_WCHAR, lobs, sizeof(lobs[0]), lobInd),
                  SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0);
    EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO || rc == SQL_ERROR)
        << "unexpected rc " << rc;
    // Every row has to have been walked past, whatever happened to the LOBs.
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    SQLCloseCursor(stmt_);
}

// Mixed access after a *block* fetch. ODBC expects SQLSetPos to nominate a row
// first, which is not implemented, so the cursor is deliberately left
// unpositioned. The contract that matters is that SQLGetData then fails
// cleanly: an unpositioned cursor must not be read as though a row were there.
TEST_F(FetchScrollLiveTest, GetDataAfterABlockFetchFailsCleanly) {
    ExecThreeRows();
    SQLINTEGER values[3] = {-1, -1, -1};
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(3u, rowsFetched);

    // Must return an error rather than crashing or inventing a value.
    SQLINTEGER scratch = -1;
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_SLONG, &scratch, sizeof(scratch), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    SQLCloseCursor(stmt_);
}

// A null TargetValuePtr unbinds the column outright; the indicator is never
// consulted. The column must therefore stay available to SQLGetData, which is
// what distinguishes an unbind from a binding that delivers nothing.
TEST_F(FetchScrollLiveTest, ANullTargetPointerUnbindsAndLeavesTheColumnReadable) {
    ExecDirect("SELECT CAST(42 AS INT) AS n, CAST('hi' AS VARCHAR(10)) AS s");
    SQLINTEGER v = -1;
    SQLLEN ind = -999;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), &ind),
                  SQL_HANDLE_STMT, stmt_);
    // Rebinding with a null data pointer unbinds, even though the indicator is live.
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, nullptr, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, v) << "an unbound column is not delivered";
    EXPECT_EQ(static_cast<SQLLEN>(-999), ind);

    // Column 1 must not have been consumed by a lingering binding.
    SQLINTEGER viaGetData = -1;
    SQLLEN gdInd = 0;
    EXPECT_TRUE(SQL_SUCCEEDED(
        SQLGetData(stmt_, 1, SQL_C_SLONG, &viaGetData, sizeof(viaGetData), &gdInd)));
    EXPECT_EQ(42, viaGetData);
    SQLCloseCursor(stmt_);
}

// msodbcsql keys the fetch return code on the rowset size: a single-row fetch
// lets a row error stand as SQL_ERROR, while a block fetch demotes it to
// SQL_SUCCESS_WITH_INFO and leaves the detail in the row status array.
TEST_F(FetchScrollLiveTest, ARowErrorIsSQL_ERRORAtRowsetSizeOne) {
    ExecDirect("SELECT CAST(NULL AS INT) AS z");
    SQLINTEGER v = -1;
    // NULL with no indicator to report it through is a row error.
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// The same row error inside a rowset is demoted, so the application reads the
// per-row detail from the status array instead.
TEST_F(FetchScrollLiveTest, TheSameRowErrorIsDemotedInABlockFetch) {
    ExecDirect("SELECT CAST(NULL AS INT) AS z");
    SQLINTEGER v[2] = {-1, -1};
    SQLUSMALLINT status[2] = {0, 0};
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, v, sizeof(SQLINTEGER), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(SQL_ROW_ERROR, status[0]);
    SQLCloseCursor(stmt_);
}

// A binding left over from a wider result set is not an error: msodbcsql
// skips it and reports nothing at all -- no diagnostic, plain SQL_SUCCESS.
// Neither 07009 ("invalid descriptor index", which is what bind time uses for
// an out-of-range ordinal) nor 07006 is raised, so a stale binding must not
// start failing fetches here.
TEST_F(FetchScrollLiveTest, AStaleOrdinalPastTheResultSetIsIgnored) {
    ExecDirect("SELECT 1 AS only_column");
    SQLINTEGER v = -1;
    SQLLEN ind = -999;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 5, SQL_C_SLONG, &v, sizeof(v), &ind),
                  SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    // No diagnostic, and the untouched binding stays untouched.
    SQLWCHAR state[6] = {0};
    SQLINTEGER native = 0;
    SQLWCHAR msg[256] = {0};
    SQLSMALLINT msgLen = 0;
    EXPECT_EQ(SQL_NO_DATA, SQLGetDiagRecW(SQL_HANDLE_STMT, stmt_, 1, state,
                                          &native, msg, 256, &msgLen));
    EXPECT_EQ(static_cast<SQLLEN>(-999), ind);
    SQLCloseCursor(stmt_);
}
