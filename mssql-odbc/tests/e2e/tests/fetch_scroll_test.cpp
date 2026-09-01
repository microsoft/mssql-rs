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

    bool ServerSupportsNativeJson() {
        SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT CAST(N'{}' AS JSON)");
        const bool ok =
            SQL_SUCCEEDED(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS));
        SQLCloseCursor(stmt_);
        return ok;
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

// SQL_C_DEFAULT is retained by SQLBindCol and resolved from each result
// column's IRD type when the rowset is fetched. This covers both the fixed
// stride of SQL_C_SLONG and the application-sized stride of SQL_C_CHAR.
TEST_F(FetchScrollLiveTest, DefaultTargetResolvesAtFetchTime) {
    SQLINTEGER values[3] = {-1, -1, -1};
    SQLCHAR text[3][8] = {};
    SQLLEN valueIndicators[3] = {-99, -99, -99};
    SQLLEN textIndicators[3] = {-99, -99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_DEFAULT, values, sizeof(SQLINTEGER),
                            valueIndicators),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_DEFAULT, text, sizeof(text[0]), textIndicators),
                  SQL_HANDLE_STMT, stmt_);

    ExecDirect(
        "SELECT n, s FROM (VALUES (1, CAST('one' AS VARCHAR(8))), "
        "(2, 'two'), (3, 'three')) AS t(n, s) ORDER BY n");
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(3, values[2]);
    EXPECT_STREQ("one", reinterpret_cast<const char*>(text[0]));
    EXPECT_STREQ("two", reinterpret_cast<const char*>(text[1]));
    EXPECT_STREQ("three", reinterpret_cast<const char*>(text[2]));
    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), valueIndicators[i]);
    }
    EXPECT_EQ(3, textIndicators[0]);
    EXPECT_EQ(3, textIndicators[1]);
    EXPECT_EQ(5, textIndicators[2]);
    SQLCloseCursor(stmt_);
}

// The two deliberate deviations from msodbcsql's Sql2CDefault, which the fetch
// path inherits from the resolver it shares with SQLBindParameter: an NVARCHAR
// column resolves to SQL_C_WCHAR and a uniqueidentifier to SQL_C_GUID, where
// msodbcsql resolves both to its ANSI SQL_C_CHAR. The GUID case also pins the
// resulting rowset layout, because a fixed-width target strides by its C type
// rather than by BufferLength. See mssql-odbc/docs/typed-columnar-fetch-plan.md,
// which records the measured msodbcsql values these assertions diverge from.
//
// Skipped on the reference leg by construction: asserting a deviation is the
// point, so comparing it would always report a divergence.
TEST_F(FetchScrollLiveTest, DefaultTargetResolvesWideAndGuidToTypedTargets) {
    SKIP_IF_COMPARING_MSODBCSQL();
    SQLWCHAR wide[2][8] = {};
    // Four slots for a rowset of two, with BufferLength deliberately set to two
    // SQLGUIDs. A BufferLength-driven stride would land row 1 in guids[2]; the
    // C-type stride lands it in guids[1]. Both stay inside the array, so the
    // wrong layout fails an assertion instead of corrupting the stack.
    SQLGUID guids[4] = {};
    SQLLEN wideIndicators[2] = {-99, -99};
    SQLLEN guidIndicators[2] = {-99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_DEFAULT, wide, sizeof(wide[0]), wideIndicators),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_DEFAULT, guids,
                             static_cast<SQLLEN>(2 * sizeof(SQLGUID)), guidIndicators),
                  SQL_HANDLE_STMT, stmt_);

    ExecDirect(
        "SELECT w, g FROM (VALUES "
        "(1, CAST(N'one' AS NVARCHAR(8)), "
        "CAST('01020304-0506-0708-090A-0B0C0D0E0F10' AS UNIQUEIDENTIFIER)), "
        "(2, N'two', CAST('11121314-1516-1718-191A-1B1C1D1E1F20' AS UNIQUEIDENTIFIER))"
        ") AS t(n, w, g) ORDER BY n");
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2u, rowsFetched);

    const SQLWCHAR one[] = {'o', 'n', 'e', 0};
    const SQLWCHAR two[] = {'t', 'w', 'o', 0};
    for (int i = 0; i < 4; ++i) {
        EXPECT_EQ(one[i], wide[0][i]) << "row 0 unit " << i;
        EXPECT_EQ(two[i], wide[1][i]) << "row 1 unit " << i;
    }
    // Bytes of UTF-16, which is what makes the wide resolution observable: the
    // narrow default would report 3.
    EXPECT_EQ(static_cast<SQLLEN>(3 * sizeof(SQLWCHAR)), wideIndicators[0]);
    EXPECT_EQ(static_cast<SQLLEN>(3 * sizeof(SQLWCHAR)), wideIndicators[1]);

    EXPECT_EQ(0x01020304u, guids[0].Data1);
    EXPECT_EQ(0x0506u, guids[0].Data2);
    EXPECT_EQ(0x0708u, guids[0].Data3);
    const unsigned char firstTail[8] = {0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10};
    EXPECT_EQ(0, std::memcmp(guids[0].Data4, firstTail, sizeof(firstTail)));
    EXPECT_EQ(0x11121314u, guids[1].Data1);
    EXPECT_EQ(0x1516u, guids[1].Data2);
    EXPECT_EQ(0x1718u, guids[1].Data3);
    const unsigned char secondTail[8] = {0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20};
    EXPECT_EQ(0, std::memcmp(guids[1].Data4, secondTail, sizeof(secondTail)));
    // Nothing beyond the rowset was written, which is what rules out a
    // BufferLength-driven stride.
    const SQLGUID untouched{};
    EXPECT_EQ(0, std::memcmp(&guids[2], &untouched, sizeof(SQLGUID)));
    // sizeof(SQLGUID), not the 36 characters msodbcsql's SQL_C_CHAR would give.
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLGUID)), guidIndicators[0]);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLGUID)), guidIndicators[1]);
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

