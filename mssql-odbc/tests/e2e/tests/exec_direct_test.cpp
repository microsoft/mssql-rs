// Copyright (c) Microsoft Corporation. All rights reserved.
// exec_direct_test.cpp  –  E2E tests for SQLExecDirectW.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <string>

static std::string GetDiagMessageForRecord(SQLHSTMT stmt, SQLSMALLINT recordNumber, SQLINTEGER* nativeError = nullptr) {
    SQLTCHAR state[8] = {};
    SQLINTEGER native = 0;
    SQLTCHAR message[512] = {};
    SQLSMALLINT messageLen = 0;

    SQLRETURN rc = SQLGetDiagRec(SQL_HANDLE_STMT, stmt, recordNumber,
                                 state, &native,
                                 message,
                                 static_cast<SQLSMALLINT>(sizeof(message) / sizeof(SQLTCHAR)),
                                 &messageLen);
    EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO);
    if (nativeError != nullptr) {
        *nativeError = native;
    }
    return ODBCTestUtils::ToNarrow(SqlTString(message));
}

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
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
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

TEST_F(ExecDirectLiveTest, InfoMessagesSurfaceAsDiagnostics) {
    // Statement-wise navigation (msodbcsql parity): each no-row statement is its
    // own result. The PRINT surfaces on SQLExecDirect; the low-severity
    // RAISERROR surfaces on the next SQLMoreResults; then the batch ends.
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "PRINT N'odbc info one'; RAISERROR(N'odbc info two', 10, 1) WITH NOWAIT;");

    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, rc);

    SQLINTEGER native1 = 0;
    std::string message1 = GetDiagMessageForRecord(stmt_, 1, &native1);
    EXPECT_EQ(0, native1);
    EXPECT_NE(std::string::npos, message1.find("odbc info one"));

    // Advance to the RAISERROR statement's result; its message surfaces here.
    rc = SQLMoreResults(stmt_);
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, rc);

    SQLINTEGER native2 = 0;
    std::string message2 = GetDiagMessageForRecord(stmt_, 1, &native2);
    EXPECT_EQ(50000, native2);
    EXPECT_NE(std::string::npos, message2.find("odbc info two"));

    // No more statements.
    rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// AB#47535: a PRINT reached only after variable assignments must still surface
// its message on SQLExecDirect itself. The assignments carry a SQLSELECT-tagged
// DONE_COUNT; when those were treated as update counts the batch stopped on the
// assignment and SQLExecDirect returned plain SQL_SUCCESS, so callers that only
// read diagnostics on SQL_SUCCESS_WITH_INFO (mssql-python's cursor.messages)
// saw nothing at all.
TEST_F(ExecDirectLiveTest, PrintAfterAssignmentsSurfacesOnExecute) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "DECLARE @msg VARCHAR(MAX);"
        " SET @msg = REPLICATE(CAST('a' AS VARCHAR(MAX)), 2047);"
        " PRINT @msg;");

    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO, rc);

    SQLINTEGER native = 0;
    std::string message = GetDiagMessageForRecord(stmt_, 1, &native);
    EXPECT_EQ(0, native);
    EXPECT_NE(std::string::npos, message.find("aaaaaaaaaa"));

    // The assignments produced no navigable result of their own.
    SQLLEN row_count = 0;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &row_count), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, row_count);
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
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

// --- Regression: variable assignment must not be exposed as a leading result ---
//
// SQL Server compiles variable assignment (DECLARE with an initializer, SET, or
// SELECT @var = col) as a SQLSELECT command that still carries DONE_COUNT. The
// driver must not turn that into an update-count result, otherwise SQLExecDirect
// stops on a phantom 0-column result and an immediate SQLFetch fails with 24000.
// msodbcsql behaves the same way, so these run unguarded against both drivers.

namespace {

// Executes `sql`, asserting it opens a row set directly, and returns the first
// column of the first row as text.
std::string ExecAndReadFirstCell(SQLHSTMT stmt, const char* sql) {
    SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
    SQLRETURN rc = SQLExecDirect(stmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt);

    SQLSMALLINT cols = -1;
    rc = SQLNumResultCols(stmt, &cols);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt);
    EXPECT_EQ(1, cols) << "expected to be positioned on the row set, not a count";

    SQLLEN row_count = 0;
    rc = SQLRowCount(stmt, &row_count);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt);
    EXPECT_EQ(-1, row_count) << "a row set must report SQLRowCount -1";

    rc = SQLFetch(stmt);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt);

    SQLCHAR buf[32] = {0};
    SQLLEN ind = 0;
    rc = SQLGetData(stmt, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt));
    EXPECT_SQL_OK(SQLCloseCursor(stmt), SQL_HANDLE_STMT, stmt);
    return std::string(reinterpret_cast<const char*>(buf));
}

}  // namespace

