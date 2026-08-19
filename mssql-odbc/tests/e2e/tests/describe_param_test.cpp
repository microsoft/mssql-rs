// Copyright (c) Microsoft Corporation. All rights reserved.
// SQLDescribeParam parity and mssql-python NULL binding tests.

#include "odbc_test_fixture.h"

#include <array>
#include <string>

namespace {

struct ParamDescription {
    SQLSMALLINT data_type = 0;
    SQLULEN size = 0;
    SQLSMALLINT scale = 0;
    SQLSMALLINT nullable = 0;
};

}  // namespace

TEST(DescribeParamTest, NullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLDescribeParam(SQL_NULL_HSTMT, 1, nullptr, nullptr, nullptr, nullptr));
}

class DescribeParamLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or "
                      "ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLRETURN Prepare(const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    ParamDescription Describe(SQLUSMALLINT ordinal) {
        ParamDescription description;
        SQLRETURN rc =
            SQLDescribeParam(stmt_, ordinal, &description.data_type, &description.size,
                             &description.scale, &description.nullable);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        return description;
    }

    void BindDefaultNull(SQLUSMALLINT ordinal,
                         const ParamDescription& description,
                         SQLLEN& indicator) {
        ASSERT_SQL_OK(
            SQLBindParameter(stmt_, ordinal, SQL_PARAM_INPUT, SQL_C_DEFAULT,
                             description.data_type, description.size, description.scale,
                             nullptr, 0, &indicator),
            SQL_HANDLE_STMT, stmt_);
    }

    std::string GetColumn(SQLUSMALLINT ordinal, SQLLEN* indicator = nullptr) {
        SQLCHAR value[128] = {};
        SQLLEN length = 0;
        EXPECT_SQL_OK(SQLGetData(stmt_, ordinal, SQL_C_CHAR, value, sizeof(value), &length),
                      SQL_HANDLE_STMT, stmt_);
        if (indicator != nullptr) {
            *indicator = length;
        }
        return length == SQL_NULL_DATA
                   ? std::string()
                   : std::string(reinterpret_cast<const char*>(value));
    }
};

TEST_F(DescribeParamLiveTest, IsAdvertised) {
    SQLUSMALLINT supported = SQL_FALSE;
    ASSERT_SQL_OK(SQLGetFunctions(dbc_, SQL_API_SQLDESCRIBEPARAM, &supported),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_TRUE, supported);
}

TEST_F(DescribeParamLiveTest, RequiresPreparedStatement) {
    SQLSMALLINT data_type = 0;
    SQLRETURN rc =
        SQLDescribeParam(stmt_, 1, &data_type, nullptr, nullptr, nullptr);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

TEST_F(DescribeParamLiveTest, RejectsInvalidOrdinals) {
    ASSERT_SQL_OK(Prepare("SELECT ISNULL(?, 42)"), SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT data_type = 0;
    SQLRETURN rc =
        SQLDescribeParam(stmt_, 0, &data_type, nullptr, nullptr, nullptr);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");

    rc = SQLDescribeParam(stmt_, 2, &data_type, nullptr, nullptr, nullptr);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");
}

TEST_F(DescribeParamLiveTest, ReportsRepresentativeMetadata) {
    ASSERT_SQL_OK(
        Prepare("SELECT CAST(? AS INT), CAST(? AS NVARCHAR(40)), "
                "CAST(? AS VARBINARY(16)), CAST(? AS DECIMAL(12,3)), "
                "CAST(? AS DATETIME2(4))"),
        SQL_HANDLE_STMT, stmt_);

    const std::array<ParamDescription, 5> expected = {{
        {SQL_INTEGER, 10, 0, SQL_NULLABLE},
        {SQL_WVARCHAR, 40, 0, SQL_NULLABLE},
        {SQL_VARBINARY, 16, 0, SQL_NULLABLE},
        {SQL_DECIMAL, 12, 3, SQL_NULLABLE},
        {SQL_TYPE_TIMESTAMP, 24, 4, SQL_NULLABLE},
    }};

    for (SQLUSMALLINT ordinal = 1; ordinal <= expected.size(); ++ordinal) {
        ParamDescription actual = Describe(ordinal);
        const ParamDescription& wanted = expected[ordinal - 1];
        EXPECT_EQ(wanted.data_type, actual.data_type);
        EXPECT_EQ(wanted.size, actual.size);
        EXPECT_EQ(wanted.scale, actual.scale);
        EXPECT_EQ(wanted.nullable, actual.nullable);
    }
}

TEST_F(DescribeParamLiveTest, ExecutesMssqlPythonDefaultNullPath) {
    ASSERT_SQL_OK(Prepare("SELECT ISNULL(?, 42)"), SQL_HANDLE_STMT, stmt_);

    ParamDescription description = Describe(1);
    ASSERT_EQ(SQL_INTEGER, description.data_type);

    SQLLEN indicator = SQL_NULL_DATA;
    BindDefaultNull(1, description, indicator);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("42", GetColumn(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

TEST_F(DescribeParamLiveTest, DescribesAllNullsBeforeBinding) {
    ASSERT_SQL_OK(Prepare("SELECT ISNULL(?, 7), CAST(? AS NVARCHAR(8))"),
                  SQL_HANDLE_STMT, stmt_);

    ParamDescription first = Describe(1);
    ParamDescription second = Describe(2);
    ASSERT_EQ(SQL_INTEGER, first.data_type);
    ASSERT_EQ(SQL_WVARCHAR, second.data_type);

    SQLLEN first_indicator = SQL_NULL_DATA;
    SQLLEN second_indicator = SQL_NULL_DATA;
    BindDefaultNull(1, first, first_indicator);
    BindDefaultNull(2, second, second_indicator);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("7", GetColumn(1));
    SQLLEN second_result = 0;
    GetColumn(2, &second_result);
    EXPECT_EQ(SQL_NULL_DATA, second_result);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

TEST_F(DescribeParamLiveTest, ReprepareInvalidatesMetadata) {
    ASSERT_SQL_OK(Prepare("SELECT ISNULL(?, 42)"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_INTEGER, Describe(1).data_type);

    ASSERT_SQL_OK(Prepare("SELECT COALESCE(?, N'fallback')"), SQL_HANDLE_STMT, stmt_);
    ParamDescription description = Describe(1);
    EXPECT_EQ(SQL_WVARCHAR, description.data_type);
    EXPECT_EQ(8U, description.size);
}
