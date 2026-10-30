// Copyright (c) Microsoft Corporation. All rights reserved.
// num_result_cols_test.cpp  –  E2E tests for SQLNumResultCols.
//
// Verifies:
//   1. NullHandle                          - SQL_NULL_HSTMT → SQL_INVALID_HANDLE
//   2. FreshStatementReturnsSequenceError  - no active stmt → HY010 (or HY000)
//   3. SelectReturnsProjectedColumnCount   - SELECT with 3 cols → colCount = 3
//   4. DmlReturnsZeroColumns               - CREATE/INSERT → colCount = 0
//   5. NullOutPtrIsTolerated               - null ColumnCountPtr → SQL_SUCCESS
//   6. MultiStatementBatchReportsFirstResultSet - SELECT 1; SELECT 1,2,3 → colCount = 1

#include "odbc_concurrent_test.h"
#include "odbc_test_fixture.h"

// SQLNumResultCols with SQL_NULL_HSTMT — DM rejects before driver.
TEST(NumResultColsTest, NullHandle) {
    SQLSMALLINT colCount = -1;
    SQLRETURN rc = SQLNumResultCols(SQL_NULL_HSTMT, &colCount);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

class NumResultColsLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

ODBC_TEST_F_MODES(NumResultColsLiveTest, FreshStatementReturnsSequenceError,
                  odbc_concurrency::kAllModes) {
    SQLSMALLINT colCount = -1;
    // The DM's HY010 is cleaned up before unixODBC takes its statement lock.
    std::unique_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
    SQLRETURN rc = SQLNumResultCols(stmt_, &colCount);
    EXPECT_SQL_ERROR(rc);
    {
        std::string state = ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
        EXPECT_TRUE(state == "HY010") << "state=" << state;
    }
    SQLULEN timeout = 0;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT, &timeout, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
}

ODBC_TEST_F_MODES(NumResultColsLiveTest, SelectReturnsProjectedColumnCount,
                  odbc_concurrency::kAllModes) {
    ExecDirect("SELECT CAST(1 AS INT) AS i, CAST('abc' AS VARCHAR(10)) AS v, CAST(N'xy' AS NVARCHAR(12)) AS n");

    SQLSMALLINT colCount = -1;
    SQLRETURN rc = SQLNumResultCols(stmt_, &colCount);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, colCount);

    SQLCloseCursor(stmt_);
}

ODBC_TEST_F(NumResultColsLiveTest, DmlReturnsZeroColumns) {
    ExecDirect("CREATE TABLE #num_result_cols_t(i int); INSERT INTO #num_result_cols_t VALUES (1);");

    SQLSMALLINT colCount = -1;
    SQLRETURN rc = SQLNumResultCols(stmt_, &colCount);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, colCount);
}

ODBC_TEST_F_MODES(NumResultColsLiveTest, NullOutPtrIsTolerated, odbc_concurrency::kAllModes) {
    ExecDirect("SELECT 1 AS i");

    SQLRETURN rc = SQLNumResultCols(stmt_, nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    SQLCloseCursor(stmt_);
}

// Multi-statement batch: SQLNumResultCols reflects the *current* result set
// only. Until SQLMoreResults advances to the next set, it must report the
// first SELECT's column count (1), not the second's (3).
ODBC_TEST_F_MODES(NumResultColsLiveTest, MultiStatementBatchReportsFirstResultSet,
                  odbc_concurrency::kAllModes) {
    ExecDirect("SELECT 1 AS a; SELECT 1 AS a, 2 AS b, 3 AS c");

    SQLSMALLINT colCount = -1;
    SQLRETURN rc = SQLNumResultCols(stmt_, &colCount);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, colCount);

    SQLCloseCursor(stmt_);
}