// DECLARE with an initializer, followed by a SELECT.
TEST_F(ExecDirectLiveTest, DeclareWithInitializerDoesNotPrecedeRowSet) {
    EXPECT_EQ("7", ExecAndReadFirstCell(stmt_, "DECLARE @x int = 1; SELECT 7 AS a;"));
}

// SET assignment is compiled the same way as an initialized DECLARE.
TEST_F(ExecDirectLiveTest, SetAssignmentDoesNotPrecedeRowSet) {
    EXPECT_EQ("8", ExecAndReadFirstCell(stmt_, "DECLARE @x int; SET @x = 1; SELECT 8 AS a;"));
}

// Assignment-SELECT reports the source row count on its DONE token.
TEST_F(ExecDirectLiveTest, AssignmentSelectDoesNotPrecedeRowSet) {
    EXPECT_EQ("9", ExecAndReadFirstCell(stmt_, "DECLARE @x int; SELECT @x = 1; SELECT 9 AS a;"));
}

// Several assignments in a row all collapse into the following row set.
TEST_F(ExecDirectLiveTest, ConsecutiveAssignmentsDoNotPrecedeRowSet) {
    EXPECT_EQ("10",
              ExecAndReadFirstCell(
                  stmt_, "DECLARE @a int = 1; DECLARE @b int = 2; SET @a = 3; SELECT 10 AS a;"));
}

// The reported scenario: DECLARE then EXEC of a stored procedure. An immediate
// SQLFetch must return the procedure's rows without an intervening
// SQLMoreResults.
TEST_F(ExecDirectLiveTest, DeclareThenExecProcOpensProcRowSet) {
    SqlTString create = ODBCTestUtils::ToSqlTStr(
        "CREATE PROCEDURE #decl_proc AS BEGIN SELECT 42 AS answer; END");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(create.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ("42", ExecAndReadFirstCell(stmt_, "DECLARE @out int = 1; EXEC #decl_proc;"));
}

// A batch that is only an assignment must not report a phantom row count.
// msodbcsql reports SQLRowCount -1 here, not 1.
TEST_F(ExecDirectLiveTest, AssignmentOnlyBatchReportsNoRowCount) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("DECLARE @x int = 1;");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT cols = -1;
    rc = SQLNumResultCols(stmt_, &cols);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, cols);

    SQLLEN row_count = 0;
    rc = SQLRowCount(stmt_, &row_count);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, row_count);

    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
}

// Genuine DML counts are unaffected: the UPDATE count is still reported before
// the trailing row set, even when preceded by an assignment.
TEST_F(ExecDirectLiveTest, UpdateCountStillPrecedesRowSetAfterAssignment) {
    SqlTString setup =
        ODBCTestUtils::ToSqlTStr("CREATE TABLE #upd(v int); INSERT INTO #upd VALUES (1),(2);");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(setup.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    // Bounded drain: stop on SQL_ERROR instead of spinning, so a driver bug
    // fails the test rather than hanging the CI leg.
    for (SQLRETURN more = SQLMoreResults(stmt_); more != SQL_NO_DATA;
         more = SQLMoreResults(stmt_)) {
        ASSERT_SQL_OK(more, SQL_HANDLE_STMT, stmt_);
    }

    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "DECLARE @x int = 1; UPDATE #upd SET v = v; SELECT * FROM #upd;");
    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // Positioned on the UPDATE count, not on the assignment and not on the rows.
    SQLSMALLINT cols = -1;
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &cols), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, cols);

    SQLLEN row_count = 0;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &row_count), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, row_count);

    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &cols), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, cols);
    EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A warning raised by the assignment itself puts a message and a SQLSELECT
// count on the same DONE. The message-only result still surfaces, but must
// report SQLRowCount -1 rather than the assignment's count -- or the preceding
// UPDATE's. The UPDATE is what makes the -1 assertion meaningful: without it
// SQLRowCount is still -1 from execution start and the test would pass even if
// a prior count leaked.
TEST_F(ExecDirectLiveTest, AssignmentWarningResultReportsNoRowCount) {
    SqlTString setup =
        ODBCTestUtils::ToSqlTStr("CREATE TABLE #agg(v int); INSERT INTO #agg VALUES (1),(NULL);");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(setup.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    for (SQLRETURN more = SQLMoreResults(stmt_); more != SQL_NO_DATA;
         more = SQLMoreResults(stmt_)) {
        ASSERT_SQL_OK(more, SQL_HANDLE_STMT, stmt_);
    }

    // SELECT @x = MAX(v) over a NULL emits "Null value is eliminated by an
    // aggregate or other SET operation" (msg 8153) on the assignment's DONE.
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "UPDATE #agg SET v = v; DECLARE @x int; SELECT @x = MAX(v) FROM #agg; SELECT 5 AS a;");
    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // The UPDATE's genuine count.
    SQLSMALLINT cols = -1;
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &cols), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, cols);
    SQLLEN row_count = 0;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &row_count), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, row_count);

    // The warning result: no count of its own, and the UPDATE's must not leak.
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &cols), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, cols);
    ASSERT_SQL_OK(SQLRowCount(stmt_, &row_count), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, row_count);

    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &cols), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, cols);
    EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A message-only statement (PRINT) after a DML count reports SQLRowCount -1,
