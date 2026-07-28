// Copyright (c) Microsoft Corporation. All rights reserved.
// row_count_test.cpp  –  E2E tests for SQLRowCount.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <cstring>

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

// SQL_NULL_HSTMT — the DM rejects this before the driver sees it.
TEST(RowCountTest, NullHandle) {
    SQLLEN rows = -999;
    SQLRETURN rc = SQLRowCount(SQL_NULL_HSTMT, &rows);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class RowCountLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    void Exec(const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
        ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    }

    SQLLEN RowCount() {
        SQLLEN rows = -999;
        SQLRETURN rc = SQLRowCount(stmt_, &rows);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        return rows;
    }
};

// Before any execute, the Driver Manager's state machine rejects SQLRowCount
// with HY010 (Function sequence error) — the driver is never invoked. This is
// identical for msodbcsql through the same DM. The driver's own -1
// (SQL_NO_ROWCOUNT_TOTAL) default only surfaces once a statement has executed;
// that path is covered directly (bypassing the DM) by the unit test in
// src/api/row_count.rs.
TEST_F(RowCountLiveTest, FreshStatementReturnsSequenceError) {
    SQLLEN rows = 12345;
    SQLRETURN rc = SQLRowCount(stmt_, &rows);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

// INSERT reports the number of rows inserted.
TEST_F(RowCountLiveTest, InsertReportsAffectedRows) {
    Exec("CREATE TABLE #rc(i int)");
    Exec("INSERT INTO #rc VALUES (1), (2), (3)");
    EXPECT_EQ(3, RowCount());
}

// UPDATE reports the number of rows matched/updated.
TEST_F(RowCountLiveTest, UpdateReportsAffectedRows) {
    Exec("CREATE TABLE #rc(i int)");
    Exec("INSERT INTO #rc VALUES (1), (2), (3), (4)");
    Exec("UPDATE #rc SET i = i + 10 WHERE i >= 2");
    EXPECT_EQ(3, RowCount());
}

// DELETE reports the number of rows removed.
TEST_F(RowCountLiveTest, DeleteReportsAffectedRows) {
    Exec("CREATE TABLE #rc(i int)");
    Exec("INSERT INTO #rc VALUES (1), (2), (3), (4), (5)");
    Exec("DELETE FROM #rc WHERE i <= 2");
    EXPECT_EQ(2, RowCount());
}

// A result-returning SELECT reports -1 on a forward-only cursor — the row
// count is unavailable until fully fetched (msodbcsql parity).
TEST_F(RowCountLiveTest, SelectReportsNoRowCount) {
    Exec("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3");
    EXPECT_EQ(-1, RowCount());

    SQLRETURN rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// DDL carries no DONE_COUNT, so SQLRowCount reports -1.
TEST_F(RowCountLiveTest, DdlReportsNoRowCount) {
    Exec("CREATE TABLE #rc_ddl(i int)");
    EXPECT_EQ(-1, RowCount());
}

// SET NOCOUNT ON suppresses the row count, so a DML statement reports -1.
TEST_F(RowCountLiveTest, NoCountSuppressesRowCount) {
    Exec("SET NOCOUNT ON; CREATE TABLE #rc_nc(i int); INSERT INTO #rc_nc VALUES (1), (2);");
    EXPECT_EQ(-1, RowCount());
}

// A DML statement followed by a SELECT in the same batch: the forward-only
// SELECT the cursor lands on must report -1, not the DML's count. Guards the
// review fix that clears the count when positioning on COLMETADATA.
TEST_F(RowCountLiveTest, DmlThenSelectBatchReportsMinusOneForSelect) {
    Exec("CREATE TABLE #rc_mix(i int)");
    Exec("INSERT INTO #rc_mix VALUES (1), (2), (3)");
    Exec("UPDATE #rc_mix SET i = i + 1; SELECT * FROM #rc_mix;");
    EXPECT_EQ(-1, RowCount());

    SQLRETURN rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// The count from one execute must not leak into the next statement that reports
// none. A DDL after a counted INSERT reports -1, not the INSERT's count.
TEST_F(RowCountLiveTest, RowCountResetsBetweenExecutes) {
    Exec("CREATE TABLE #rc_a(i int)");
    Exec("INSERT INTO #rc_a VALUES (1), (2), (3)");
    EXPECT_EQ(3, RowCount());

    Exec("CREATE TABLE #rc_b(i int)");
    EXPECT_EQ(-1, RowCount());
}

// A pure-DML batch surfaces each statement as its own result set: SQLRowCount
// reports UPDATE(3), then DELETE(2), then INSERT(1) as SQLMoreResults steps
// through, then SQL_NO_DATA — matching msodbcsql.
TEST_F(RowCountLiveTest, MultiDmlBatchReportsPerStatementCounts) {
    Exec("CREATE TABLE #rc_multi(id int, age int)");
    Exec("INSERT INTO #rc_multi VALUES (1,10),(2,20),(3,30),(4,40),(5,50),(6,60)");
    Exec("UPDATE #rc_multi SET age = 99 WHERE id <= 3; "
         "DELETE FROM #rc_multi WHERE id IN (4, 5); "
         "INSERT INTO #rc_multi VALUES (20, 45);");

    EXPECT_EQ(3, RowCount());

    SQLRETURN rc = SQLMoreResults(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, RowCount());

    rc = SQLMoreResults(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, RowCount());

    rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// SQLRowCount tracks the currently-positioned result set: after SQLMoreResults
// advances to the next SELECT, the count reflects that result set (-1), not a
// stale value from the previous one.
TEST_F(RowCountLiveTest, RowCountRefreshedAcrossResultSets) {
    Exec("SELECT 1 AS a; SELECT 2 AS b;");
    EXPECT_EQ(-1, RowCount());

    // Drain the first result set.
    SQLRETURN rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLFetch(stmt_);
    ASSERT_EQ(SQL_NO_DATA, rc);

    // Advance to the second result set.
    rc = SQLMoreResults(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, RowCount());

    rc = SQLMoreResults(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);
}
