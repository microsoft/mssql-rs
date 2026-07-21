// Copyright (c) Microsoft Corporation. All rights reserved.
// execute_test.cpp  –  E2E tests for SQLPrepare + SQLBindParameter + SQLExecute.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <cstring>
#include <vector>

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

TEST(ExecuteTest, ExecuteNullHandle) {
    SQLRETURN rc = SQLExecute(SQL_NULL_HSTMT);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

TEST(ExecuteTest, BindParameterNullHandle) {
    SQLCHAR value[] = "x";
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(SQL_NULL_HSTMT, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                    SQL_VARCHAR, 1, 0, value, sizeof(value), &ind);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class PrepareExecuteLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    // Prepare helper.
    SQLRETURN Prepare(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // Bind a narrow (SQL_C_CHAR / varchar) input parameter held in |store|.
    // |store| must outlive the SQLExecute call (bound by reference).
    SQLRETURN BindChar(SQLUSMALLINT param, std::vector<SQLCHAR>& store,
                       SQLLEN& ind) {
        return SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_CHAR,
                                SQL_VARCHAR, store.size(), 0, store.data(),
                                static_cast<SQLLEN>(store.size()), &ind);
    }

    // Read column 1 of the current row as a narrow string.
    std::string GetColumnChar(SQLUSMALLINT col, SQLLEN* ind_out = nullptr) {
        SQLCHAR buf[512] = {0};
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, col, SQL_C_CHAR, buf, sizeof(buf), &ind);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        if (ind_out) {
            *ind_out = ind;
        }
        if (ind == SQL_NULL_DATA) {
            return std::string();
        }
        return std::string(reinterpret_cast<const char*>(buf));
    }
};