// not the preceding statement's count. Pre-existing shape, pinned here because
// the assignment fix routes a new case onto the same branch.
TEST_F(ExecDirectLiveTest, MessageOnlyResultAfterDmlReportsNoRowCount) {
    SqlTString setup =
        ODBCTestUtils::ToSqlTStr("CREATE TABLE #pr(v int); INSERT INTO #pr VALUES (1),(2);");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(setup.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    for (SQLRETURN more = SQLMoreResults(stmt_); more != SQL_NO_DATA;
         more = SQLMoreResults(stmt_)) {
        ASSERT_SQL_OK(more, SQL_HANDLE_STMT, stmt_);
    }

    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "UPDATE #pr SET v = v; PRINT 'hello'; SELECT 5 AS a;");
    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    SQLLEN row_count = 0;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &row_count), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, row_count);

    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLRowCount(stmt_, &row_count), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, row_count) << "PRINT must not inherit the UPDATE's count";

    SQLSMALLINT cols = -1;
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &cols), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, cols);
    EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SELECT ... INTO reports DONE_COUNT with no CurCmd, which is a genuine update
// count and must survive the SQLSELECT filter.
TEST_F(ExecDirectLiveTest, SelectIntoCountStillReported) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1 AS a INTO #si; SELECT * FROM #si;");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    SQLLEN row_count = 0;
    ASSERT_SQL_OK(SQLRowCount(stmt_, &row_count), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, row_count);

    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLSMALLINT cols = -1;
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &cols), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, cols);
    EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// -------------------------------------------------------------------
// Re-execution without an intervening close
// -------------------------------------------------------------------

// A parameterized DML runs as sp_executesql, whose DONEINPROC always claims
// DONE_MORE because the RPC's own DONEPROC follows it. Taking that at face
// value left a 0-column cursor open and failed every subsequent execute on the
// statement with 24000 — the loop shape every ORM and mssql-python's own
// batch-insert tests use (AB#47531).
TEST_F(ExecDirectLiveTest, ParameterizedDmlIsImmediatelyReExecutable) {
    SqlTString setup = ODBCTestUtils::ToSqlTStr("CREATE TABLE #reexec(i int)");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(setup.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    SqlTString insert = ODBCTestUtils::ToSqlTStr("INSERT INTO #reexec VALUES (?)");
    for (SQLINTEGER value = 1; value <= 3; ++value) {
        SCOPED_TRACE("execute #" + std::to_string(value) +
                     " must not need an intervening SQLCloseCursor");
        SQLINTEGER bound = value;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                       &bound, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(insert.c_str()), SQL_NTS),
                      SQL_HANDLE_STMT, stmt_);

        SQLLEN row_count = 0;
        ASSERT_SQL_OK(SQLRowCount(stmt_, &row_count), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(1, row_count);
    }
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    SqlTString count = ODBCTestUtils::ToSqlTStr("SELECT COUNT(*) FROM #reexec");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(count.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER rows = -1;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &rows, sizeof(rows), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, rows);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The look-ahead that makes the case above work must not swallow a real second
// result: a parameterized statement with anything still pending keeps its
// cursor open and rejects re-execution with 24000, exactly as msodbcsql does.
TEST_F(ExecDirectLiveTest, ParameterizedBatchWithPendingResultsRejectsReExecute) {
    SqlTString setup = ODBCTestUtils::ToSqlTStr("CREATE TABLE #pending(i int)");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(setup.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    SQLINTEGER first = 1;
    SQLINTEGER second = 2;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                   &first, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                   &second, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);

    SqlTString batch = ODBCTestUtils::ToSqlTStr(
        "INSERT INTO #pending VALUES (?); INSERT INTO #pending VALUES (?)");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(batch.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    SqlTString again = ODBCTestUtils::ToSqlTStr("SELECT 1");
    EXPECT_SQL_ERROR(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(again.c_str()), SQL_NTS));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
}
