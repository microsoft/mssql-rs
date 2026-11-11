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

// Plain batches expose a failed statement as its own SQLMoreResults boundary;
// later statements remain reachable after the error (msodbcsql B1 parity).
ODBC_TEST_F_MODES(MoreResultsLiveTest, BatchInfoErrorInfoHaveSeparateBoundaries,
                  odbc_concurrency::kAllModes) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SET NOCOUNT OFF; RAISERROR('i1',0,1); RAISERROR('e16',16,1); RAISERROR('i2',0,1);");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01000");

    rc = SQLMoreResults(stmt_);
    ASSERT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "42000");

    rc = SQLMoreResults(stmt_);
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01000");
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
}

// A batch whose failing first statement is followed by another server error
// keeps that later error for the next SQLMoreResults call (msodbcsql B3 parity).
// unixODBC answers SQLMoreResults after a failed SQLExecDirect with
// SQL_NO_DATA without calling either driver (measured in CI); the Windows
// Driver Manager passes it through.
ODBC_TEST_F_MODES(MoreResultsLiveTest, BatchContinuesAfterInitialStatementError,
                  odbc_concurrency::kAllModes) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SET NOCOUNT ON; EXEC('RAISERROR(''e16'',16,1); DECLARE @x int = 1/0;')");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "42000");

#ifdef _WIN32
    rc = SQLMoreResults(stmt_);
    ASSERT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22012");
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
#else
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
#endif
}

// A statement that fails after returning a row keeps the batch: the next fetch
// reports the error, the one after returns SQL_NO_DATA, and SQLMoreResults
// reaches the next statement (msodbcsql 18.7.1.1).
ODBC_TEST_F_MODES(MoreResultsLiveTest, BoundFetchReportsAStatementErrorAfterItsRow,
                  odbc_concurrency::kAllModes) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SET NOCOUNT ON; SELECT n, 1/(2-n) AS q FROM (VALUES(1),(2),(3)) t(n); SELECT 42 AS v;");
    SQLINTEGER n = 0;
    SQLINTEGER q = 0;
    SQLLEN n_ind = 0;
    SQLLEN q_ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &n, 0, &n_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &q, 0, &q_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(1, n);
    ASSERT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22012");
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));

    ASSERT_EQ(SQL_SUCCESS, SQLMoreResults(stmt_));
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(42, n);
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    SQLFreeStmt(stmt_, SQL_UNBIND);
}

// A block fetch returns the rows before a statement's error with SQL_SUCCESS
// and leaves the error for the next fetch, which fetches no rows; the batch
// continues (msodbcsql 18.7.1.1). The rowset is sized after execution so
// msodbcsql streams the result, as this driver does.
ODBC_TEST_F_MODES(MoreResultsLiveTest, BlockFetchReturnsRowsBeforeAStatementError,
                  odbc_concurrency::kAllModes) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SET NOCOUNT ON; SELECT n, 1/(2-n) AS q FROM (VALUES(1),(2),(3)) t(n); SELECT 42 AS v;");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    constexpr SQLULEN rowset = 10;
    SQLINTEGER n[rowset] = {};
    SQLINTEGER q[rowset] = {};
    SQLLEN n_ind[rowset] = {};
    SQLLEN q_ind[rowset] = {};
    SQLUSMALLINT status[rowset] = {};
    SQLULEN fetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(rowset), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, n, 0, n_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, q, 0, q_ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(1u, fetched);
    EXPECT_EQ(1, n[0]);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[0]);
    EXPECT_EQ(SQL_ROW_NOROW, status[1]);
    ASSERT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22012");
    EXPECT_EQ(0u, fetched);
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));

    ASSERT_EQ(SQL_SUCCESS, SQLMoreResults(stmt_));
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(1u, fetched);
    EXPECT_EQ(42, n[0]);
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    SQLFreeStmt(stmt_, SQL_UNBIND);
}

