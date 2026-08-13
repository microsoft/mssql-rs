// Copyright (c) Microsoft Corporation. All rights reserved.
// col_attribute_test.cpp  –  E2E tests for SQLColAttributeW.
//
// Unit tests can only build `int` column metadata, so the per-type mapping
// tables (concise type, type name, radix, and the sql_variant underlying type)
// are only meaningfully exercised here, against a live server.
//
// Verifies:
//   1.  NullHandle                        - SQL_NULL_HSTMT → SQL_INVALID_HANDLE
//   2.  FreshStatementReturnsSequenceError- no active stmt → HY010
//   3.  InvalidColumnOrdinal              - column 0 and past-end → 07009
//   4.  UnknownFieldIdentifier            - unreported field id → HY091
//   5.  DescCountIgnoresColumnNumber      - SQL_DESC_COUNT describes the result set
//   6.  ConciseTypePerColumnType          - int/varchar/nvarchar/decimal concise types
//   7.  TypeNameAndRadix                  - SQL_DESC_TYPE_NAME, SQL_DESC_NUM_PREC_RADIX
//   8.  PrecisionScaleAndNullable         - DECIMAL(10,2), NOT NULL vs NULL
//   9.  UnsignedOnlyForTinyint            - tinyint unsigned, int signed
//   10. NameIsReportedInBytes             - SQL_DESC_NAME length is a byte count
//   11. NameTruncationReturnsInfo         - short buffer → SUCCESS_WITH_INFO + 01004
//   12. VariantTypeOnNonVariantColumn     - HY113
//   13. VariantUnderlyingTypeAfterProbe   - probe then SQL_CA_SS_VARIANT_TYPE
//   14. VariantTypeBeforeProbeIsSequenceError - attribute before the value is read

#include "odbc_test_fixture.h"

#include <algorithm>
#include <string>

// SQL Server-specific identifiers not in standard <sqlext.h>.
#ifndef SQL_CA_SS_VARIANT_TYPE
#define SQL_CA_SS_VARIANT_TYPE (1215)
#endif
#ifndef SQL_SS_VARIANT
#define SQL_SS_VARIANT (-150)
#endif

class ColAttributeLiveTest : public ODBCTest {};

// Reads a numeric attribute, asserting the call succeeded.
static SQLLEN NumericAttr(SQLHSTMT stmt, SQLUSMALLINT col, SQLUSMALLINT field) {
    SQLLEN value = -1;
    SQLRETURN rc = SQLColAttribute(stmt, col, field, nullptr, 0, nullptr, &value);
    EXPECT_TRUE(SQL_SUCCEEDED(rc)) << "field " << field;
    return value;
}

