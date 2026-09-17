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
#ifndef SQL_SS_XML
#define SQL_SS_XML (-152)
#endif
#ifndef SQL_SS_TABLE
#define SQL_SS_TABLE (-153)
#endif
#ifndef SQL_SS_VECTOR
#define SQL_SS_VECTOR (-156)
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
    virtual SQLUINTEGER OdbcVersion() const { return SQL_OV_ODBC3_80; }

    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        ASSERT_SQL_OK(SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                                    reinterpret_cast<SQLPOINTER>(OdbcVersion()), 0),
                      SQL_HANDLE_ENV, env_);
        Connect();
    }
};

class GetTypeInfoOdbcVersionLiveTest
    : public GetTypeInfoLiveTest,
      public ::testing::WithParamInterface<SQLUINTEGER> {
protected:
    SQLUINTEGER OdbcVersion() const override { return GetParam(); }
};

// Benefits-from-mock-tds: request capture could assert the positional
// SQL_TYPE_TIMESTAMP and named @ODBCVer=4 parameters directly for both app
// versions; the live server exposes only their result-set effects.
TEST_P(GetTypeInfoOdbcVersionLiveTest, TimestampFilterAndColumnContractMatch) {
    ASSERT_SQL_OK(SQLGetTypeInfo(stmt_, SQL_TYPE_TIMESTAMP), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("TYPE_NAME", DescribeColName(stmt_, 1));
    EXPECT_EQ("DATA_TYPE", DescribeColName(stmt_, 2));
    EXPECT_EQ("COLUMN_SIZE", DescribeColName(stmt_, 3));
    EXPECT_EQ("FIXED_PREC_SCALE", DescribeColName(stmt_, 11));
    EXPECT_EQ("AUTO_UNIQUE_VALUE", DescribeColName(stmt_, 12));
    int rows = 0;
    SQLRETURN rc;
    while ((rc = SQLFetch(stmt_)) == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) {
        char dataType[16] = {};
        SQLLEN indicator = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_CHAR, dataType, sizeof(dataType), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(std::to_string(SQL_TYPE_TIMESTAMP), std::string(dataType));
        ++rows;
    }
    EXPECT_EQ(SQL_NO_DATA, rc);
    EXPECT_GT(rows, 0);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// msodbcsql sends pseudo-version 4 here rather than the catalog functions'
// value 3 (`sqlcdd.cpp:2206`, `fODBCVer = ISYUKON(lpdbc) ? 4 : 3`), and the
// comment beside it attributes that to making sp_datatype_info report NULL
// precision for XML. That effect does not reproduce on a modern server: in
// build 176155 both this driver and msodbcsql 18.6.2.1 returned a non-NULL,
// one-character COLUMN_SIZE for the XML row on all 16 runs, against both
// SQL_OV_ODBC3 and SQL_OV_ODBC3_80. The RPC parameter itself is pinned by the
// `odbc_ver_is_the_yukon_pseudo_version` unit test; what stays worth checking
// live is that the XML row is returned and reports a column size at all.
TEST_P(GetTypeInfoOdbcVersionLiveTest, XmlColumnSizeIsReported) {
    ASSERT_SQL_OK(SQLGetTypeInfo(stmt_, SQL_SS_XML), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    char columnSize[32] = {};
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 3, SQL_C_CHAR, columnSize, sizeof(columnSize), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_NE(SQL_NULL_DATA, indicator)
        << "sp_datatype_info_100 reported no COLUMN_SIZE for the XML row";
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

INSTANTIATE_TEST_SUITE_P(Odbc3And38, GetTypeInfoOdbcVersionLiveTest,
                         ::testing::Values(static_cast<SQLUINTEGER>(SQL_OV_ODBC3),
                                           static_cast<SQLUINTEGER>(SQL_OV_ODBC3_80)));

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

// The columns the ODBC spec defines as NOT NULL report SQL_NO_NULLS, matching
// msodbcsql's ClearNullable post-processing.
TEST_F(GetTypeInfoLiveTest, NotNullColumnsReportNoNulls) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_ALL_TYPES);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    for (SQLUSMALLINT col : {1, 2, 7, 8, 9, 11, 16}) {
        SQLTCHAR name[128] = {};
        SQLSMALLINT nameLen = 0;
        SQLSMALLINT dataType = 0;
        SQLULEN columnSize = 0;
        SQLSMALLINT decimalDigits = 0;
        SQLSMALLINT nullable = -1;
        rc = SQLDescribeCol(stmt_, col, name,
                            static_cast<SQLSMALLINT>(sizeof(name) / sizeof(SQLTCHAR)),
                            &nameLen, &dataType, &columnSize, &decimalDigits, &nullable);
        ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_NO_NULLS, nullable) << "column " << col << " must be NOT NULL";
    }

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

// A failed call (invalid type -> HY004) leaves the statement clean, so a
// subsequent valid call on the same handle succeeds and opens a result set.
TEST_F(GetTypeInfoLiveTest, RecoversAfterInvalidType) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, 999);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY004");

    rc = SQLGetTypeInfo(stmt_, SQL_INTEGER);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLFetch(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// A user-defined type is not reported as an ODBC type (HYC00), matching
// msodbcsql.
TEST_F(GetTypeInfoLiveTest, UdtReturnsHYC00) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_SS_UDT);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// SQL_SS_UDT is not special: msodbcsql answers HYC00 for every unmapped id at
// or below SQL_TYPE_DRIVER_START (-80), not just the UDT id.
TEST_F(GetTypeInfoLiveTest, DriverRangeTypeReturnsHYC00) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, -200);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// A table type sits far below SQL_TYPE_DRIVER_START but is still HY004, because
// msodbcsql's FInternalSqlType check runs before the driver-range bound.
TEST_F(GetTypeInfoLiveTest, TableTypeReturnsHY004) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_SS_TABLE);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY004");
}

