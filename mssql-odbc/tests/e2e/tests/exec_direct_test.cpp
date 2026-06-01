// Copyright (c) Microsoft Corporation. All rights reserved.
// exec_direct_test.cpp  –  E2E tests for SQLExecDirectW.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

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

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class ExecDirectLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
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
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// Query that produces no rows returns SQL_NO_DATA (Phase 1 behavior: driver
// drains all rows eagerly and returns SQL_NO_DATA when the result set is empty).
TEST_F(ExecDirectLiveTest, EmptyResultSet) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1 WHERE 1 = 0");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// Syntactically invalid SQL returns SQL_ERROR.
TEST_F(ExecDirectLiveTest, InvalidSql) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("NOT VALID SQL @@##");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_ERROR(rc);
}

// Re-executing on the same STMT succeeds (Phase 1: each execute drains fully
// and resets pending_rows, so re-use is always safe).
TEST_F(ExecDirectLiveTest, ReExecute) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}
