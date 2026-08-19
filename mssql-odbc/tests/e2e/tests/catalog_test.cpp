// Copyright (c) Microsoft Corporation. All rights reserved.
// catalog_test.cpp  –  E2E tests for the ODBC catalog functions: SQLTables,
// SQLColumns, SQLPrimaryKeys, SQLForeignKeys, SQLSpecialColumns,
// SQLStatistics, SQLProcedures.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <cstdint>
#include <cstdio>
#include <random>
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

// Reads a column's Nullable flag via SQLDescribeCol (SQL_NO_NULLS /
// SQL_NULLABLE / SQL_NULLABLE_UNKNOWN), exercising the same SQLDescribeCol
// call as DescribeColName so ClearNullable coverage doesn't need a second
// round trip.
SQLSMALLINT DescribeColNullable(SQLHSTMT stmt, SQLUSMALLINT column) {
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
    return nullable;
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

// A per-process random suffix appended to the catalog tests' fixture table
// names. These are permanent tables (unlike every other e2e test file's
// #temp tables — sp_tables et al. would report mangled names for a temp
// table), dropped and recreated in SetUp/TearDown, so two concurrent runs
// against the same database (two CI legs, or a developer running locally
// alongside CI) would otherwise delete each other's fixtures mid-test.
// Memoized in a function-local static so every call within one process
// shares the same suffix.
const std::string& UniqueSuffix() {
    static const std::string suffix = [] {
        std::random_device rd;
        std::mt19937_64 gen(rd());
        std::uniform_int_distribution<uint64_t> dist;
        char buf[17] = {};
        std::snprintf(buf, sizeof(buf), "%016llx", static_cast<unsigned long long>(dist(gen)));
        return std::string(buf);
    }();
    return suffix;
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
    // Permanent tables (unlike other e2e files' #temp tables — catalog
    // functions need a table sp_tables et al. can actually report by name).
    // A per-process suffix (UniqueSuffix() above) keeps them collision-free
    // between concurrent runs against the same database.
    static inline const std::string kParentTable = "odbc_e2e_catalog_parent_" + UniqueSuffix();
    static inline const std::string kChildTable = "odbc_e2e_catalog_child_" + UniqueSuffix();

    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
        DropTestTables();
        ExecDirect(
            "CREATE TABLE " + kParentTable +
            " ("
            "  id INT NOT NULL PRIMARY KEY,"
            "  name VARCHAR(50) NOT NULL,"
            "  note VARCHAR(50) NULL"
            ")");
        ExecDirect("CREATE UNIQUE INDEX ix_" + kParentTable + "_name ON " + kParentTable +
                   "(name)");
        ExecDirect(
            "CREATE TABLE " + kChildTable +
            " ("
            "  id INT NOT NULL PRIMARY KEY,"
            "  parent_id INT NOT NULL REFERENCES " +
            kParentTable + "(id)"
            ")");
    }

    void TearDown() override {
        if (dbc_ != SQL_NULL_HDBC) {
            DropTestTables();
        }
        ODBCTest::TearDown();
    }

    void DropTestTables() {
        ExecDirectIgnoreError("DROP TABLE IF EXISTS " + kChildTable);
        ExecDirectIgnoreError("DROP TABLE IF EXISTS " + kParentTable);
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
//
// Benefits-from-mock-tds: this only proves the black-box outcome (0 rows, no
// error). A mock TDS server would let this assert that the qualified
// `[db].sys.sp_tables` RPC actually failed and a *second*, unqualified RPC
// with the unmatchable name filter actually fired — i.e. that the retry
// mechanism in `run_catalog` ran, not merely that some code path happened to
// return empty.
TEST_F(CatalogLiveTest, TablesNonexistentCatalogReturnsEmptyNotError) {
    SqlTString catalog = ODBCTestUtils::ToSqlTStr("odbc_e2e_definitely_missing_db");
    SQLRETURN rc = SQLTables(stmt_, const_cast<SQLTCHAR*>(catalog.c_str()), SQL_NTS, nullptr, 0,
                             nullptr, 0, nullptr, 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, DrainRows(stmt_));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The bare "%" catalog with empty schema and table is the ODBC-mandated idiom
// for enumerating every catalog (database) the server exposes — every
// ODBC-based tool relies on it. It must not be treated as a literal (and
// here, nonexistent) catalog named "%", which would otherwise silently
// produce an empty result via the nonexistent-catalog path above instead of
// the catalog list. Every SQL Server instance has at least `master`, so this
// must return at least one row.
TEST_F(CatalogLiveTest, TablesEnumeratesCatalogsWithBarePercent) {
    SqlTString percent = ODBCTestUtils::ToSqlTStr("%");
    SqlTString empty = ODBCTestUtils::ToSqlTStr("");
    SQLRETURN rc = SQLTables(stmt_, const_cast<SQLTCHAR*>(percent.c_str()), SQL_NTS,
                             const_cast<SQLTCHAR*>(empty.c_str()), SQL_NTS,
                             const_cast<SQLTCHAR*>(empty.c_str()), SQL_NTS, nullptr, 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_GE(DrainRows(stmt_), 1);

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
    // ClearNullable: TABLE_NAME and NULLABLE are ODBC-mandated NOT NULL,
    // regardless of what sp_columns_100 itself reports for them.
    EXPECT_EQ(SQL_NO_NULLS, DescribeColNullable(stmt_, 3));  // TABLE_NAME
    EXPECT_EQ(SQL_NO_NULLS, DescribeColNullable(stmt_, 11)); // NULLABLE
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

// Supplying two different, both-valid catalog names for the PK and FK sides
// disagrees about which database the foreign key should be resolved in;
// msodbcsql forces an empty result set rather than guessing which side the
// caller meant (sqlcdd.cpp SQLForeignKeysW, lines 984-1008). `master` and
// `tempdb` are guaranteed to exist on every SQL Server instance.
TEST_F(CatalogLiveTest, ForeignKeysConflictingCatalogsReturnsEmpty) {
    SqlTString pkTable = ODBCTestUtils::ToSqlTStr(kParentTable);
    SqlTString fkTable = ODBCTestUtils::ToSqlTStr(kChildTable);
    SqlTString pkCatalog = ODBCTestUtils::ToSqlTStr("master");
    SqlTString fkCatalog = ODBCTestUtils::ToSqlTStr("tempdb");
    SQLRETURN rc = SQLForeignKeys(
        stmt_, const_cast<SQLTCHAR*>(pkCatalog.c_str()), SQL_NTS, nullptr, 0,
        const_cast<SQLTCHAR*>(pkTable.c_str()), SQL_NTS,
        const_cast<SQLTCHAR*>(fkCatalog.c_str()), SQL_NTS, nullptr, 0,
        const_cast<SQLTCHAR*>(fkTable.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, DrainRows(stmt_));

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