TEST(ColAttributeTest, NullHandle) {
    SQLLEN value = 0;
    SQLRETURN rc = SQLColAttribute(
        SQL_NULL_HSTMT, 1, SQL_DESC_CONCISE_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

TEST_F(ColAttributeLiveTest, FreshStatementReturnsSequenceError) {
    SQLLEN value = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_DESC_CONCISE_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

TEST_F(ColAttributeLiveTest, InvalidColumnOrdinal) {
    ExecDirect("SELECT CAST(1 AS INT) AS c1");
    for (SQLUSMALLINT col : {static_cast<SQLUSMALLINT>(0), static_cast<SQLUSMALLINT>(2)}) {
        SQLLEN value = 0;
        SQLRETURN rc =
            SQLColAttribute(stmt_, col, SQL_DESC_CONCISE_TYPE, nullptr, 0, nullptr, &value);
        EXPECT_EQ(SQL_ERROR, rc) << "column " << col;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");
    }
    SQLCloseCursor(stmt_);
}

// An identifier this driver does not report is rejected rather than answered
// with a silent zero. msodbcsql reports a wider set, so only compare our leg.
TEST_F(ColAttributeLiveTest, UnknownFieldIdentifier) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("SELECT CAST(1 AS INT) AS c1");
    SQLLEN value = 0;
    SQLRETURN rc = SQLColAttribute(stmt_, 1, 9999, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY091");
    SQLCloseCursor(stmt_);
}

// SQL_DESC_COUNT describes the result set, so it answers for a column number
// that would otherwise be out of range.
TEST_F(ColAttributeLiveTest, DescCountIgnoresColumnNumber) {
    ExecDirect("SELECT 1 AS a, 2 AS b, 3 AS c");
    EXPECT_EQ(3, NumericAttr(stmt_, 1, SQL_DESC_COUNT));
    EXPECT_EQ(3, NumericAttr(stmt_, 99, SQL_DESC_COUNT));
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, ConciseTypePerColumnType) {
    ExecDirect(
        "SELECT CAST(1 AS INT) AS i, CAST('a' AS VARCHAR(10)) AS v,"
        " CAST(N'b' AS NVARCHAR(10)) AS n, CAST(1.5 AS DECIMAL(10,2)) AS d");
    EXPECT_EQ(SQL_INTEGER, NumericAttr(stmt_, 1, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(SQL_VARCHAR, NumericAttr(stmt_, 2, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(SQL_WVARCHAR, NumericAttr(stmt_, 3, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(SQL_DECIMAL, NumericAttr(stmt_, 4, SQL_DESC_CONCISE_TYPE));
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, TypeNameAndRadix) {
    ExecDirect("SELECT CAST(1 AS INT) AS i, CAST(1.5 AS FLOAT) AS f,"
               " CAST('a' AS VARCHAR(10)) AS v");

    SQLTCHAR name[64] = {};
    SQLSMALLINT nameLen = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_DESC_TYPE_NAME, name, sizeof(name), &nameLen, nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("int", ODBCTestUtils::ToNarrow(SqlTString(name)));

    // Exact numerics are base 10, approximate are base 2, non-numerics have none.
    EXPECT_EQ(10, NumericAttr(stmt_, 1, SQL_DESC_NUM_PREC_RADIX));
    EXPECT_EQ(2, NumericAttr(stmt_, 2, SQL_DESC_NUM_PREC_RADIX));
    EXPECT_EQ(0, NumericAttr(stmt_, 3, SQL_DESC_NUM_PREC_RADIX));
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, PrecisionScaleAndNullable) {
    ExecDirect("SELECT CAST(1.5 AS DECIMAL(10,2)) AS d,"
               " CAST(NULL AS INT) AS n");
    EXPECT_EQ(10, NumericAttr(stmt_, 1, SQL_DESC_PRECISION));
    EXPECT_EQ(2, NumericAttr(stmt_, 1, SQL_DESC_SCALE));
    EXPECT_EQ(SQL_NULLABLE, NumericAttr(stmt_, 2, SQL_DESC_NULLABLE));
    SQLCloseCursor(stmt_);
}

// `tinyint` is the only unsigned integer SQL Server exposes.
TEST_F(ColAttributeLiveTest, UnsignedOnlyForTinyint) {
    ExecDirect("SELECT CAST(1 AS TINYINT) AS t, CAST(1 AS INT) AS i");
    EXPECT_EQ(SQL_TRUE, NumericAttr(stmt_, 1, SQL_DESC_UNSIGNED));
    EXPECT_EQ(SQL_FALSE, NumericAttr(stmt_, 2, SQL_DESC_UNSIGNED));
    SQLCloseCursor(stmt_);
}

// The wide entry point reports string lengths in bytes, not characters.
TEST_F(ColAttributeLiveTest, NameIsReportedInBytes) {
    ExecDirect("SELECT 1 AS abcd");
    SQLTCHAR name[32] = {};
    SQLSMALLINT nameLen = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_DESC_NAME, name, sizeof(name), &nameLen, nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abcd", ODBCTestUtils::ToNarrow(SqlTString(name)));
    EXPECT_EQ(static_cast<SQLSMALLINT>(4 * sizeof(SQLTCHAR)), nameLen);
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, NameTruncationReturnsInfo) {
    ExecDirect("SELECT 1 AS averylongcolumnname");
    SQLTCHAR name[3] = {};
    SQLSMALLINT nameLen = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_DESC_NAME, name, sizeof(name), &nameLen, nullptr);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    SQLCloseCursor(stmt_);
}

// The variant attribute is rejected outright on a column that is not a
// sql_variant, rather than reporting a type the caller would then trust.
TEST_F(ColAttributeLiveTest, VariantTypeOnNonVariantColumn) {
    ExecDirect("SELECT CAST(1 AS INT) AS c1");
    SQLLEN value = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_CA_SS_VARIANT_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY113");
    SQLCloseCursor(stmt_);
}

// The full sequence an application uses to read a sql_variant: describe the
// column, probe it with a zero-length SQL_C_BINARY read, then ask for the
// underlying C type. The underlying type belongs to the value, so it tracks the
// row rather than the column.
TEST_F(ColAttributeLiveTest, VariantUnderlyingTypeAfterProbe) {
    ExecDirect(
        "SELECT CAST(42 AS SQL_VARIANT) AS v"
        " UNION ALL SELECT CAST(CAST('abc' AS VARCHAR(10)) AS SQL_VARIANT)");

    SQLSMALLINT dataType = 0;
    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, nullptr, 0, nullptr, &dataType, nullptr, nullptr, nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SS_VARIANT, dataType);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, nullptr, 0, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_NE(SQL_NULL_DATA, indicator);
    EXPECT_EQ(SQL_C_SLONG, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

    // Second row holds a different base type in the same column.
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, nullptr, 0, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_C_CHAR, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

    SQLCloseCursor(stmt_);
}

// Without the probe there is no value to report a type for; msodbcsql relies on
// the same ordering, so this only pins our diagnostic.
TEST_F(ColAttributeLiveTest, VariantTypeBeforeProbeIsSequenceError) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("SELECT CAST(42 AS SQL_VARIANT) AS v");
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLLEN value = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_CA_SS_VARIANT_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
    SQLCloseCursor(stmt_);
}
