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
// A and B are two HSTMTs on one HDBC throughout, matching the review's full
// 12-row parity table (see microsoft/mssql-rs#399 for the table and its
// history): rows 1, 1b, 2, 3, 4, 4b are the ones whose verdict this PR's fix
// actually changes; rows 3b, 5, 6, 7, 8, 9 keep their verdict unchanged from
// `main` and are included for completeness/regression coverage. None of
// these need SKIP_IF_COMPARING_MSODBCSQL(): every assertion here also holds
// for msodbcsql 18, confirmed empirically during review by running this same
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

    static SQLRETURN Prepare(SQLHSTMT hstmt, const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(hstmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    static SQLRETURN BindInt(SQLHSTMT hstmt, SQLUSMALLINT ordinal,
                             SQLINTEGER* value, SQLLEN* indicator) {
        return SQLBindParameter(hstmt, ordinal, SQL_PARAM_INPUT, SQL_C_SLONG,
                                SQL_INTEGER, 0, 0, value, 0, indicator);
    }
};

// A first execution of a prepared statement uses sp_prepexec. Its output
// handle trails the row result on the wire, but it is part of the same RPC
// response and must not keep the connection claimed after the only row and
// all its columns have been consumed.
TEST_F(ConnectionBusyLiveTest, PreparedNullParameterBoundFetchReleasesConnection) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Prepare(stmt_, "SELECT ?"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER parameter = 0;
    SQLLEN parameter_ind = SQL_NULL_DATA;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &parameter, &parameter_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 0;
    SQLLEN value_ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &value_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NULL_DATA, value_ind);

    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

TEST_F(ConnectionBusyLiveTest, PreparedParametersGetDataThroughLastColumnReleaseConnection) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Prepare(stmt_, "SELECT ?, ?"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER first_parameter = 1;
    SQLINTEGER second_parameter = 2;
    SQLLEN first_ind = 0;
    SQLLEN second_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &first_parameter, &first_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindInt(stmt_, 2, &second_parameter, &second_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER first = 0;
    SQLINTEGER second = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &first, sizeof(first), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &second, sizeof(second), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, first);
    EXPECT_EQ(2, second);

    EXPECT_SQL_OK(Run(b, "SELECT 3"), SQL_HANDLE_STMT, b);
}

// fetchall-style consumers issue one final SQLFetch to receive SQL_NO_DATA.
// That completion must drain the trailing RPC tokens and release the claim.
TEST_F(ConnectionBusyLiveTest, PreparedParameterFetchToNoDataReleasesConnection) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Prepare(stmt_, "SELECT ?"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER parameter = -42;
    SQLLEN parameter_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &parameter, &parameter_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-42, value);
    ASSERT_EQ(SQL_NO_DATA, SQLFetch(stmt_));

    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

TEST_F(ConnectionBusyLiveTest, PreparedParameterMultiRowReleasesAfterLastRow) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Prepare(stmt_, "SELECT ? FROM (VALUES (1), (2)) AS v(n) ORDER BY n"),
                  SQL_HANDLE_STMT, stmt_);
    SQLINTEGER parameter = 42;
    SQLLEN parameter_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &parameter, &parameter_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, value);
    EXPECT_EQ(SQL_ERROR, Run(b, "SELECT 2"));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, b, "HY000");

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, value);
    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

TEST_F(ConnectionBusyLiveTest, PreparedParameterZeroRowReleasesConnection) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Prepare(stmt_, "SELECT ? WHERE 1 = 0"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER parameter = 42;
    SQLLEN parameter_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &parameter, &parameter_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_NO_DATA, SQLFetch(stmt_));

    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

