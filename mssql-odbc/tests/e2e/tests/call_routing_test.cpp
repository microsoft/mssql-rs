// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"

#include <array>
#include <string>

class CallRoutingTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection());
        Connect();
    }

    SQLRETURN Direct(const std::string& sql) {
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    SQLRETURN Prepare(const std::string& sql) {
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    void BindInt(SQLUSMALLINT ordinal, SQLSMALLINT direction,
                 SQLINTEGER& value, SQLLEN& length) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, ordinal, direction, SQL_C_SLONG,
                                      SQL_INTEGER, 10, 0, &value, 0, &length),
                      SQL_HANDLE_STMT, stmt_);
    }

    void Exhaust() {
        SQLRETURN rc;
        while (SQL_SUCCEEDED(rc = SQLMoreResults(stmt_))) {}
        ASSERT_EQ(SQL_NO_DATA, rc) << StmtDiagState();
    }
};

TEST_F(CallRoutingTest, OutputOnlyIgnoresDestinationContents) {
    ExecDirect("CREATE PROCEDURE #route @v varchar(8) OUTPUT AS SET @v='ok'");
    for (bool with_indicator : {true, false}) {
        SCOPED_TRACE(with_indicator);
        std::array<char, 8> value;
        value.fill('x');
        SQLLEN length = SQL_DATA_AT_EXEC;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_CHAR,
                                      SQL_VARCHAR, value.size(), 0, value.data(),
                                      value.size(), with_indicator ? &length : nullptr),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(Direct("{call #route(?)}"), SQL_HANDLE_STMT, stmt_);
        Exhaust();
        EXPECT_EQ(std::string("ok"), value.data());
        if (with_indicator) EXPECT_EQ(2, length);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }
}

// Benefits-from-mock-tds: assert named text RPC declarations for both streamed
// and buffered DAE, rather than just the result and return-value round trip.
TEST_F(CallRoutingTest, DataAtExecutionKeepsNamedArgumentsAndReturnVariable) {
    ExecDirect("CREATE PROCEDURE #route @v varchar(max) AS "
               "SELECT @v; RETURN 37");
    for (SQLSMALLINT sql_type : {SQL_VARCHAR, SQL_INTEGER}) {
        for (bool returns_status : {false, true}) {
            SCOPED_TRACE(sql_type);
            SCOPED_TRACE(returns_status);
            SQLINTEGER status = -1;
            SQLLEN status_length = SQL_DATA_AT_EXEC;
            if (returns_status) BindInt(1, SQL_PARAM_OUTPUT, status, status_length);
            char token = 0;
            SQLLEN length = SQL_DATA_AT_EXEC;
            ASSERT_SQL_OK(SQLBindParameter(stmt_, returns_status ? 2 : 1,
                                          SQL_PARAM_INPUT, SQL_C_CHAR, sql_type,
                                          sql_type == SQL_INTEGER ? 10 : 0, 0,
                                          &token, 0, &length),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_EQ(SQL_NEED_DATA, Direct(returns_status
                ? "{?=call #route(?)}" : "{call #route(?)}")) << StmtDiagState();
            SQLPOINTER requested = nullptr;
            ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &requested));
            EXPECT_EQ(&token, requested);
            char payload[] = "42";
            ASSERT_SQL_OK(SQLPutData(stmt_, payload, 2), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLParamData(stmt_, &requested), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
            char result[8] = {};
            SQLLEN result_length = 0;
            ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, result, sizeof(result),
                                    &result_length), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(std::string("42"), result);
            EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
            Exhaust();
            if (returns_status) {
                EXPECT_EQ(37, status);
                EXPECT_EQ(sizeof(SQLINTEGER), status_length);
            }
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
        }
    }
}

