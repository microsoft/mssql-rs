// Copyright (c) Microsoft Corporation. All rights reserved.
// describe_col_test.cpp  –  E2E tests for SQLDescribeColW.
//
// Verifies:
//   1.  NullHandle                          - SQL_NULL_HSTMT → SQL_INVALID_HANDLE
//   2.  FreshStatementReturnsSequenceError  - no active stmt → HY010
//   3.  BasicMetadata                       - INT/VARCHAR/NVARCHAR cols → name + type + size
//   4.  NameTruncationReturnsInfo           - short name buf → SUCCESS_WITH_INFO + 01004 + full length
//   5.  InvalidColumnOrdinal                - column 0 (bookmark) and past-end → 07009
//   6.  DecimalPrecisionAndScale            - DECIMAL(10,2) → SQL_DECIMAL, colSize=10, decDigits=2
//   7.  NullableFlag                        - sys.objects.name (NOT NULL) vs principal_id (NULL)
//   8.  DateTime2AndDateTimeOffset          - datetime2 → SQL_TYPE_TIMESTAMP, datetimeoffset → -155
//   9.  NVarCharColumnSizeIsCharCount       - NVARCHAR(100) → colSize=100 (chars, not bytes)

#include "odbc_test_fixture.h"

// SQL Server-specific types not in standard <sqlext.h>.
#ifndef SQL_SS_TIMESTAMPOFFSET
#define SQL_SS_TIMESTAMPOFFSET (-155)
#endif

// SQLDescribeColW with SQL_NULL_HSTMT — DM rejects before driver.
TEST(DescribeColTest, NullHandle) {
    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT nullable = 0;
    SQLRETURN rc = SQLDescribeCol(
        SQL_NULL_HSTMT,
        1,
        nullptr,
        0,
        nullptr,
        &dataType,
        &colSize,
        &decDigits,
        &nullable);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

class DescribeColLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

TEST_F(DescribeColLiveTest, FreshStatementReturnsSequenceError) {
    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT nullable = 0;

    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, nullptr, 0, nullptr, &dataType, &colSize, &decDigits, &nullable);
    EXPECT_SQL_ERROR(rc);
    {
        std::string state = ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
        EXPECT_TRUE(state == "HY010") << "state=" << state;
    }
}

TEST_F(DescribeColLiveTest, BasicMetadata) {
    ExecDirect("SELECT CAST(1 AS INT) AS i, CAST('abc' AS VARCHAR(10)) AS v, CAST(N'xy' AS NVARCHAR(12)) AS n");

    SQLTCHAR name[32] = {};
    SQLSMALLINT nameLen = 0;
    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT nullable = 0;

    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, name, 32, &nameLen, &dataType, &colSize, &decDigits, &nullable);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("i", ODBCTestUtils::ToNarrow(SqlTString(name)));
    EXPECT_EQ(SQL_INTEGER, dataType);
    EXPECT_GT(colSize, 0u);

    std::fill(std::begin(name), std::end(name), 0);
    rc = SQLDescribeCol(
        stmt_, 2, name, 32, &nameLen, &dataType, &colSize, &decDigits, &nullable);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("v", ODBCTestUtils::ToNarrow(SqlTString(name)));
    EXPECT_EQ(SQL_VARCHAR, dataType);
    EXPECT_GE(colSize, 10u);

    std::fill(std::begin(name), std::end(name), 0);
    rc = SQLDescribeCol(
        stmt_, 3, name, 32, &nameLen, &dataType, &colSize, &decDigits, &nullable);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("n", ODBCTestUtils::ToNarrow(SqlTString(name)));
    EXPECT_EQ(SQL_WVARCHAR, dataType);
    EXPECT_GE(colSize, 12u);

    SQLCloseCursor(stmt_);
}

TEST_F(DescribeColLiveTest, NameTruncationReturnsInfo) {
    ExecDirect("SELECT 1 AS this_is_a_long_column_name");

    SQLTCHAR name[8] = {};
    SQLSMALLINT nameLen = 0;
    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT nullable = 0;

    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, name, 8, &nameLen, &dataType, &colSize, &decDigits, &nullable);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    // Per spec, *NameLengthPtr is the untruncated character length of the column name.
    EXPECT_EQ(sizeof("this_is_a_long_column_name") - 1, nameLen);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    SQLCloseCursor(stmt_);
}

