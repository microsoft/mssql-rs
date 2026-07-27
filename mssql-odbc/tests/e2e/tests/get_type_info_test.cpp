// Copyright (c) Microsoft Corporation. All rights reserved.
// get_type_info_test.cpp  –  E2E tests for SQLGetTypeInfoW.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <string>

// msodbcsql-specific SQL type id for a user-defined (CLR) type. Not present in
// the stock unixODBC headers, so define it locally.
#ifndef SQL_SS_UDT
#define SQL_SS_UDT (-151)
#endif

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

} // namespace

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

// SQL_NULL_HSTMT — the DM rejects this before the driver sees it.
TEST(GetTypeInfoTest, NullHandle) {
    SQLRETURN rc = SQLGetTypeInfo(SQL_NULL_HSTMT, SQL_ALL_TYPES);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class GetTypeInfoLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// SQL_ALL_TYPES opens a fetchable result set with the full ODBC type-info
// column contract (at least 19 columns) and at least one row.
TEST_F(GetTypeInfoLiveTest, AllTypesReturnsRows) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_ALL_TYPES);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT columnCount = 0;
    rc = SQLNumResultCols(stmt_, &columnCount);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_GE(columnCount, 19);

    rc = SQLFetch(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// The result set carries the ODBC 3.x column names, including the three columns
// msodbcsql renames from their legacy catalog-proc names.
TEST_F(GetTypeInfoLiveTest, ColumnNamesMatchOdbcContract) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_ALL_TYPES);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ("TYPE_NAME", DescribeColName(stmt_, 1));
    EXPECT_EQ("DATA_TYPE", DescribeColName(stmt_, 2));
    EXPECT_EQ("COLUMN_SIZE", DescribeColName(stmt_, 3));
    EXPECT_EQ("FIXED_PREC_SCALE", DescribeColName(stmt_, 11));
    EXPECT_EQ("AUTO_UNIQUE_VALUE", DescribeColName(stmt_, 12));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// Filtering by a specific type returns only rows whose DATA_TYPE matches.
TEST_F(GetTypeInfoLiveTest, SpecificTypeFilters) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_INTEGER);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // Read DATA_TYPE (column 2) as text — the Phase-1 SQLGetData supports
    // SQL_C_CHAR; the integer value SQL_INTEGER (4) renders as "4".
    char dataType[16] = {};
    SQLLEN indicator = 0;
    rc = SQLGetData(stmt_, 2, SQL_C_CHAR, dataType, sizeof(dataType), &indicator);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(std::to_string(SQL_INTEGER), std::string(dataType));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// An unrecognized SQL type id is rejected with HY004 before any server round
// trip (parity with msodbcsql's client-side IsValidSqlType check).
TEST_F(GetTypeInfoLiveTest, InvalidTypeReturnsHY004) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, 999);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY004");
}

// A user-defined type is not reported as an ODBC type (HYC00), matching
// msodbcsql.
TEST_F(GetTypeInfoLiveTest, UdtReturnsHYC00) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_SS_UDT);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}