// A bound LOB column is delivered into the fixed buffer (AB#47361), and its
// bytes have to leave the wire either way. Abandoning the PLP stream mid value
// left the row cursor inside the LOB, so the *next* column parsed payload bytes
// as a length prefix -- which segfaulted the driver rather than failing
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

    // The LOB truncates into the buffer with 01004, and the column after it
    // still arrives -- the desync this guards against would corrupt that one.
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_EQ(4242, n) << "the column after the LOB must still decode";

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

// ---------------------------------------------------------------------------
// Bound PLP (max/LOB) delivery — AB#47361.
//
// The indicator rule is the subtle part, and each case below was verified
// against msodbcsql before being asserted: a value that fits reports exactly
// what was produced; a truncated one reports the full length when the target's
// units match the wire's, and SQL_NO_TOTAL when transcoding makes the wire byte
// count the wrong unit to report.
// ---------------------------------------------------------------------------

TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxThatFitsReportsItsLength) {
    ExecDirect("SELECT CAST(N'abcdefghij' AS NVARCHAR(MAX)) AS c1");

    char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_STREQ("abcdefghij", buf);
    EXPECT_EQ(10, ind);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Transcoding UTF-16 to a narrow target means the wire byte count is not the
// delivered byte count, so the full length cannot be reported.
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxTruncatedToCharReportsNoTotal) {
    ExecDirect("SELECT REPLICATE(CAST(N'x' AS NVARCHAR(MAX)), 5000) AS c1");

    char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(SQL_NO_TOTAL, ind);
    EXPECT_EQ(31u, std::strlen(buf)) << "filled to capacity, less the terminator";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Same source into a wide target: the units match the wire, so the full length
// is knowable and is reported.
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxTruncatedToWcharReportsFullLength) {
    ExecDirect("SELECT REPLICATE(CAST(N'x' AS NVARCHAR(MAX)), 5000) AS c1");

    SQLWCHAR buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(10000, ind) << "5000 characters, two bytes each";

    // The indicator alone would pass while the buffer held nothing, so check
    // what actually landed.
    int units = 0;
    while (units < 16 && buf[units] != 0) {
        ++units;
    }
    EXPECT_EQ(15, units) << "filled to capacity, less the terminator";
    EXPECT_EQ(u'x', buf[0]);
    EXPECT_EQ(u'x', buf[14]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundVarcharMaxTruncatedReportsFullLength) {
    ExecDirect("SELECT REPLICATE(CAST('y' AS VARCHAR(MAX)), 5000) AS c1");

    char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(5000, ind) << "same encoding, so the length is knowable";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Non-ASCII, to prove the transcode is not a byte copy: each e-acute is two
// UTF-16 bytes on the wire and two UTF-8 bytes delivered.
//
// Skipped on the msodbcsql leg because it asserts UTF-8 specifically. This
// driver always delivers SQL_C_CHAR as UTF-8; msodbcsql converts to the client
// code page, so on a Windows client the same value arrives as one 0xE9 byte per
// character. That is the documented divergence AB#47564, and it is invisible on
// Linux only because the client code page there is already UTF-8.
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxTranscodesNonAscii) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("SELECT CAST(REPLICATE(NCHAR(233), 4) AS NVARCHAR(MAX)) AS c1");

    unsigned char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(8, ind) << "four characters, two UTF-8 bytes each";
    for (int i = 0; i < 4; ++i) {
        EXPECT_EQ(0xC3u, buf[i * 2]);
        EXPECT_EQ(0xA9u, buf[i * 2 + 1]);
    }
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Truncation has to stop on a character boundary. Each e-acute is two UTF-8
// bytes, so a 32-byte buffer holds 15 of them in 30 bytes and the 16th does not
// fit -- delivering its lead byte alone would leave the caller with text that
// does not decode. msodbcsql trims the same way (TrimPartialCodePt).
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxTruncatesOnACharacterBoundary) {
    SKIP_IF_COMPARING_MSODBCSQL();  // asserts UTF-8; see AB#47564 above
    ExecDirect("SELECT REPLICATE(CAST(NCHAR(233) AS NVARCHAR(MAX)), 5000) AS c1");

    unsigned char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    const size_t len = std::strlen(reinterpret_cast<const char*>(buf));
    EXPECT_EQ(30u, len) << "15 whole characters, not 31 bytes ending mid-sequence";
    ASSERT_EQ(0u, len % 2u);
    for (size_t i = 0; i < len; i += 2) {
        EXPECT_EQ(0xC3u, buf[i]) << "lead byte at " << i;
        EXPECT_EQ(0xA9u, buf[i + 1]) << "continuation byte at " << (i + 1);
    }
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundJsonTruncatesOnACharacterBoundary) {
    SKIP_IF_COMPARING_MSODBCSQL();  // asserts UTF-8; see AB#47564 above
    if (!ServerSupportsNativeJson()) {
        GTEST_SKIP() << "server has no native json type";
    }
    ExecDirect(
        "SELECT CAST(N'[\"' + REPLICATE(NCHAR(233), 20) + N'\"]' AS JSON) AS c1");

    unsigned char buf[10] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    const size_t len = std::strlen(reinterpret_cast<const char*>(buf));
    EXPECT_EQ(8u, len) << "three whole e-acute characters after the JSON prefix";
    EXPECT_EQ('[', buf[0]);
    EXPECT_EQ('"', buf[1]);
    for (size_t i = 2; i < len; i += 2) {
        EXPECT_EQ(0xC3u, buf[i]) << "lead byte at " << i;
        EXPECT_EQ(0xA9u, buf[i + 1]) << "continuation byte at " << (i + 1);
    }
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// The wide-target equivalent: a surrogate pair must not be split across the
// capacity boundary. U+1F600 is two code units, so an odd-sized buffer would
// otherwise end on a lone high surrogate. msodbcsql trims it too
// (GetColDataSurrogateSafe).
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxDoesNotSplitASurrogatePair) {
    ExecDirect(
        "SELECT REPLICATE(CAST(NCHAR(0xD83D) + NCHAR(0xDE00) AS NVARCHAR(MAX)), 500) AS c1");

    // 9 usable units: four whole pairs, and no room for the ninth's low half.
    SQLWCHAR buf[10] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    int units = 0;
    while (units < 10 && buf[units] != 0) {
        ++units;
    }
    EXPECT_EQ(8, units) << "four whole pairs; the fifth high surrogate is dropped";
    ASSERT_EQ(0, units % 2);
    for (int i = 0; i < units; i += 2) {
        EXPECT_GE(buf[i], 0xD800) << "high surrogate at " << i;
        EXPECT_LT(buf[i], 0xDC00) << "high surrogate at " << i;
        EXPECT_GE(buf[i + 1], 0xDC00) << "low surrogate at " << (i + 1);
        EXPECT_LT(buf[i + 1], 0xE000) << "low surrogate at " << (i + 1);
    }
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Bound binary delivery is unimplemented for every type, not just the max ones
// (AB#47239), so this asserts our own answer rather than parity -- msodbcsql
// delivers it.
TEST_F(FetchScrollLiveTest, ABoundVarbinaryMaxIsStillUnsupported) {
    SKIP_IF_COMPARING_MSODBCSQL();
    // Two rows and a trailing scalar: the refused target takes the drain path
    // rather than the fill loop, so proving the row ended is not enough --
    // the value after it, and the row after that, have to decode correctly.
    ExecDirect(
        "SELECT n, REPLICATE(CAST(0x41 AS VARBINARY(MAX)), 5000) AS lob, n * 11 AS tail "
        "FROM (VALUES (1),(2)) AS t(n) ORDER BY n");

    SQLINTEGER n = -1;
    unsigned char buf[32] = {};
    SQLINTEGER tail = -1;
    SQLLEN nInd = 0, ind = 0, tailInd = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &n, sizeof(n), &nInd), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 3, SQL_C_SLONG, &tail, sizeof(tail), &tailInd),
                  SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
    EXPECT_EQ(11, tail) << "the column after a refused LOB must still decode";

    // And the next row too: a drain that stopped short would misread it.
    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_EQ(2, n);
    EXPECT_EQ(22, tail);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}