// SQLExecDirect with bound parameters uses sp_executesql rather than the
// prepared sp_prepexec path. Its RPC completion tokens must obey the same
// release rule after the single row is consumed.
TEST_F(ConnectionBusyLiveTest, ExecDirectParameterBoundFetchReleasesConnection) {
    SQLHSTMT b = AllocStmt();

    SQLINTEGER parameter = 42;
    SQLLEN parameter_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &parameter, &parameter_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(Run(stmt_, "SELECT ?"), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, value);

    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

TEST_F(ConnectionBusyLiveTest, PreparedParameterCloseWithoutFetchReleasesConnection) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Prepare(stmt_, "SELECT ?"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER parameter = 42;
    SQLLEN parameter_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &parameter, &parameter_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

TEST_F(ConnectionBusyLiveTest, ExecDirectParameterCloseWithoutFetchReleasesConnection) {
    SQLHSTMT b = AllocStmt();

    SQLINTEGER parameter = 42;
    SQLLEN parameter_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &parameter, &parameter_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(Run(stmt_, "SELECT ?"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

// Mirrors msodbcsql MARSODBC TCTestAPIs variation 2: a parameterized direct
// query with two rowsets must retain the second rowset for SQLMoreResults.
TEST_F(ConnectionBusyLiveTest, ExecDirectParametersPreserveSecondResultSet) {
    SQLHSTMT b = AllocStmt();

    SQLINTEGER first_parameter = 41;
    SQLINTEGER second_parameter = 42;
    SQLLEN first_ind = 0;
    SQLLEN second_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &first_parameter, &first_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindInt(stmt_, 2, &second_parameter, &second_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(Run(stmt_, "SELECT ?; SELECT ?"), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(41, value);

    EXPECT_EQ(SQL_ERROR, Run(b, "SELECT 20"));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, b, "HY000");

    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, value);
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));

    EXPECT_SQL_OK(Run(b, "SELECT 20"), SQL_HANDLE_STMT, b);
}

// Mirrors msodbcsql PrepareEx variation 49, with parameters added to keep the
// sp_prepexec completion tail in the scenario. Re-execution also proves that
// the handle captured after the first RPC remains usable by sp_execute.
TEST_F(ConnectionBusyLiveTest, PreparedParametersPreserveMultipleResultsAcrossReuse) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Prepare(stmt_, "SELECT ?; SELECT ?"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER first_parameter = 41;
    SQLINTEGER second_parameter = 42;
    SQLLEN first_ind = 0;
    SQLLEN second_ind = 0;
    ASSERT_SQL_OK(BindInt(stmt_, 1, &first_parameter, &first_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindInt(stmt_, 2, &second_parameter, &second_ind), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    for (int execution = 0; execution < 2; ++execution) {
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(41, value);
        ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(42, value);
        EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    }

    EXPECT_SQL_OK(Run(b, "SELECT 20"), SQL_HANDLE_STMT, b);
}

// Cursor wrappers may free an executed statement directly rather than fetching
// or calling SQLCloseCursor first. Freeing it must drain the rowset and the
// sp_prepexec tail before a second statement uses the connection.
TEST_F(ConnectionBusyLiveTest, FreeUnfetchedPreparedParameterStatementReleasesConnection) {
    SQLHSTMT a = AllocStmt();
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Prepare(a, "SELECT ?"), SQL_HANDLE_STMT, a);
    SQLINTEGER parameter = 1;
    SQLLEN parameter_ind = 0;
    ASSERT_SQL_OK(BindInt(a, 1, &parameter, &parameter_ind), SQL_HANDLE_STMT, a);
    ASSERT_SQL_OK(SQLExecute(a), SQL_HANDLE_STMT, a);
    FreeStmt(a);

    EXPECT_SQL_OK(Run(b, "SELECT 2"), SQL_HANDLE_STMT, b);
}

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

// Row 3b: SQLGetData on the *first* of two rows in a result set must NOT
// release the connection — the peek finds a second row still pending, so
// the connection correctly stays busy. Same trigger as row 2, but with a
// result set that isn't actually exhausted yet: distinguishes "every column
// of the current row was read" from "the whole result set is done" (only
// the latter releases). Matches msodbcsql exactly.
TEST_F(ConnectionBusyLiveTest, GetDataOfFirstRowLeavesConnectionBusyWhenAnotherRowIsStillPending) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Run(stmt_, "SELECT 10 UNION ALL SELECT 20"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(10, value);

    EXPECT_EQ(SQL_ERROR, Run(b, "SELECT 2"))
        << "a pending second row in A's result set must keep the connection busy";
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, b, "HY000");
}

// Row 5: a further result set pending in the same batch must keep the
// connection busy for B, while A's own SQLMoreResults still genuinely
// advances and reads the next result set. Unaffected by this PR's fix
// (identical on main and msodbcsql); included for completeness.
TEST_F(ConnectionBusyLiveTest, PendingSecondResultSetKeepsConnectionBusyAndAdvancesCorrectly) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Run(stmt_, "SELECT 1; SELECT 2"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, value);

    // A further result set is still pending in the batch — B must see busy.
    EXPECT_EQ(SQL_ERROR, Run(b, "SELECT 20"));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, b, "HY000");

    // A can still genuinely advance to, and read, its own second result set.
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, value);
}