TEST_F(CallRoutingTest, DefaultArgumentsPreservePositions) {
    ExecDirect("CREATE PROCEDURE #route @a int=10,@b int=20,@c int=30 AS "
               "SELECT @a*100+@b*10+@c");
    SQLINTEGER first = 1, last = 3;
    SQLLEN first_length = 0, last_length = 0;
    BindInt(1, SQL_PARAM_INPUT, first, first_length);
    BindInt(2, SQL_PARAM_INPUT, last, last_length);
    for (const char* sql : {"{call #route(?,DEFAULT,?)}", "{call #route(?,,?)}"}) {
        SCOPED_TRACE(sql);
        ASSERT_SQL_OK(Direct(sql), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER result = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &result, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(303, result);
        Exhaust();
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }
}

TEST_F(CallRoutingTest, TextCallsAnnotateOutputArguments) {
    ExecDirect("CREATE PROCEDURE #route @a int,@b int OUTPUT AS SET @b=@a+5");
    for (bool prepared : {false, true}) {
        for (SQLSMALLINT direction : {SQL_PARAM_OUTPUT, SQL_PARAM_INPUT_OUTPUT}) {
            SCOPED_TRACE(prepared);
            SCOPED_TRACE(direction);
            SQLINTEGER output = -1;
            SQLLEN length = 0;
            BindInt(1, direction, output, length);
            if (prepared) {
                ASSERT_SQL_OK(Prepare("{call #route(7,?)}"), SQL_HANDLE_STMT, stmt_);
                ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
            } else {
                ASSERT_SQL_OK(Direct("{call #route(7,?)}"), SQL_HANDLE_STMT, stmt_);
            }
            Exhaust();
            EXPECT_EQ(12, output);
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
        }
    }
}

TEST_F(CallRoutingTest, EmbeddedCallUsesGlobalMarkerOrdinals) {
    ExecDirect("CREATE PROCEDURE #route @a int,@b int OUTPUT AS SET @b=@a+5");
    SQLINTEGER input = 7, output = -1;
    SQLLEN input_length = 0, output_length = 0;
    BindInt(1, SQL_PARAM_INPUT, input, input_length);
    BindInt(2, SQL_PARAM_OUTPUT, output, output_length);
    ASSERT_SQL_OK(Direct("DECLARE @unused int=?; {call #route(7,?)}"),
                  SQL_HANDLE_STMT, stmt_);
    Exhaust();
    EXPECT_EQ(12, output);
}

TEST_F(CallRoutingTest, PreparedDirectionChangesRebuildCallSite) {
    ExecDirect("CREATE PROCEDURE #route @v int OUTPUT AS SET @v=91");
    ASSERT_SQL_OK(Prepare("{call #route(?)}"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 1;
    SQLLEN length = 0;
    for (SQLSMALLINT direction : {SQL_PARAM_INPUT, SQL_PARAM_OUTPUT,
                                 SQL_PARAM_INPUT_OUTPUT, SQL_PARAM_INPUT}) {
        SCOPED_TRACE(direction);
        value = 1;
        BindInt(1, direction, value, length);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        Exhaust();
        EXPECT_EQ(direction == SQL_PARAM_INPUT ? 1 : 91, value);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }
}

TEST_F(CallRoutingTest, TextReturnAssignmentDoesNotUseWrapperStatus) {
    ExecDirect("CREATE PROCEDURE #route @v int AS RETURN @v");
    for (bool prepared : {false, true}) {
        SQLINTEGER status = -1;
        SQLLEN length = 0;
        BindInt(1, SQL_PARAM_OUTPUT, status, length);
        if (prepared) {
            ASSERT_SQL_OK(Prepare("{?=call #route(37)}"), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        } else {
            ASSERT_SQL_OK(Direct("{?=call #route(37)}"), SQL_HANDLE_STMT, stmt_);
        }
        Exhaust();
        EXPECT_EQ(37, status);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }
}

TEST_F(CallRoutingTest, PreparedOutputDoesNotInheritPriorRpcReturnMapping) {
    ExecDirect("CREATE PROCEDURE #status AS RETURN 37");
    ExecDirect("CREATE PROCEDURE #output @v int OUTPUT AS SET @v=91");
    SQLINTEGER value = -1;
    SQLLEN length = 0;
    BindInt(1, SQL_PARAM_OUTPUT, value, length);
    ASSERT_SQL_OK(Direct("{?=call #status}"), SQL_HANDLE_STMT, stmt_);
    Exhaust();
    EXPECT_EQ(37, value);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(Prepare("{call #output(?)}"), SQL_HANDLE_STMT, stmt_);
    value = -1;
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    Exhaust();
    EXPECT_EQ(91, value);
}