// Both column 0 (reserved as the bookmark column without bookmarks enabled)
// and ordinals past the last column must be rejected with 07009
// (invalid descriptor index).
TEST_F(DescribeColLiveTest, InvalidColumnOrdinal) {
    ExecDirect("SELECT 1 AS i");

    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT nullable = 0;

    for (SQLUSMALLINT ordinal : {SQLUSMALLINT{0}, SQLUSMALLINT{2}}) {
        SCOPED_TRACE("ordinal=" + std::to_string(ordinal));
        SQLRETURN rc = SQLDescribeCol(
            stmt_, ordinal, nullptr, 0, nullptr, &dataType, &colSize, &decDigits, &nullable);
        EXPECT_SQL_ERROR(rc);
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");
    }

    SQLCloseCursor(stmt_);
}

TEST_F(DescribeColLiveTest, DecimalPrecisionAndScale) {
    ExecDirect("SELECT CAST(3.14 AS DECIMAL(10,2)) AS d");

    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT nullable = 0;

    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, nullptr, 0, nullptr, &dataType, &colSize, &decDigits, &nullable);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_DECIMAL, dataType);
    EXPECT_EQ(10u, colSize);
    EXPECT_EQ(2, decDigits);

    SQLCloseCursor(stmt_);
}

TEST_F(DescribeColLiveTest, NullableFlag) {
    // Use sys.objects: `name` is NVARCHAR(128) NOT NULL, `principal_id` is INT NULL.
    // Stable across SQL Server versions, no DDL needed.
    ExecDirect("SELECT TOP 1 name, principal_id FROM sys.objects");

    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT notNullCol = 0;
    SQLSMALLINT nullableCol = 0;

    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, nullptr, 0, nullptr, &dataType, &colSize, &decDigits, &notNullCol);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLDescribeCol(
        stmt_, 2, nullptr, 0, nullptr, &dataType, &colSize, &decDigits, &nullableCol);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_NO_NULLS, notNullCol);
    EXPECT_EQ(SQL_NULLABLE, nullableCol);

    SQLCloseCursor(stmt_);
}

// datetime2 has no ODBC core equivalent for sub-second precision, but maps to
// SQL_TYPE_TIMESTAMP. datetimeoffset is SQL Server-specific → SQL_SS_TIMESTAMPOFFSET (-155).
TEST_F(DescribeColLiveTest, DateTime2AndDateTimeOffset) {
    ExecDirect("SELECT CAST(SYSDATETIME() AS DATETIME2) AS dt2, "
               "CAST(SYSDATETIMEOFFSET() AS DATETIMEOFFSET) AS dto");

    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT nullable = 0;

    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, nullptr, 0, nullptr, &dataType, &colSize, &decDigits, &nullable);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_TYPE_TIMESTAMP, dataType);

    rc = SQLDescribeCol(
        stmt_, 2, nullptr, 0, nullptr, &dataType, &colSize, &decDigits, &nullable);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SS_TIMESTAMPOFFSET, dataType);

    SQLCloseCursor(stmt_);
}

// ColumnSize for character types is in characters, not bytes. NVARCHAR(100) on the wire
// occupies 200 bytes but must be reported as 100.
TEST_F(DescribeColLiveTest, NVarCharColumnSizeIsCharCount) {
    ExecDirect("SELECT CAST(N'x' AS NVARCHAR(100)) AS n");

    SQLSMALLINT dataType = 0;
    SQLULEN colSize = 0;
    SQLSMALLINT decDigits = 0;
    SQLSMALLINT nullable = 0;

    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, nullptr, 0, nullptr, &dataType, &colSize, &decDigits, &nullable);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_WVARCHAR, dataType);
    EXPECT_EQ(100u, colSize);

    SQLCloseCursor(stmt_);
}