// Row 6: binding only a prefix of the row's columns and never reading the
// rest via SQLGetData is a deliberate scope limit (documented in
// exec_common.rs / README.md) — peeking early would discard the still
// legally-retrievable unbound column, so the connection correctly stays
// busy here. Parity with msodbcsql, not a gap.
TEST_F(ConnectionBusyLiveTest, PrefixBoundFetchLeavesConnectionBusy) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(Run(stmt_, "SELECT 1 AS c1, 2 AS c2"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER c1 = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &c1, sizeof(c1), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, c1);
    // c2 is never bound or read via SQLGetData.

    EXPECT_EQ(SQL_ERROR, Run(b, "SELECT 2"))
        << "an unread trailing column must keep the connection busy";
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, b, "HY000");
}

// Row 7: same shape as row 6 (an unread trailing column), but with a large
// nvarchar(max) column — the SQLGetData PLP/streaming completion path is
// not wired to the new peek in this PR, so an unread streamed column also
// keeps the connection busy. Parity with msodbcsql, not a gap.
TEST_F(ConnectionBusyLiveTest, UnreadTrailingNvarcharMaxColumnLeavesConnectionBusy) {
    SQLHSTMT b = AllocStmt();

    ASSERT_SQL_OK(
        Run(stmt_, "SELECT 1 AS c1, CAST(REPLICATE('x', 10000) AS nvarchar(max)) AS c2"),
        SQL_HANDLE_STMT, stmt_);
    SQLINTEGER c1 = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &c1, sizeof(c1), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, c1);
    // c2 (nvarchar(max)) is never bound or read via SQLGetData.

    EXPECT_EQ(SQL_ERROR, Run(b, "SELECT 2"))
        << "an unread nvarchar(max) column must keep the connection busy";
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, b, "HY000");
}

// Row 8: a leading PRINT before a zero-row SELECT surfaces its message on
// the SQLExecDirect call itself — the driver reads past PRINT's INFO/DONE
// tokens on the way to the SELECT's own COLMETADATA (the first row-bearing
// stopping point), so the message is already captured by the time that same
// execute call returns. Unaffected by this PR's fix; included for
// completeness.
TEST_F(ConnectionBusyLiveTest, LeadingPrintMessageSurfacesOnExecDirect) {
    SQLRETURN rc = Run(stmt_, "PRINT 'row8 leading print'; SELECT 1 WHERE 1 = 0");
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc)
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    std::string msg = ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    EXPECT_NE(std::string::npos, msg.find("row8 leading print"));
}

// Row 9: the same PRINT reordered *after* a zero-row SELECT surfaces its
// message on the SQLMoreResults call that advances past the SELECT — the
// PRINT statement hasn't been read off the wire yet at execute time (the
// driver stops at the SELECT's own COLMETADATA), so its message can only be
// captured once SQLMoreResults reaches it. Unaffected by this PR's fix;
// included for completeness.
TEST_F(ConnectionBusyLiveTest, TrailingPrintMessageSurfacesOnMoreResults) {
    ASSERT_SQL_OK(Run(stmt_, "SELECT 1 WHERE 1 = 0; PRINT 'row9 trailing print'"),
                  SQL_HANDLE_STMT, stmt_);

    // The zero-row SELECT's cursor is open but empty.
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));

    SQLRETURN rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc)
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    std::string msg = ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    EXPECT_NE(std::string::npos, msg.find("row9 trailing print"));
}
