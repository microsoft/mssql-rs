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

// Output parameters are not yet supported (Phase 1 input-only).
TEST_F(PrepareExecuteLiveTest, OutputParameterReturnsHyc00) {
    std::vector<SQLCHAR> value = {'x', '\0'};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_CHAR,
                                    SQL_VARCHAR, 1, 0, value.data(),
                                    static_cast<SQLLEN>(value.size()), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}