// SQL Server supports no interval types, but the request is not an error:
// msodbcsql discards IsValidSqlType's HYC00 verdict for these ids and sends the
// RPC anyway, so the call succeeds with an empty result set.
TEST_F(GetTypeInfoLiveTest, IntervalTypeReturnsEmptyResultSet) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_INTERVAL_YEAR);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// msodbcsql accepts a vector type id, but only because it also switches to
// sp_datatype_info_170 when the connection negotiated vector support. This
// driver always calls _100, which has no vector row, so it answers HYC00 ("not
// implemented") rather than reporting success with no vector metadata. Update
// this expectation when the _170 selection lands.
TEST_F(GetTypeInfoLiveTest, VectorTypeIsNotImplementedYet) {
    SKIP_IF_COMPARING_MSODBCSQL();
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_SS_VECTOR);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// The type-info cursor is fully drainable: every row fetches cleanly until
// SQL_NO_DATA, exercising the open-cursor fetch loop over the live result set.
TEST_F(GetTypeInfoLiveTest, DrainsAllRows) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_ALL_TYPES);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    int rows = 0;
    while ((rc = SQLFetch(stmt_)) == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO) {
        ++rows;
    }
    EXPECT_EQ(SQL_NO_DATA, rc);
    EXPECT_GT(rows, 0);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// Filtering by SQL_INTEGER returns the `int` type row — confirms the @data_type
// argument reaches the catalog proc and the TYPE_NAME column carries its value.
TEST_F(GetTypeInfoLiveTest, SpecificTypeReturnsExpectedTypeName) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_INTEGER);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    char typeName[64] = {};
    SQLLEN indicator = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, typeName, sizeof(typeName), &indicator);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("int", std::string(typeName));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// After a full round trip and cursor close, the same statement re-opens a fresh
// type-info result set — exercises the context reset in the live execute path.
TEST_F(GetTypeInfoLiveTest, ReExecuteAfterCloseSucceeds) {
    SQLRETURN rc = SQLGetTypeInfo(stmt_, SQL_INTEGER);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLCloseCursor(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLGetTypeInfo(stmt_, SQL_ALL_TYPES);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("TYPE_NAME", DescribeColName(stmt_, 1));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// SQLGetTypeInfo replaces a prior query's result set on the same statement,
// exercising the metadata reset before the catalog RPC.
TEST_F(GetTypeInfoLiveTest, ReplacesPriorQueryResultSet) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1 AS one");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    rc = SQLCloseCursor(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLGetTypeInfo(stmt_, SQL_ALL_TYPES);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    // The new result set is the type-info contract, not the prior SELECT.
    EXPECT_EQ("TYPE_NAME", DescribeColName(stmt_, 1));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}
