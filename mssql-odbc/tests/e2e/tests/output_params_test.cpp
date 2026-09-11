// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"

#include <array>
#include <string>

class OutputParamsTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection());
        Connect();
        SQLCHAR version[32] = {};
        ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                      SQL_HANDLE_DBC, dbc_);
        RecordProperty("driver_version", reinterpret_cast<const char*>(version));
    }

    SQLRETURN Direct(const std::string& sql, SQLHSTMT stmt = SQL_NULL_HSTMT) {
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt == SQL_NULL_HSTMT ? stmt_ : stmt,
                             const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    void BindInt(SQLUSMALLINT ordinal, SQLINTEGER& value, SQLLEN& length) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, ordinal, SQL_PARAM_OUTPUT,
                                      SQL_C_SLONG, SQL_INTEGER, 10, 0,
                                      &value, 0, &length),
                      SQL_HANDLE_STMT, stmt_);
    }

    SQLRETURN Exhaust(SQLRETURN rc = SQL_SUCCESS) {
        while (rc == SQL_SUCCESS) rc = SQLMoreResults(stmt_);
        return rc;
    }

    void FetchOnlyRow() {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER row = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &row, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(1, row);
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    }
};

// Benefits-from-mock-tds: assert the return tokens are captured before the
// connection's client is published idle, not just the later buffer contents.
TEST_F(OutputParamsTest, PendingValuesSurviveAnotherStatementUsingTheConnection) {
    ExecDirect("CREATE PROCEDURE #outputs @v int OUTPUT AS "
               "SET NOCOUNT ON; SELECT 1; SET @v=73; RETURN 19");
    SQLINTEGER status = -1, output = -1;
    SQLLEN status_length = -1, output_length = -1;
    BindInt(1, status, status_length);
    BindInt(2, output, output_length);
    ASSERT_SQL_OK(Direct("{?=call #outputs(?)}"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, output);
    EXPECT_EQ(-1, status);
    FetchOnlyRow();

    SQLHSTMT other = AllocStmt();
    ASSERT_NE(nullptr, other);
    ASSERT_SQL_OK(Direct("SELECT 2", other), SQL_HANDLE_STMT, other);
    EXPECT_EQ(SQL_NO_DATA, Exhaust());
    EXPECT_EQ(73, output);
    EXPECT_EQ(19, status);
    EXPECT_EQ(sizeof(SQLINTEGER), output_length);
    EXPECT_EQ(sizeof(SQLINTEGER), status_length);
    output = -2;
    status = -2;
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    EXPECT_EQ(-2, output);
    EXPECT_EQ(-2, status);
    FreeStmt(other);
}

TEST_F(OutputParamsTest, ResetBindingsDiscardsPendingWrites) {
    ExecDirect("CREATE PROCEDURE #outputs @v int OUTPUT AS "
               "SET NOCOUNT ON; SELECT 1; SET @v=73");
    SQLINTEGER output = -1;
    SQLLEN length = -1;
    BindInt(1, output, length);
    ASSERT_SQL_OK(Direct("{call #outputs(?)}"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, output);
    // Change bindings before fetching can consume the RPC return tokens.
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    FetchOnlyRow();
    EXPECT_EQ(SQL_NO_DATA, Exhaust());
    EXPECT_EQ(-1, output);
    EXPECT_EQ(-1, length);
}

TEST_F(OutputParamsTest, RebindingRedirectsPendingWrites) {
    ExecDirect("CREATE PROCEDURE #outputs @v int OUTPUT AS "
               "SET NOCOUNT ON; SELECT 1; SET @v=73");
    SQLINTEGER old_output = -1, new_output = -2;
    SQLLEN old_length = -1, new_length = -2;
    BindInt(1, old_output, old_length);
    ASSERT_SQL_OK(Direct("{call #outputs(?)}"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, old_output);
    BindInt(1, new_output, new_length);
    FetchOnlyRow();
    EXPECT_EQ(SQL_NO_DATA, Exhaust());
    EXPECT_EQ(-1, old_output);
    EXPECT_EQ(-1, old_length);
    EXPECT_EQ(73, new_output);
    EXPECT_EQ(sizeof(SQLINTEGER), new_length);
}

TEST_F(OutputParamsTest, CurrentBindOffsetDisplacesAllDescriptorPointers) {
    ExecDirect("CREATE PROCEDURE #outputs @v int OUTPUT AS "
               "SET NOCOUNT ON; SELECT 1; SET @v=73");
    std::array<SQLINTEGER, 4> values = {-1, -1, -1, -1};
    std::array<SQLLEN, 2> indicators = {-1, -1}, lengths = {-1, -1};
    SQLLEN offset = 0;
    BindInt(1, values[0], indicators[0]);
    SQLHDESC apd = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_APP_PARAM_DESC, &apd, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetDescField(apd, 1, SQL_DESC_OCTET_LENGTH_PTR, lengths.data(), 0),
                  SQL_HANDLE_DESC, apd);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAM_BIND_OFFSET_PTR, &offset, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(Direct("{call #outputs(?)}"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, values[0]);
    offset = sizeof(SQLLEN);
    FetchOnlyRow();
    EXPECT_EQ(SQL_NO_DATA, Exhaust());
    EXPECT_EQ(-1, values[0]);
    EXPECT_EQ(73, values[sizeof(SQLLEN) / sizeof(SQLINTEGER)]);
    EXPECT_EQ(-1, indicators[0]);
    EXPECT_EQ(-1, lengths[0]);
    EXPECT_EQ(0, indicators[1]);
    EXPECT_EQ(sizeof(SQLINTEGER), lengths[1]);
}

TEST_F(OutputParamsTest, OutputConversionErrorsReachTheApiReturnCode) {
    ExecDirect("CREATE PROCEDURE #outputs @v varchar(8) OUTPUT AS "
               "SET NOCOUNT ON; SET @v='invalid'");
    SQLINTEGER output = -1;
    SQLLEN length = -1;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_SLONG,
                                  SQL_VARCHAR, 8, 0, &output, 0, &length),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, Exhaust(Direct("{call #outputs(?)}")));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
    EXPECT_EQ(-1, output);
}

TEST_F(OutputParamsTest, StringTruncationReachesTheApiReturnCode) {
    ExecDirect("CREATE PROCEDURE #outputs @v varchar(8) OUTPUT AS "
               "SET NOCOUNT ON; SET @v='abcdefgh'");
    char output[4] = {};
    SQLLEN length = -1;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_CHAR,
                                  SQL_VARCHAR, 8, 0, output, sizeof(output), &length),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, Exhaust(Direct("{call #outputs(?)}")));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_STREQ("abc", output);
    EXPECT_EQ(8, length);
}

