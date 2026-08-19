// Copyright (c) Microsoft Corporation. All rights reserved.
// catalog_test.cpp  –  E2E tests for the ODBC catalog functions: SQLTables,
// SQLColumns, SQLPrimaryKeys, SQLForeignKeys, SQLSpecialColumns,
// SQLStatistics, SQLProcedures.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <string>

namespace {

// Reads a column's name via SQLDescribeCol and returns it as a narrow string.
std::string DescribeColName(SQLHSTMT stmt, SQLUSMALLINT column) {
    SQLTCHAR name[128] = {};
    SQLSMALLINT nameLen = 0;
    SQLSMALLINT dataType = 0;
    SQLULEN columnSize = 0;
    SQLSMALLINT decimalDigits = 0;
    SQLSMALLINT nullable = 0;
    SQLRETURN rc = SQLDescribeCol(stmt, column, name,
                                  static_cast<SQLSMALLINT>(sizeof(name) / sizeof(SQLTCHAR)),
                                  &nameLen, &dataType, &columnSize, &decimalDigits, &nullable);
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    return ODBCTestUtils::ToNarrow(SqlTString(name));
}

// Drains a cursor to completion and returns the number of rows fetched.
int DrainRows(SQLHSTMT stmt) {
    int rows = 0;
    SQLRETURN rc;
    while ((rc = SQLFetch(stmt)) == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) {
        ++rows;
    }
    EXPECT_EQ(SQL_NO_DATA, rc);
    return rows;
}

} // namespace

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

TEST(CatalogTest, TablesNullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLTables(SQL_NULL_HSTMT, nullptr, 0, nullptr, 0, nullptr, 0, nullptr, 0));
}

TEST(CatalogTest, ColumnsNullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLColumns(SQL_NULL_HSTMT, nullptr, 0, nullptr, 0, nullptr, 0, nullptr, 0));
}

TEST(CatalogTest, PrimaryKeysNullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLPrimaryKeys(SQL_NULL_HSTMT, nullptr, 0, nullptr, 0, nullptr, 0));
}

TEST(CatalogTest, ForeignKeysNullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLForeignKeys(SQL_NULL_HSTMT, nullptr, 0, nullptr, 0, nullptr, 0, nullptr, 0,
                              nullptr, 0, nullptr, 0));
}

TEST(CatalogTest, StatisticsNullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLStatistics(SQL_NULL_HSTMT, nullptr, 0, nullptr, 0, nullptr, 0, SQL_INDEX_ALL,
                            SQL_QUICK));
}

TEST(CatalogTest, SpecialColumnsNullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLSpecialColumns(SQL_NULL_HSTMT, SQL_BEST_ROWID, nullptr, 0, nullptr, 0, nullptr, 0,
                                SQL_SCOPE_CURROW, SQL_NO_NULLS));
}

TEST(CatalogTest, ProceduresNullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLProcedures(SQL_NULL_HSTMT, nullptr, 0, nullptr, 0, nullptr, 0));
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class CatalogLiveTest : public ODBCTest {
protected:
    // Prefixed and specific enough that a collision with an unrelated table in
    // the test database is unlikely.
    static constexpr const char* kParentTable = "odbc_e2e_catalog_parent";
    static constexpr const char* kChildTable = "odbc_e2e_catalog_child";

    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
        DropTestTables();
        ExecDirect(
            "CREATE TABLE odbc_e2e_catalog_parent ("
            "  id INT NOT NULL PRIMARY KEY,"
            "  name VARCHAR(50) NOT NULL,"
            "  note VARCHAR(50) NULL"
            ")");
        ExecDirect(
            "CREATE UNIQUE INDEX ix_odbc_e2e_catalog_parent_name "
            "ON odbc_e2e_catalog_parent(name)");
        ExecDirect(
            "CREATE TABLE odbc_e2e_catalog_child ("
            "  id INT NOT NULL PRIMARY KEY,"
            "  parent_id INT NOT NULL REFERENCES odbc_e2e_catalog_parent(id)"
            ")");
    }

    void TearDown() override {
        if (dbc_ != SQL_NULL_HDBC) {
            DropTestTables();
        }
        ODBCTest::TearDown();
    }

    void DropTestTables() {
        ExecDirectIgnoreError("DROP TABLE IF EXISTS odbc_e2e_catalog_child");
        ExecDirectIgnoreError("DROP TABLE IF EXISTS odbc_e2e_catalog_parent");
    }
};