// SQLExecute on a statement that was never prepared is a sequence error.
TEST_F(PrepareExecuteLiveTest, ExecuteWithoutPrepareReturnsHy010) {
    SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

// Prepare + execute with no parameters.
TEST_F(PrepareExecuteLiveTest, PrepareExecuteNoParams) {
    ASSERT_SQL_OK(Prepare("SELECT 1"), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLExecute(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", GetColumnChar(1));

    rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// A single character parameter flows through sp_prepare/sp_execute and is
// returned verbatim.
TEST_F(PrepareExecuteLiveTest, SingleCharParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value = {'h', 'e', 'l', 'l', 'o', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("hello", GetColumnChar(1));

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A wide-character parameter binds as nvarchar and round-trips.
TEST_F(PrepareExecuteLiveTest, WideCharParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    // UTF-16 "wide" + NUL terminator.
    SQLWCHAR value[] = {'w', 'i', 'd', 'e', 0};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                    SQL_WVARCHAR, 4, 0, value, sizeof(value), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("wide", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A wide-char parameter bound with an explicit byte-length indicator (not
// SQL_NTS) sends exactly that many bytes. The indicator is a byte count per the
// ODBC spec, so 8 bytes == 4 UTF-16 units.
TEST_F(PrepareExecuteLiveTest, ExplicitLengthWideCharParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    // "wider" (no NUL) with an indicated length of 8 bytes → only "wide".
    SQLWCHAR value[] = {'w', 'i', 'd', 'e', 'r', 0};
    SQLLEN ind = 4 * sizeof(SQLWCHAR);  // 8 bytes = 4 code units
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                    SQL_WVARCHAR, 5, 0, value, sizeof(value), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("wide", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A NULL-indicator parameter produces a SQL NULL result.
TEST_F(PrepareExecuteLiveTest, NullParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value = {'i', 'g', 'n', 'o', 'r', 'e', 'd', '\0'};
    SQLLEN ind = SQL_NULL_DATA;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLLEN ind_out = 0;
    GetColumnChar(1, &ind_out);
    EXPECT_EQ(SQL_NULL_DATA, ind_out);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Multiple parameters are bound in order and substituted positionally,
// including a NULL-indicator parameter mixed in with non-NULL ones.
TEST_F(PrepareExecuteLiveTest, MultipleParams) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(? AS INT) + CAST(? AS INT) AS s, ? AS n"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> a = {'3', '\0'};
    std::vector<SQLCHAR> b = {'4', '\0'};
    std::vector<SQLCHAR> c = {'i', 'g', 'n', 'o', 'r', 'e', 'd', '\0'};
    SQLLEN ind_a = SQL_NTS, ind_b = SQL_NTS, ind_c = SQL_NULL_DATA;
    ASSERT_SQL_OK(BindChar(1, a, ind_a), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindChar(2, b, ind_b), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindChar(3, c, ind_c), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("7", GetColumnChar(1));

    SQLLEN ind_out = 0;
    GetColumnChar(2, &ind_out);
    EXPECT_EQ(SQL_NULL_DATA, ind_out);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A parameter used in a WHERE clause filters rows correctly.
TEST_F(PrepareExecuteLiveTest, ParamInWhereClause) {
    ExecDirect("CREATE TABLE #people (id INT, name VARCHAR(50))");
    ExecDirect("INSERT INTO #people VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')");

    ASSERT_SQL_OK(Prepare("SELECT id FROM #people WHERE name = ?"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> name = {'b', 'o', 'b', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, name, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetColumnChar(1));

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Re-executing a prepared statement with a changed parameter buffer reuses the
// cached server handle (sp_execute) and reflects the new value. The buffer is
// read by reference at each SQLExecute.
TEST_F(PrepareExecuteLiveTest, ReExecuteWithNewParamValue) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    // Fixed-capacity buffer so its address is stable across re-execute — the
    // driver reads the bound buffer by reference at each SQLExecute, so it must
    // not be reallocated between calls.
    std::vector<SQLCHAR> value(32, 0);
    std::memcpy(value.data(), "first", 6);
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("first", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Overwrite the SAME buffer in place (no reallocation) and re-execute.
    std::memset(value.data(), 0, value.size());
    std::memcpy(value.data(), "second", 7);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("second", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Prepare once, execute many times with different parameter values. The plan is
// prepared on the first execute (sp_prepexec) and reused via sp_execute on every
// subsequent call; the bound buffer is re-read by reference each time.
TEST_F(PrepareExecuteLiveTest, PrepareOnceExecuteMany) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(? AS INT) * 2 AS v"), SQL_HANDLE_STMT, stmt_);

    // Fixed-capacity buffer bound once; its address must stay stable across the
    // re-executes since the driver reads it by reference at each SQLExecute.
    std::vector<SQLCHAR> value(16, 0);
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    for (int i = 1; i <= 5; ++i) {
        std::string in = std::to_string(i);
        std::memset(value.data(), 0, value.size());
        std::memcpy(value.data(), in.c_str(), in.size() + 1);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(std::to_string(i * 2), GetColumnChar(1));
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

// Rebinding a parameter after an execute must not corrupt the connection: the
// second execute returning the new value proves the statement remains usable
// after the rebind (which internally orphans the cached handle for release).
// This is a behavioral check only — that sp_unprepare + sp_prepexec actually
// fire is asserted by the unit test rebind_invalidates_cached_prepared_handle;
// both the reused and re-prepared paths return the same value, so this test
// alone cannot distinguish them.
//
// Benefits-from-mock-tds: a mock TDS server could assert sp_unprepare +
// sp_prepexec actually fired, which the returned value alone cannot.
TEST_F(PrepareExecuteLiveTest, RebindReleasesPriorHandleAndReprepares) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> first = {'f', 'i', 'r', 's', 't', '\0'};
    SQLLEN ind1 = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, first, ind1), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("first", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Rebind parameter 1 to a different buffer — invalidates the prepared plan.
    std::vector<SQLCHAR> second = {'s', 'e', 'c', 'o', 'n', 'd', '\0'};
    SQLLEN ind2 = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, second, ind2), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("second", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Re-preparing a statement with new text must not corrupt the connection: the
// execute of the new plan returning its value proves the statement remains
// usable after the re-prepare (which internally orphans the prior handle for
// release). This is a behavioral check only — that the prior handle is actually
// released is asserted by the unit test reprepare_orphans_prior_handle_for_unprepare.
//
// Benefits-from-mock-tds: a mock TDS server could assert the prior handle's
// sp_unprepare / piggybacked @handle drop actually fired.
TEST_F(PrepareExecuteLiveTest, ReprepareReleasesPriorHandle) {
    ASSERT_SQL_OK(Prepare("SELECT 1 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Re-prepare with different text — orphans the first handle.
    ASSERT_SQL_OK(Prepare("SELECT 2 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Freeing a prepared statement while its result-set cursor is still open must
// leave the connection healthy: internally the driver drains the cursor
// (capturing the prepared handle) and issues sp_unprepare, but this test only
// verifies the observable outcome — a fresh statement on the same connection
// executes normally afterward. Uses a private statement so the fixture's stmt_
// teardown is unaffected.
//
// Benefits-from-mock-tds: a mock TDS server could assert the drain + sp_unprepare
// RPCs fired, not just the healthy-connection outcome.
TEST_F(PrepareExecuteLiveTest, FreeWithOpenCursorReleasesHandleAndKeepsConnection) {
    SQLHSTMT s = SQL_NULL_HSTMT;
    ASSERT_EQ(SQL_SUCCESS, SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &s));

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 7 AS v");
    ASSERT_EQ(SQL_SUCCESS,
              SQLPrepare(s, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS));
    ASSERT_EQ(SQL_SUCCESS, SQLExecute(s));
    // Fetch a row but deliberately leave the cursor open.
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(s));

    // Free with the cursor still open — the driver must drain + unprepare.
    ASSERT_EQ(SQL_SUCCESS, SQLFreeHandle(SQL_HANDLE_STMT, s));

    // The connection is still healthy: a fresh statement executes normally.
    ASSERT_SQL_OK(Prepare("SELECT 8 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("8", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A '?' inside a string literal is not a parameter marker.
TEST_F(PrepareExecuteLiveTest, LiteralQuestionMarkIsNotAParam) {
    ASSERT_SQL_OK(Prepare("SELECT '?' AS v"), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("?", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Re-executing while the cursor is still open is a cursor-state error, but the
// same statement re-executes cleanly once the cursor is closed.
TEST_F(PrepareExecuteLiveTest, ReExecuteWhileCursorOpenReturns24000) {
    ASSERT_SQL_OK(Prepare("SELECT 1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    // Fetch a row so the cursor is firmly open in the DM's state machine.
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // After closing the cursor, the prepared statement is reusable.
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQLExecDirect with a bound parameter executes directly (sp_executesql) and
// substitutes the value, with no persistent prepared handle.
TEST_F(PrepareExecuteLiveTest, ExecDirectWithParam) {
    std::vector<SQLCHAR> value = {'d', 'i', 'r', 'e', 'c', 't', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS v");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("direct", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQLExecDirect substitutes multiple bound parameters positionally, including a
// NULL-indicator parameter.
TEST_F(PrepareExecuteLiveTest, ExecDirectWithMultipleParams) {
    std::vector<SQLCHAR> a = {'5', '\0'};
    std::vector<SQLCHAR> b = {'6', '\0'};
    std::vector<SQLCHAR> c = {'i', 'g', 'n', 'o', 'r', 'e', 'd', '\0'};
    SQLLEN ind_a = SQL_NTS, ind_b = SQL_NTS, ind_c = SQL_NULL_DATA;
    ASSERT_SQL_OK(BindChar(1, a, ind_a), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindChar(2, b, ind_b), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindChar(3, c, ind_c), SQL_HANDLE_STMT, stmt_);

    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SELECT CAST(? AS INT) + CAST(? AS INT) AS s, ? AS n");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("11", GetColumnChar(1));

    SQLLEN ind_out = 0;
    GetColumnChar(2, &ind_out);
    EXPECT_EQ(SQL_NULL_DATA, ind_out);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQLExecDirect with a marker but no bound parameter fails with 07002.
TEST_F(PrepareExecuteLiveTest, ExecDirectUnboundMarkerReturns07002) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS v");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07002");
}

// A prepared statement with an unbound marker fails at execute time.
TEST_F(PrepareExecuteLiveTest, UnboundMarkerReturns07002) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07002");
}

// SQL_RESET_PARAMS drops bindings; a subsequent execute sees an unbound marker.
TEST_F(PrepareExecuteLiveTest, ResetParamsClearsBindings) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value = {'x', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07002");
}

// ParameterNumber 0 is invalid.
TEST_F(PrepareExecuteLiveTest, BindParameterNumberZeroReturns07009) {
    std::vector<SQLCHAR> value = {'x', '\0'};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 0, SQL_PARAM_INPUT, SQL_C_CHAR,
                                    SQL_VARCHAR, 1, 0, value.data(),
                                    static_cast<SQLLEN>(value.size()), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");
}

// Output parameters are not implemented in Phase 1: mssql-odbc rejects the bind
// with HYC00. The reference msodbcsql driver supports output params, so this is
// mssql-odbc-specific behavior — skip it on the msodbcsql comparison leg.
TEST_F(PrepareExecuteLiveTest, OutputParameterReturnsHyc00) {
    SKIP_IF_COMPARING_MSODBCSQL();
    std::vector<SQLCHAR> value = {'x', '\0'};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_CHAR,
                                    SQL_VARCHAR, 1, 0, value.data(),
                                    static_cast<SQLLEN>(value.size()), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// A re-prepare whose new plan fails at sp_prepexec (syntax error) must leave the
// statement reusable. The failing sp_prepexec carries the prior handle as its
// piggybacked `@handle` drop, and the server releases it while processing the
// RPC — so the driver must forget it, not re-arm it. Mirrors msodbcsql, which
// clears `hPrepDropDeferred` before dispatch and never restores it on failure
// (PrepOrPrepExecQuery, sqlccmd.cpp). Had the driver re-armed the handle, the
// next sp_prepexec would re-drop it and fail with HY000/8179 (handle not found).
TEST_F(PrepareExecuteLiveTest, FailedReprepareKeepsStatementUsable) {
    // Prepare + execute a valid plan so a server handle is cached.
    ASSERT_SQL_OK(Prepare("SELECT 1 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Re-prepare with failing text: sp_prepexec drops the cached handle but then
    // fails on the syntax error. The driver must forget the released handle.
    ASSERT_SQL_OK(Prepare("SELECT FROM WHERE"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));

    // A fresh plan still prepares and executes cleanly — a re-armed handle would
    // make this sp_prepexec fail with HY000/8179 (handle not found).
    ASSERT_SQL_OK(Prepare("SELECT 2 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}
