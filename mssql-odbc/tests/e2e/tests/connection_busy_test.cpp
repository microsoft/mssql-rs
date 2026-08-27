// Copyright (c) Microsoft Corporation. All rights reserved.
// connection_busy_test.cpp  –  E2E tests for the wire-state connection-busy
// gate (AB#47508).
//
// msodbcsql releases a connection once the wire has drained a statement's
// result set — even while the ODBC-level cursor itself stays open — rather
// than holding it for the cursor's whole lifetime. A fully-bound fetch of the
// last row, or an SQLGetData reaching the last column, both count: either one
// lets the driver peek one token past the row and discover the batch is
// done. This suite locks in that same wire-state behavior for mssql-odbc.
//
// A and B are two HSTMTs on one HDBC throughout, matching the review's
// parity table (PR #383 / microsoft/mssql-rs#399 has the full 12-row table;
// this file covers the six rows whose verdict this PR's fix actually
// changes — 1, 1b, 2, 3, 4, 4b). None of these need
// SKIP_IF_COMPARING_MSODBCSQL(): every assertion here also holds for
// msodbcsql 18, confirmed empirically during review by running this same
// probe shape against both drivers.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <string>

class ConnectionBusyLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    static SQLRETURN Run(SQLHSTMT hstmt, const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(hstmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }
};

// Row 1: a fully-bound single-column fetch of A's only row must release the
// connection immediately — the peek past that row finds the terminating
// DONE — so B can execute on the same connection without seeing HY000
// "Connection is busy". A's cursor itself stays open; only the
// connection-level claim is released.
TEST_F(ConnectionBusyLiveTest, BoundColumnFetchOfOnlyRowReleasesConnectionForOtherStatement) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Run(stmt_, "SELECT 1"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, value);

    EXPECT_TRUE(SQL_SUCCEEDED(Run(b, "SELECT 2")))
        << "B must not see the connection as busy once A's only row is delivered: "
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, b);
}

// Row 1b: same as above, but with an explicit second SQLFetch(A) — which
// takes the StmtState::result_set_exhausted fast path and returns
// SQL_NO_DATA without touching the connection — inserted before B executes.
// Confirms the fast path leaves the already-released claim released rather
// than reclaiming it.
TEST_F(ConnectionBusyLiveTest, ExplicitNoDataFetchStillLeavesConnectionReleased) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Run(stmt_, "SELECT 1"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_NO_DATA, SQLFetch(stmt_));

    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

// Row 2: the SQLGetData path releases the connection the same way the bound
// path does, once every column of the row has been read — GetData on the
// *last* column peeks past the row, same trigger as SQLFetch's bound-column
// fill loop.
TEST_F(ConnectionBusyLiveTest, GetDataThroughLastColumnReleasesConnectionForOtherStatement) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Run(stmt_, "SELECT 1 AS c1, 2 AS c2"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER c1 = 0, c2 = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &c1, sizeof(c1), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &c2, sizeof(c2), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, c1);
    EXPECT_EQ(2, c2);

    EXPECT_TRUE(SQL_SUCCEEDED(Run(b, "SELECT 2")))
        << "B must not see the connection as busy once A's last column is read via SQLGetData: "
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, b);
}