// Once a failed last statement's completion is read, the connection is free,
// including for a statement that failed inside EXEC or sp_executesql. A
// statement whose SQLExecDirect failed runs again; after SQLMoreResults reports
// the failure, another statement runs. A later statement still keeps the batch
// (msodbcsql 18.7.1.1).
ODBC_TEST_F_MODES(MoreResultsLiveTest, FailedLastStatementReleasesTheConnection,
                  odbc_concurrency::kAllModes) {
    const SqlTString reuse = ODBCTestUtils::ToSqlTStr("SELECT 1");
    for (const char* text : {"RAISERROR('reuse_probe',16,1);",
                             "SET NOCOUNT ON; EXEC('RAISERROR(''reuse_probe'',16,1)');",
                             "EXEC sp_executesql N'RAISERROR(''reuse_probe'',16,1)';"}) {
        SCOPED_TRACE(text);
        SqlTString sql = ODBCTestUtils::ToSqlTStr(text);
        ASSERT_EQ(SQL_ERROR, SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "42000");
        ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(reuse.c_str()), SQL_NTS),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }

    // A Driver Manager may keep the statement's cursor state after
    // SQLMoreResults fails, so the release is shown through another statement.
    SQLHSTMT other = AllocStmt();
    ASSERT_NE(nullptr, other);
    for (const char* text : {"SELECT 1; RAISERROR('reuse_probe',16,1);",
                             "SELECT 1; EXEC('RAISERROR(''reuse_probe'',16,1)');"}) {
        SCOPED_TRACE(text);
        SqlTString sql = ODBCTestUtils::ToSqlTStr(text);
        ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_EQ(SQL_ERROR, SQLMoreResults(stmt_));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "42000");
        ASSERT_SQL_OK(SQLExecDirect(other, const_cast<SQLTCHAR*>(reuse.c_str()), SQL_NTS),
                      SQL_HANDLE_STMT, other);
        ASSERT_SQL_OK(SQLCloseCursor(other), SQL_HANDLE_STMT, other);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }

    SqlTString pending = ODBCTestUtils::ToSqlTStr("RAISERROR('reuse_probe',16,1); SELECT 2;");
    ASSERT_EQ(SQL_ERROR, SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(pending.c_str()), SQL_NTS));
    EXPECT_EQ(SQL_ERROR, SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(reuse.c_str()), SQL_NTS));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
}

// Inside sp_executesql a failed row set ends on DONEINPROC, and the
// procedure's RETURNSTATUS and DONEPROC still end the batch. Once the error is
// reported the connection is free and the cursor stays open, so the next fetch
// returns SQL_NO_DATA. A row set failing before its first row fails
// SQLExecDirect and leaves no cursor (msodbcsql 18.07.0001).
ODBC_TEST_F_MODES(MoreResultsLiveTest, FailedProcedureRowSetReleasesTheConnection,
                  odbc_concurrency::kAllModes) {
    const SqlTString reuse = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLHSTMT other = AllocStmt();
    ASSERT_NE(nullptr, other);
    SqlTString after_row = ODBCTestUtils::ToSqlTStr(
        "EXEC sp_executesql N'SELECT n, 1/(2-n) AS q FROM (VALUES(1),(2),(3)) t(n)';");
    SQLINTEGER n = 0;
    SQLINTEGER q = 0;
    SQLLEN n_ind = 0;
    SQLLEN q_ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &n, 0, &n_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &q, 0, &q_ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(after_row.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(1, n);
    ASSERT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22012");
    ASSERT_SQL_OK(SQLExecDirect(other, const_cast<SQLTCHAR*>(reuse.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, other);
    ASSERT_SQL_OK(SQLCloseCursor(other), SQL_HANDLE_STMT, other);
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLFreeStmt(stmt_, SQL_UNBIND);

    SqlTString first_row = ODBCTestUtils::ToSqlTStr(
        "EXEC sp_executesql N'SELECT n, 1/(1-n) AS q FROM (VALUES(1),(2)) t(n)';");
    ASSERT_EQ(SQL_ERROR, SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(first_row.c_str()), SQL_NTS));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22012");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(reuse.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// For diagnostics returned by one execution call, a mapped server warning
// precedes the subsequent unmapped server error.
ODBC_TEST_F_MODES(MoreResultsLiveTest, ServerDiagnosticsUseMsodbcsqlPriorityOrder,
                  odbc_concurrency::kAllModes) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SET NOCOUNT ON; EXEC('DECLARE @x int; SELECT @x = SUM(v) FROM "
        "(VALUES(CAST(NULL AS int)),(1)) t(v); RAISERROR(''e16'',16,1);')");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_EQ(SQL_ERROR, rc);

    const auto read_state = [&](SQLSMALLINT record, SQLINTEGER expected_native,
                                const char* expected_state) {
        SQLWCHAR state[6] = {};
        SQLINTEGER native = -1;
        SQLWCHAR message[512] = {};
        SQLSMALLINT message_len = 0;
        SQLRETURN diag_rc = SQLGetDiagRecW(
            SQL_HANDLE_STMT, stmt_, record, state, &native, message,
            static_cast<SQLSMALLINT>(sizeof(message) / sizeof(message[0])), &message_len);
        ASSERT_TRUE(diag_rc == SQL_SUCCESS || diag_rc == SQL_SUCCESS_WITH_INFO);
        std::string actual_state;
        for (int i = 0; i < 5; ++i) {
            actual_state.push_back(static_cast<char>(state[i]));
        }
        EXPECT_EQ(expected_state, actual_state);
        EXPECT_EQ(expected_native, native);
    };
    read_state(1, 8153, "01003");
    read_state(2, 50000, "42000");
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
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
