// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Selected native scenarios from json_output_param_test.cpp and
// json_input_param_edge_test.cpp at msodbcsql feature/JsonFeature
// 88c4dd572852d225448b61173fe56fd59ab22e4c. No retail parity claim.
#include "odbc_test_fixture.h"
#include "odbc_concurrent_test.h"

#include <algorithm>
#include <cstdint>
#include <cstring>

#ifndef SQL_SS_JSON
#define SQL_SS_JSON (-157)
#endif

namespace {
using Bytes = std::vector<unsigned char>;

Bytes Wide(const std::u16string& text) {
    Bytes bytes;
    for (char16_t unit : text) {
        bytes.push_back(static_cast<unsigned char>(unit & 0xff));
        bytes.push_back(static_cast<unsigned char>(unit >> 8));
    }
    return bytes;
}

SQLRETURN ExecText(SQLHSTMT stmt, const std::string& sql) {
    auto text = ODBCTestUtils::ToSqlTStr(sql);
    return SQLExecDirect(stmt, text.data(), SQL_NTS);
}

SQLRETURN Drain(SQLHSTMT stmt, SQLRETURN rc) {
    SQLRETURN result = rc;
    while (SQL_SUCCEEDED(rc)) {
        rc = SQLMoreResults(stmt);
        if (rc == SQL_SUCCESS_WITH_INFO || rc == SQL_ERROR)
            result = rc;
    }
    return rc == SQL_NO_DATA ? result : rc;
}
} // namespace

class JsonParameterTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection())
            GTEST_SKIP() << "No connection configured";
        ASSERT_NO_FATAL_FAILURE(Connect());
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT,
            reinterpret_cast<SQLPOINTER>(static_cast<intptr_t>(60)), 0),
            SQL_HANDLE_STMT, stmt_);
        SQLTCHAR version[64] = {};
        ASSERT_SQL_OK(SQLGetInfo(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
            SQL_HANDLE_DBC, dbc_);
        RecordProperty("SQL_DRIVER_VER", ODBCTestUtils::ToNarrow(version));
        SQLRETURN rc = ExecText(stmt_, "CREATE TABLE #json_support_probe (j json)");
        if (!SQL_SUCCEEDED(rc)) {
            SQLCHAR state[6] = {};
            SQLCHAR message[SQL_MAX_MESSAGE_LENGTH] = {};
            SQLINTEGER native = 0;
            SQLGetDiagRecA(SQL_HANDLE_STMT, stmt_, 1, state, &native, message,
                SQL_MAX_MESSAGE_LENGTH, nullptr);
            if (std::string(reinterpret_cast<char*>(state)) == "42000" &&
                (native == 2715 || native == 243))
                GTEST_SKIP() << "Server does not support json";
            FAIL() << "JSON capability probe failed: " << message;
        }
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
        for (const char* sql : {
            "CREATE PROCEDURE #jv_out @j json OUTPUT AS SET @j = CAST(N'{\"a\":1}' AS json)",
            "CREATE PROCEDURE #jv_null @j json OUTPUT AS SET @j = NULL",
            "CREATE PROCEDURE #jv_in @j json AS BEGIN SET NOCOUNT ON; SELECT CAST(@j AS nvarchar(max)) END",
            "CREATE PROCEDURE #jv_inout @j json OUTPUT AS "
            "SET @j = CAST(CONCAT(N'[', CAST(@j AS nvarchar(max)), N']') AS json)",
            "CREATE PROCEDURE #jv_rs @j json OUTPUT AS BEGIN SELECT 1 AS x; "
            "SET @j = CAST(N'[1,2]' AS json) END"
        }) {
            ASSERT_SQL_OK(ExecText(stmt_, sql), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
        }
    }

    void Reset() {
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    }
};