// Finds exactly the created table and reports the ODBC 3.x column names.
TEST_F(CatalogLiveTest, TablesFindsCreatedTable) {
    SqlTString name = ODBCTestUtils::ToSqlTStr(kParentTable);
    SQLRETURN rc = SQLTables(stmt_, nullptr, 0, nullptr, 0, const_cast<SQLTCHAR*>(name.c_str()),
                             SQL_NTS, nullptr, 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("TABLE_CAT", DescribeColName(stmt_, 1));
    EXPECT_EQ("TABLE_SCHEM", DescribeColName(stmt_, 2));
    EXPECT_EQ(1, DrainRows(stmt_));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A catalog argument naming a database that does not exist yields an empty
// result set, not an error — msodbcsql's nonexistent-catalog recovery path
// (sqlcdd.cpp DoDD(), lines 1883-1895), which this test exercises live.
TEST_F(CatalogLiveTest, TablesNonexistentCatalogReturnsEmptyNotError) {
    SqlTString catalog = ODBCTestUtils::ToSqlTStr("odbc_e2e_definitely_missing_db");
    SQLRETURN rc = SQLTables(stmt_, const_cast<SQLTCHAR*>(catalog.c_str()), SQL_NTS, nullptr, 0,
                             nullptr, 0, nullptr, 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, DrainRows(stmt_));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Reports the ODBC 3.x column names/positions for the renamed columns and
// finds every column of the created table.
TEST_F(CatalogLiveTest, ColumnsReportsOdbc3ColumnNamesAndAllColumns) {
    SqlTString table = ODBCTestUtils::ToSqlTStr(kParentTable);
    SQLRETURN rc = SQLColumns(stmt_, nullptr, 0, nullptr, 0, const_cast<SQLTCHAR*>(table.c_str()),
                              SQL_NTS, nullptr, 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("TABLE_CAT", DescribeColName(stmt_, 1));
    EXPECT_EQ("TABLE_SCHEM", DescribeColName(stmt_, 2));
    EXPECT_EQ("COLUMN_SIZE", DescribeColName(stmt_, 7));
    EXPECT_EQ("BUFFER_LENGTH", DescribeColName(stmt_, 8));
    EXPECT_EQ("DECIMAL_DIGITS", DescribeColName(stmt_, 9));
    EXPECT_EQ("NUM_PREC_RADIX", DescribeColName(stmt_, 10));
    EXPECT_EQ(3, DrainRows(stmt_)); // id, name, note

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Finds the single-column primary key and reports the ODBC 3.x column names.
TEST_F(CatalogLiveTest, PrimaryKeysFindsIdColumn) {
    SqlTString table = ODBCTestUtils::ToSqlTStr(kParentTable);
    SQLRETURN rc = SQLPrimaryKeys(stmt_, nullptr, 0, nullptr, 0,
                                  const_cast<SQLTCHAR*>(table.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("TABLE_CAT", DescribeColName(stmt_, 1));
    EXPECT_EQ("TABLE_SCHEM", DescribeColName(stmt_, 2));
    EXPECT_EQ(1, DrainRows(stmt_));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Finds the child table's foreign key to the parent and reports both sides'
// ODBC 3.x column names.
TEST_F(CatalogLiveTest, ForeignKeysFindsChildReference) {
    SqlTString pkTable = ODBCTestUtils::ToSqlTStr(kParentTable);
    SqlTString fkTable = ODBCTestUtils::ToSqlTStr(kChildTable);
    SQLRETURN rc =
        SQLForeignKeys(stmt_, nullptr, 0, nullptr, 0, const_cast<SQLTCHAR*>(pkTable.c_str()),
                       SQL_NTS, nullptr, 0, nullptr, 0, const_cast<SQLTCHAR*>(fkTable.c_str()),
                       SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("PKTABLE_CAT", DescribeColName(stmt_, 1));
    EXPECT_EQ("PKTABLE_SCHEM", DescribeColName(stmt_, 2));
    EXPECT_EQ("FKTABLE_CAT", DescribeColName(stmt_, 5));
    EXPECT_EQ("FKTABLE_SCHEM", DescribeColName(stmt_, 6));
    EXPECT_EQ(1, DrainRows(stmt_));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Finds both the primary key and the unique index created on the table.
TEST_F(CatalogLiveTest, StatisticsFindsIndexes) {
    SqlTString table = ODBCTestUtils::ToSqlTStr(kParentTable);
    SQLRETURN rc = SQLStatistics(stmt_, nullptr, 0, nullptr, 0,
                                 const_cast<SQLTCHAR*>(table.c_str()), SQL_NTS, SQL_INDEX_ALL,
                                 SQL_QUICK);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("TABLE_CAT", DescribeColName(stmt_, 1));
    EXPECT_EQ("TABLE_SCHEM", DescribeColName(stmt_, 2));
    // At least the primary key's clustered index and the explicit unique index.
    EXPECT_GE(DrainRows(stmt_), 2);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The best-fit row identifier for a table with a primary key is that key.
TEST_F(CatalogLiveTest, SpecialColumnsFindsRowIdentifier) {
    SqlTString table = ODBCTestUtils::ToSqlTStr(kParentTable);
    SQLRETURN rc = SQLSpecialColumns(stmt_, SQL_BEST_ROWID, nullptr, 0, nullptr, 0,
                                     const_cast<SQLTCHAR*>(table.c_str()), SQL_NTS,
                                     SQL_SCOPE_CURROW, SQL_NO_NULLS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_GT(DrainRows(stmt_), 0);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Opens the stored-procedure result set with the ODBC 3.x column names, even
// when the data source has none matching (an empty result set is still valid).
TEST_F(CatalogLiveTest, ProceduresReportsOdbc3ColumnNames) {
    SQLRETURN rc = SQLProcedures(stmt_, nullptr, 0, nullptr, 0, nullptr, 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("PROCEDURE_CAT", DescribeColName(stmt_, 1));
    EXPECT_EQ("PROCEDURE_SCHEM", DescribeColName(stmt_, 2));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQLTables replaces a prior query's result set on the same statement,
// exercising the metadata reset before the catalog RPC.
TEST_F(CatalogLiveTest, ReplacesPriorQueryResultSet) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1 AS one");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLCloseCursor(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLTables(stmt_, nullptr, 0, nullptr, 0, nullptr, 0, nullptr, 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("TABLE_CAT", DescribeColName(stmt_, 1));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}
