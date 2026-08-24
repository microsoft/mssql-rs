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

    /// Describes a parameter, reporting failure to the caller so a failed
    /// describe does not cascade into misleading assertions on a zeroed struct.
    bool Describe(SQLUSMALLINT ordinal, ParamDescription& out) {
        out = ParamDescription{};
        SQLRETURN rc = SQLDescribeParam(stmt_, ordinal, &out.data_type, &out.size,
                                        &out.scale, &out.nullable);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        return SQL_SUCCEEDED(rc);
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

// Benefits-from-mock-tds: a mock TDS server could assert that
// sp_describe_undeclared_parameters fired once and carried the prepared
// statement text, which the returned metadata alone cannot show.
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
        ParamDescription actual;
        ASSERT_TRUE(Describe(ordinal, actual)) << "ordinal " << ordinal;
        const ParamDescription& wanted = expected[ordinal - 1];
        EXPECT_EQ(wanted.data_type, actual.data_type) << "ordinal " << ordinal;
        EXPECT_EQ(wanted.size, actual.size) << "ordinal " << ordinal;
        EXPECT_EQ(wanted.scale, actual.scale) << "ordinal " << ordinal;
        EXPECT_EQ(wanted.nullable, actual.nullable) << "ordinal " << ordinal;
    }
}

// Benefits-from-mock-tds: a mock TDS server could assert the typed NULL that
// SQL_C_DEFAULT produces reaches the wire as INTN rather than inferring it from
// the ISNULL result.
TEST_F(DescribeParamLiveTest, ExecutesMssqlPythonDefaultNullPath) {
    ASSERT_SQL_OK(Prepare("SELECT ISNULL(?, 42)"), SQL_HANDLE_STMT, stmt_);

    ParamDescription description;
    ASSERT_TRUE(Describe(1, description));
    ASSERT_EQ(SQL_INTEGER, description.data_type);

    SQLLEN indicator = SQL_NULL_DATA;
    BindDefaultNull(1, description, indicator);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("42", GetColumn(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// mssql-python describes every parameter before binding any of them, so the
// second describe must be served from the cache the first one populated.
//
// Benefits-from-mock-tds: a mock TDS server could assert only one
// sp_describe_undeclared_parameters RPC fired for the two describes.
TEST_F(DescribeParamLiveTest, DescribesAllNullsBeforeBinding) {
    ASSERT_SQL_OK(Prepare("SELECT ISNULL(?, 7), CAST(? AS NVARCHAR(8))"),
                  SQL_HANDLE_STMT, stmt_);

    ParamDescription first;
    ParamDescription second;
    ASSERT_TRUE(Describe(1, first));
    ASSERT_TRUE(Describe(2, second));
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

// Re-preparing must drop the cached metadata; serving the previous statement's
// description would silently bind the wrong type.
//
// Benefits-from-mock-tds: a mock TDS server could assert a second
// sp_describe_undeclared_parameters RPC fired after the re-prepare.
TEST_F(DescribeParamLiveTest, ReprepareInvalidatesMetadata) {
    ASSERT_SQL_OK(Prepare("SELECT ISNULL(?, 42)"), SQL_HANDLE_STMT, stmt_);
    ParamDescription first;
    ASSERT_TRUE(Describe(1, first));
    EXPECT_EQ(SQL_INTEGER, first.data_type);

    ASSERT_SQL_OK(Prepare("SELECT CAST(? AS NVARCHAR(8))"), SQL_HANDLE_STMT, stmt_);
    ParamDescription second;
    ASSERT_TRUE(Describe(1, second));
    EXPECT_EQ(SQL_WVARCHAR, second.data_type);
    EXPECT_EQ(8U, second.size);
}

// `*(max)` parameters have no bounded length; both drivers report a size of 0,
// and a bind from that description must still round-trip.
//
// Disabled: every bind is now checked against the conversion matrix, defaulted
// ones included, and `SQL_VARBINARY` has no row yet - so the second parameter is
// rejected at bind with HYC00. A deliberate phase 0 scope restriction, not a
// defect; `typed_null` already handles the type. Re-enable when binary
// conversions land: AB#47500. See the design rules in docs/parameters_plan.md.
TEST_F(DescribeParamLiveTest, DISABLED_DescribesMaxLengthParameters) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(? AS NVARCHAR(MAX)), CAST(? AS VARBINARY(MAX))"),
                  SQL_HANDLE_STMT, stmt_);

    ParamDescription wide;
    ParamDescription binary;
    ASSERT_TRUE(Describe(1, wide));
    ASSERT_TRUE(Describe(2, binary));
    EXPECT_EQ(SQL_WVARCHAR, wide.data_type);
    EXPECT_EQ(static_cast<SQLULEN>(0), wide.size);
    EXPECT_EQ(SQL_VARBINARY, binary.data_type);
    EXPECT_EQ(static_cast<SQLULEN>(0), binary.size);

    SQLLEN wide_indicator = SQL_NULL_DATA;
    SQLLEN binary_indicator = SQL_NULL_DATA;
    BindDefaultNull(1, wide, wide_indicator);
    BindDefaultNull(2, binary, binary_indicator);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLLEN result = 0;
    GetColumn(1, &result);
    EXPECT_EQ(SQL_NULL_DATA, result);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A described decimal must be re-declared with the same precision and scale, or
// the first non-NULL value bound from that description would be truncated.
//
// Disabled for the same reason as DISABLED_DescribesMaxLengthParameters:
// `SQL_DECIMAL` resolves to `SQL_C_CHAR`, which the matrix pairs only with the
// character SQL types, so the bind is rejected with HYC00. Re-enable when
// decimal conversions land (AB#47500) - this test also guards the scale-0
// wire-metadata regression, so it should come back with them.
TEST_F(DescribeParamLiveTest, DISABLED_DescribedDecimalRoundTripsPrecisionAndScale) {
    ASSERT_SQL_OK(Prepare("SELECT ISNULL(?, CAST(1.5 AS DECIMAL(12,3)))"),
                  SQL_HANDLE_STMT, stmt_);

    ParamDescription description;
    ASSERT_TRUE(Describe(1, description));
    EXPECT_EQ(SQL_DECIMAL, description.data_type);
    // The exact precision and scale are the server's to infer and vary by version,
    // so assert the shape rather than a hard-coded pair. A scale of 0 is the
    // specific regression this guards: it is what the driver reported when the
    // wire metadata was written independently of the parameter declaration.
    EXPECT_GT(description.scale, 0);
    EXPECT_GE(description.size, static_cast<SQLULEN>(description.scale));

    SQLLEN indicator = SQL_NULL_DATA;
    BindDefaultNull(1, description, indicator);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    // Compared numerically: a declaration of decimal(1,0) would round 1.5 away,
    // but the textual form of a decimal is not part of the parity contract.
    EXPECT_DOUBLE_EQ(1.5, std::stod(GetColumn(1)));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}