ODBC_TEST_F(JsonParameterTest, ReadsValueIntoEachCType) {
    for (SQLSMALLINT type : {SQL_C_WCHAR, SQL_C_DEFAULT, SQL_C_BINARY, SQL_C_CHAR}) {
        SCOPED_TRACE(type);
        Reset();
        Bytes buffer(64, 0xcc);
        SQLLEN length = -99;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, type, SQL_SS_JSON,
            0, 0, buffer.data(), buffer.size(), &length), SQL_HANDLE_STMT, stmt_);
        ASSERT_EQ(SQL_SUCCESS, Drain(stmt_, ExecText(stmt_, "{call #jv_out(?)}")));
        const Bytes expected = type == SQL_C_WCHAR || type == SQL_C_DEFAULT
            ? Wide(u"{\"a\":1}") : Bytes{'{', '"', 'a', '"', ':', '1', '}'};
        EXPECT_EQ(static_cast<SQLLEN>(expected.size()), length);
        EXPECT_TRUE(std::equal(expected.begin(), expected.end(), buffer.begin()));
        if (type != SQL_C_BINARY)
            EXPECT_EQ(0, buffer[expected.size()]);
    }
}

ODBC_TEST_F(JsonParameterTest, ReadsNullIntoEachCType) {
    for (SQLSMALLINT type : {SQL_C_WCHAR, SQL_C_DEFAULT, SQL_C_BINARY, SQL_C_CHAR}) {
        Reset();
        Bytes buffer(64, 0xcc);
        SQLLEN length = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, type, SQL_SS_JSON,
            0, 0, buffer.data(), buffer.size(), &length), SQL_HANDLE_STMT, stmt_);
        ASSERT_EQ(SQL_SUCCESS, Drain(stmt_, ExecText(stmt_, "{call #jv_null(?)}")));
        EXPECT_EQ(SQL_NULL_DATA, length);
        EXPECT_EQ(0xcc, buffer[0]);
    }
}

ODBC_TEST_F(JsonParameterTest, TruncatedWCharOutputNeverEndsInALoneHighSurrogate) {
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE PROCEDURE #jv_pairs @j json OUTPUT, "
        "@w nvarchar(max) OUTPUT AS BEGIN SET @w = N'[\"' + NCHAR(0xD83D) + "
        "NCHAR(0xDE00) + NCHAR(0xD83D) + NCHAR(0xDE00) + N'\"]'; "
        "SET @j = CAST(@w AS json) END"), SQL_HANDLE_STMT, stmt_);
    const std::u16string full = {u'[', u'"', 0xd83d, 0xde00, 0xd83d, 0xde00, u'"', u']'};
    for (SQLSMALLINT type : {SQL_C_WCHAR, SQL_C_DEFAULT}) {
        for (size_t size = 4; size <= 16; size += 2) {
            SCOPED_TRACE(type);
            SCOPED_TRACE(size);
            size_t units = size / 2 - 1;
            if (units > 0 && full[units - 1] >= 0xd800 && full[units - 1] <= 0xdbff)
                --units;
            const Bytes expected = Wide(full.substr(0, units));
            Reset();
            Bytes json(size, 0xcc);
            Bytes nvarchar(size, 0xcc);
            SQLLEN jsonLength = -99;
            SQLLEN wideLength = -99;
            ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, type, SQL_SS_JSON,
                0, 0, json.data(), size, &jsonLength), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_OUTPUT, SQL_C_WCHAR,
                SQL_WVARCHAR, 4000, 0, nvarchar.data(), size, &wideLength),
                SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(SQL_SUCCESS_WITH_INFO, Drain(stmt_, ExecText(stmt_, "{call #jv_pairs(?,?)}")));
            for (const auto* buffer : {&json, &nvarchar}) {
                EXPECT_TRUE(std::equal(expected.begin(), expected.end(), buffer->begin()));
                EXPECT_EQ(0, (*buffer)[expected.size()]);
                EXPECT_EQ(0, (*buffer)[expected.size() + 1]);
            }
        }
    }
}

// Benefits-from-mock-tds: distinguish native 0xF4 from text-backed parameters.
ODBC_TEST_F(JsonParameterTest, SendsNativeJsonInputAndInputOutput) {
    Bytes input = Wide(u"{\"k\":\"\u00e9\u20ac\U0001f600\"}");
    SQLLEN length = static_cast<SQLLEN>(input.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_SS_JSON,
        1, 0, input.data(), input.size(), &length), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(ExecText(stmt_, "{call #jv_in(?)}"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    Bytes echoed(256);
    SQLLEN echoedLength = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, echoed.data(), echoed.size(),
        &echoedLength), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(length, echoedLength);
    EXPECT_TRUE(std::equal(input.begin(), input.end(), echoed.begin()));
    Reset();
    Bytes inout(512);
    std::copy(input.begin(), input.end(), inout.begin());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT_OUTPUT, SQL_C_WCHAR,
        SQL_SS_JSON, 0, 0, inout.data(), inout.size(), &length), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, Drain(stmt_, ExecText(stmt_, "{call #jv_inout(?)}")));
    const auto expected = Wide(u"[{\"k\":\"\u00e9\u20ac\U0001f600\"}]");
    EXPECT_EQ(static_cast<SQLLEN>(expected.size()), length);
    EXPECT_TRUE(std::equal(expected.begin(), expected.end(), inout.begin()));
}