// Row 3: closing A's already-exhausted-but-open cursor (SQLCloseCursor) while
// B is mid-fetch on the same connection must not touch B's live client.
// Before AB#47508's fix, drain_and_release took dbc_state.client
// unconditionally, so this could steal and corrupt whichever statement had
// since claimed the connection (main: "B broken").
TEST_F(ConnectionBusyLiveTest, ClosingExhaustedCursorDoesNotDisturbAnotherStatementsLiveFetch) {
    SQLHSTMT b = AllocStmt();

    // A: exhaust via SQLGetData (row 2's precondition), releasing the connection.
    ASSERT_SQL_OK(Run(stmt_, "SELECT 1 AS c1, 2 AS c2"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER c1 = 0, c2 = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &c1, sizeof(c1), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &c2, sizeof(c2), nullptr),
                  SQL_HANDLE_STMT, stmt_);

    // B claims the now-idle connection and reads its first row.
    ASSERT_SQL_OK(Run(b, "SELECT 10 UNION ALL SELECT 20"), SQL_HANDLE_STMT, b);
    ASSERT_SQL_OK(SQLFetch(b), SQL_HANDLE_STMT, b);
    SQLINTEGER first = 0;
    ASSERT_SQL_OK(SQLGetData(b, 1, SQL_C_SLONG, &first, sizeof(first), nullptr),
                  SQL_HANDLE_STMT, b);
    EXPECT_EQ(10, first);

    // A already released its claim, so closing its cursor now must be a
    // no-op on the wire — it must not steal B's client mid-stream.
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // B's stream must be untouched: its second row is still readable.
    ASSERT_SQL_OK(SQLFetch(b), SQL_HANDLE_STMT, b);
    SQLINTEGER second = 0;
    ASSERT_SQL_OK(SQLGetData(b, 1, SQL_C_SLONG, &second, sizeof(second), nullptr),
                  SQL_HANDLE_STMT, b);
    EXPECT_EQ(20, second);
}

// Row 4: SQLMoreResults on a statement whose result set — and whole batch —
// is already known exhausted (StmtState::batch_exhausted) must report
// SQL_NO_DATA without touching the connection at all, matching msodbcsql
// (whose SQLMoreResults has no busy check of its own). Before this fix
// (AB#47508 item 11) this fell through to the ordinary busy check and
// returned SQL_ERROR/HY000 instead once B had claimed the connection.
TEST_F(ConnectionBusyLiveTest, MoreResultsOnAlreadyExhaustedStatementReturnsNoDataWithoutTouchingConnection) {
    SQLHSTMT b = AllocStmt();

    // A: exhaust via SQLGetData (row 2's precondition), releasing the connection.
    ASSERT_SQL_OK(Run(stmt_, "SELECT 1 AS c1, 2 AS c2"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER c1 = 0, c2 = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &c1, sizeof(c1), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &c2, sizeof(c2), nullptr),
                  SQL_HANDLE_STMT, stmt_);

    // B claims the now-idle connection and starts its own fetch.
    ASSERT_SQL_OK(Run(b, "SELECT 20"), SQL_HANDLE_STMT, b);
    ASSERT_SQL_OK(SQLFetch(b), SQL_HANDLE_STMT, b);

    // A's SQLMoreResults must answer from its own already-known state,
    // rather than contending with B for the connection.
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));

    // B's in-progress fetch must be completely undisturbed.
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLGetData(b, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, b);
    EXPECT_EQ(20, value);
}

// Row 4b: same as above, reached via the bound-column fetch path (row 1b's
// precondition) instead of SQLGetData, to also cover the SQLFetch-driven
// release path into SQLMoreResults's fast path.
TEST_F(ConnectionBusyLiveTest, MoreResultsAfterBoundFetchExhaustionReturnsNoDataWithoutTouchingConnection) {
    SQLHSTMT b = AllocStmt();

    // A: exhaust via a bound-column fetch plus an explicit NO_DATA fetch
    // (row 1b's precondition), releasing the connection.
    ASSERT_SQL_OK(Run(stmt_, "SELECT 1"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value_a = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value_a, sizeof(value_a), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_NO_DATA, SQLFetch(stmt_));

    // B claims the now-idle connection and starts its own fetch.
    ASSERT_SQL_OK(Run(b, "SELECT 20"), SQL_HANDLE_STMT, b);
    ASSERT_SQL_OK(SQLFetch(b), SQL_HANDLE_STMT, b);

    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));

    SQLINTEGER value_b = 0;
    ASSERT_SQL_OK(SQLGetData(b, 1, SQL_C_SLONG, &value_b, sizeof(value_b), nullptr),
                  SQL_HANDLE_STMT, b);
    EXPECT_EQ(20, value_b);
}