TEST_F(OutputParamsTest, FractionalTruncationIsNotStringTruncation) {
    ExecDirect("CREATE PROCEDURE #outputs @v float OUTPUT AS "
               "SET NOCOUNT ON; SELECT 1; SET @v=12.75");
    SQLDOUBLE original_output = -1;
    SQLINTEGER output = -1;
    SQLLEN length = -1;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_DOUBLE,
                                  SQL_DOUBLE, 15, 0, &original_output, 0, &length),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(Direct("{call #outputs(?)}"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, original_output);
    BindInt(1, output, length);
    const SQLRETURN fractional_rc = Exhaust();
    // Retail 18.06.0001 reports the warning alongside SQL_NO_DATA.
    EXPECT_TRUE(fractional_rc == SQL_SUCCESS_WITH_INFO || fractional_rc == SQL_NO_DATA);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S07");
    EXPECT_EQ(12, output);
}

TEST_F(OutputParamsTest, ExhaustedFastPathReportsConversionWarningsAndErrorsOnce) {
    ExecDirect("CREATE PROCEDURE #outputs @v varchar(8) OUTPUT AS "
               "SET NOCOUNT ON; SELECT 1; SET @v='abcdefgh'");
    for (bool invalid_conversion : {false, true}) {
        SCOPED_TRACE(invalid_conversion);
        std::array<char, 4> output = {};
        SQLLEN length = -1;
        ASSERT_SQL_OK(SQLBindParameter(
                          stmt_, 1, SQL_PARAM_OUTPUT,
                          invalid_conversion ? SQL_C_SLONG : SQL_C_CHAR,
                          SQL_VARCHAR, 8, 0, output.data(), output.size(), &length),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(Direct("{call #outputs(?)}"), SQL_HANDLE_STMT, stmt_);
        const SQLRETURN expected = invalid_conversion ? SQL_ERROR : SQL_SUCCESS_WITH_INFO;
        int outcomes = 0;
        // Inspect diagnostics at the call that consumes the return tokens:
        // retail msodbcsql can do that during fetch, before SQLMoreResults.
        auto observe = [&](SQLRETURN rc) {
            const auto state = StmtDiagState();
            if (!state.empty()) {
                EXPECT_EQ(invalid_conversion ? "22018" : "01004", state);
                EXPECT_TRUE(rc == expected || (!invalid_conversion && rc == SQL_NO_DATA));
                ++outcomes;
            } else {
                EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_NO_DATA);
            }
        };
        SQLRETURN rc;
        do {
            rc = SQLFetch(stmt_);
            observe(rc);
        } while (SQL_SUCCEEDED(rc));
        rc = SQLMoreResults(stmt_);
        observe(rc);
        EXPECT_TRUE(rc == expected || rc == SQL_NO_DATA);
        EXPECT_EQ(1, outcomes);
        if (invalid_conversion) {
            EXPECT_EQ((std::array<char, 4>{}), output);
        } else {
            EXPECT_STREQ("abc", output.data());
            EXPECT_EQ(8, length);
        }
        EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }
}