ODBC_TEST_F(JsonParameterTest, SendsNullJsonInput) {
    SQLLEN indicator = SQL_NULL_DATA;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
        SQL_SS_JSON, 0, 0, nullptr, 0, &indicator), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(ExecText(stmt_, "{call #jv_in(?)}"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR buffer[8] = {};
    SQLLEN length = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, buffer, sizeof(buffer), &length),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NULL_DATA, length);
}

ODBC_TEST_F(JsonParameterTest, ReadsValueOnEveryExecutionOfAPreparedCall) {
    Bytes buffer(256);
    SQLLEN length = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_WCHAR,
        SQL_SS_JSON, 0, 0, buffer.data(), buffer.size(), &length),
        SQL_HANDLE_STMT, stmt_);
    auto sql = ODBCTestUtils::ToSqlTStr("{call #jv_out(?)}");
    ASSERT_SQL_OK(SQLPrepare(stmt_, sql.data(), SQL_NTS), SQL_HANDLE_STMT, stmt_);
    for (int run = 0; run < 3; ++run) {
        std::fill(buffer.begin(), buffer.end(), 0xcc);
        ASSERT_EQ(SQL_SUCCESS, Drain(stmt_, SQLExecute(stmt_)));
        EXPECT_EQ(14, length);
        const auto expected = Wide(u"{\"a\":1}");
        EXPECT_TRUE(std::equal(expected.begin(), expected.end(), buffer.begin()));
    }
}

ODBC_TEST_F(JsonParameterTest, OutputArrivesAfterAllResultSets) {
    Bytes buffer(64, 0xcc);
    SQLLEN length = -99;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_WCHAR,
        SQL_SS_JSON, 0, 0, buffer.data(), buffer.size(), &length),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(ExecText(stmt_, "{call #jv_rs(?)}"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-99, length);
    EXPECT_EQ(0xcc, buffer[0]);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-99, length);
    ASSERT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    ASSERT_SQL_OK(Drain(stmt_, SQL_SUCCESS), SQL_HANDLE_STMT, stmt_);
    const auto expected = Wide(u"[1,2]");
    EXPECT_EQ(10, length);
    EXPECT_TRUE(std::equal(expected.begin(), expected.end(), buffer.begin()));
}

// Benefits-from-mock-tds: prove odd SQLPutData boundaries do not change UTF8 wire bytes.
ODBC_TEST_F(JsonParameterTest, SendsDataAtExecutionWideInputSplitAtEveryByteOffset) {
    const auto bytes = Wide(u"[\"\u00e9\U0001f600\"]");
    for (size_t split = 1; split < bytes.size(); ++split) {
        Reset();
        SQLLEN length = SQL_LEN_DATA_AT_EXEC(static_cast<SQLLEN>(bytes.size()));
        int token = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
            SQL_SS_JSON, 0, 0, &token, 0, &length), SQL_HANDLE_STMT, stmt_);
        ASSERT_EQ(SQL_NEED_DATA, ExecText(stmt_, "{call #jv_in(?)}"));
        SQLPOINTER requested = nullptr;
        ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &requested));
        EXPECT_EQ(&token, requested);
        ASSERT_SQL_OK(SQLPutData(stmt_, const_cast<unsigned char*>(bytes.data()), split),
            SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLPutData(stmt_, const_cast<unsigned char*>(bytes.data() + split),
            bytes.size() - split), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLParamData(stmt_, &requested), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        Bytes actual(256);
        SQLLEN actualLength = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, actual.data(), actual.size(),
            &actualLength), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(static_cast<SQLLEN>(bytes.size()), actualLength);
        EXPECT_TRUE(std::equal(bytes.begin(), bytes.end(), actual.begin()));
    }
}
