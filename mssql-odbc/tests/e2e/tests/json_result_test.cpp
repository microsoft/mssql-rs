// Copyright (c) Microsoft Corporation. All rights reserved.
// Native JSON result scenarios from msodbcsql feature/JsonFeature at
// 88c4dd572852d225448b61173fe56fd59ab22e4c: json_result_test.cpp and
// json_fetch_parity_test.cpp. Requires a JSON-enabled reference driver.

#include "odbc_concurrent_test.h"
#include "odbc_test_fixture.h"
#include "odbc_sql_server_types.h"

#include <algorithm>
#include <array>
#include <chrono>
#include <cstring>
#include <string>
#include <vector>

class JsonResultTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "Native JSON results need a configured JSON-capable server";
        }
        Connect();
        ASSERT_SQL_OK(Exec("SELECT TYPE_ID('json')"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER typeId = 0;
        SQLLEN indicator = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &typeId, sizeof(typeId), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        if (indicator == SQL_NULL_DATA) {
            GTEST_SKIP() << "Server has no native JSON type";
        }
    }

    SQLRETURN Exec(const std::string& sql) {
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    void Select(const std::string& expression, SQLINTEGER following = 42) {
        ASSERT_SQL_OK(Exec("SELECT CAST(" + expression + " AS json) AS j, " +
                          std::to_string(following) + " AS following"),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    }

    SQLLEN Attribute(SQLUSMALLINT field) {
        SQLLEN value = -99;
        EXPECT_SQL_OK(SQLColAttribute(stmt_, 1, field, nullptr, 0, nullptr, &value),
                      SQL_HANDLE_STMT, stmt_);
        return value;
    }

    std::vector<unsigned char> Drain(SQLSMALLINT target, SQLLEN capacity) {
        std::vector<unsigned char> value;
        auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(30);
        while (std::chrono::steady_clock::now() < deadline) {
            std::array<unsigned char, 128> output;
            output.fill(0xCD);
            SQLLEN indicator = -99;
            SQLRETURN rc = SQLGetData(stmt_, 1, target, output.data(), capacity, &indicator);
            EXPECT_TRUE(SQL_SUCCEEDED(rc))
                << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
            if (!SQL_SUCCEEDED(rc)) {
                return value;
            }
            const bool wide = target == SQL_C_WCHAR || target == SQL_C_DEFAULT;
            size_t written = 0;
            if (target == SQL_C_BINARY) {
                written = rc == SQL_SUCCESS_WITH_INFO ? static_cast<size_t>(capacity)
                                                     : static_cast<size_t>(indicator);
            } else if (wide) {
                for (; written + 1 < static_cast<size_t>(capacity); written += 2) {
                    if (output[written] == 0 && output[written + 1] == 0) {
                        break;
                    }
                }
            } else {
                while (written < static_cast<size_t>(capacity) && output[written] != 0) {
                    ++written;
                }
            }
            EXPECT_LE(written, static_cast<size_t>(capacity));
            EXPECT_TRUE(std::all_of(output.begin() + capacity, output.end(),
                                    [](unsigned char byte) { return byte == 0xCD; }));
            value.insert(value.end(), output.begin(), output.begin() + written);
            if (rc == SQL_SUCCESS) {
                EXPECT_EQ(static_cast<SQLLEN>(written), indicator);
                EXPECT_EQ(SQL_NO_DATA, SQLGetData(stmt_, 1, target, output.data(), capacity, &indicator));
                return value;
            }
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
            EXPECT_GT(written, 0u);
            if (written != 0) {
                deadline = std::chrono::steady_clock::now() + std::chrono::seconds(30);
            }
        }
        ADD_FAILURE() << "JSON read did not finish";
        return value;
    }

    static std::vector<unsigned char> WideBytes(const std::u16string& text) {
        std::vector<unsigned char> bytes;
        for (char16_t unit : text) {
            bytes.push_back(static_cast<unsigned char>(unit));
            bytes.push_back(static_cast<unsigned char>(unit >> 8));
        }
        return bytes;
    }

    void Following(SQLINTEGER expected = 42) {
        SQLINTEGER value = 0;
        SQLLEN indicator = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &value, sizeof(value), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, value);
    }
};

ODBC_TEST_F(JsonResultTest, ColumnAndDescriptorMetadataAgree) {
    ASSERT_SQL_OK(Exec("SELECT CAST(N'{\"a\":1}' AS json) AS j"),
                  SQL_HANDLE_STMT, stmt_);
    SQLSMALLINT type = 0, scale = -1;
    SQLULEN size = 99;
    ASSERT_SQL_OK(SQLDescribeCol(stmt_, 1, nullptr, 0, nullptr, &type, &size, &scale, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SS_JSON, type);
    EXPECT_EQ(0u, size);
    EXPECT_EQ(0, scale);
    EXPECT_EQ(SQL_SS_JSON, Attribute(SQL_DESC_TYPE));
    EXPECT_EQ(SQL_SS_JSON, Attribute(SQL_DESC_CONCISE_TYPE));
    for (auto field : {SQL_DESC_LENGTH, SQL_DESC_OCTET_LENGTH, SQL_DESC_DISPLAY_SIZE,
                       SQL_DESC_PRECISION, SQL_DESC_SCALE, SQL_DESC_SEARCHABLE}) {
        EXPECT_EQ(0, Attribute(static_cast<SQLUSMALLINT>(field)));
    }
    SQLHDESC ird = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_IMP_ROW_DESC, &ird, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    for (auto field : {SQL_DESC_TYPE, SQL_DESC_CONCISE_TYPE}) {
        type = 0;
        ASSERT_SQL_OK(SQLGetDescField(ird, 1, static_cast<SQLSMALLINT>(field), &type, 0, nullptr),
                      SQL_HANDLE_DESC, ird);
        EXPECT_EQ(SQL_SS_JSON, type);
    }
    for (auto field : {SQL_DESC_TYPE_NAME, SQL_DESC_LOCAL_TYPE_NAME,
                       SQL_DESC_LITERAL_PREFIX, SQL_DESC_LITERAL_SUFFIX}) {
        SQLWCHAR text[32] = {};
        SQLINTEGER length = 0;
        ASSERT_SQL_OK(SQLGetDescFieldW(ird, 1, static_cast<SQLSMALLINT>(field), text, sizeof(text), &length),
                      SQL_HANDLE_DESC, ird);
        const std::u16string expected = field == SQL_DESC_TYPE_NAME || field == SQL_DESC_LOCAL_TYPE_NAME
                                           ? u"json" : u"'";
        EXPECT_EQ(static_cast<SQLINTEGER>(expected.size() * 2), length);
        EXPECT_EQ(0, std::memcmp(text, expected.data(), expected.size() * 2));
    }
}

ODBC_TEST_F(JsonResultTest, ChunkedReadsAndLengthProbesPreserveValues) {
    const std::string token = WorkerToken("json");
    const std::string ascii = "[\"" + token + std::string(12000, 'x') + "\"]";
    for (SQLSMALLINT target : {SQL_C_CHAR, SQL_C_WCHAR, SQL_C_BINARY, SQL_C_DEFAULT}) {
        SCOPED_TRACE(target);
        Select("N'[\"" + token + "' + REPLICATE(CAST(N'x' AS nvarchar(max)),12000) + N'\"]'");
        SQLLEN length = -99;
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, target, nullptr, 0, &length));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
        EXPECT_TRUE(length >= 0 || length == SQL_NO_TOTAL);
        const auto actual = Drain(target, 24);
        const auto expected = target == SQL_C_WCHAR || target == SQL_C_DEFAULT
                                  ? WideBytes(std::u16string(ascii.begin(), ascii.end()))
                                  : std::vector<unsigned char>(ascii.begin(), ascii.end());
        EXPECT_EQ(expected, actual);
        Following();
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

ODBC_TEST_F(JsonResultTest, UnicodeWideAndBinaryHaveNoBomOrLoss) {
    const SQLINTEGER following = 42 + Worker() * 10000 + Iteration();
    const std::string expression =
        "N'{\"k\":\"' + NCHAR(252) + NCHAR(8364) + NCHAR(55357) + NCHAR(56832) + N'\"}'";
    const std::u16string text = u"{\"k\":\"\u00FC\u20AC\U0001F600\"}";
    const std::string utf8 = "{\"k\":\"\xC3\xBC\xE2\x82\xAC\xF0\x9F\x98\x80\"}";
    for (SQLSMALLINT target : {SQL_C_WCHAR, SQL_C_DEFAULT, SQL_C_BINARY}) {
        Select(expression, following);
        EXPECT_EQ(target == SQL_C_BINARY ? std::vector<unsigned char>(utf8.begin(), utf8.end())
                                         : WideBytes(text),
                  Drain(target, target == SQL_C_BINARY ? 3 : 8));
        Following(following);
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

ODBC_TEST_F(JsonResultTest, NullDoesNotWriteTheApplicationBuffer) {
    const SQLINTEGER following = 42 + Worker() * 10000 + Iteration();
    for (SQLSMALLINT target : {SQL_C_CHAR, SQL_C_WCHAR, SQL_C_BINARY, SQL_C_DEFAULT}) {
        Select("NULL", following);
        std::array<unsigned char, 32> output;
        output.fill(0xCD);
        SQLLEN indicator = -99;
        EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, target, output.data(), output.size(), &indicator));
        EXPECT_EQ(SQL_NULL_DATA, indicator);
        EXPECT_TRUE(std::all_of(output.begin(), output.end(),
                                [](unsigned char byte) { return byte == 0xCD; }));
        Following(following);
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

ODBC_TEST_F(JsonResultTest, BoundTruncationNullAndNextRowStaySynchronized) {
    for (SQLSMALLINT target : {SQL_C_CHAR, SQL_C_WCHAR, SQL_C_BINARY, SQL_C_DEFAULT}) {
        ASSERT_SQL_OK(Exec("SELECT CAST(N'{\"a\":1234567890}' AS json) AS j "
                          "UNION ALL SELECT CAST(NULL AS json)"),
                      SQL_HANDLE_STMT, stmt_);
        std::array<unsigned char, 8> output;
        output.fill(0xCD);
        SQLLEN indicator = -99;
        ASSERT_SQL_OK(SQLBindCol(stmt_, 1, target, output.data(), output.size(), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
        EXPECT_TRUE(indicator > static_cast<SQLLEN>(output.size()) || indicator == SQL_NO_TOTAL);
        output.fill(0xCD);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_NULL_DATA, indicator);
        EXPECT_TRUE(std::all_of(output.begin(), output.end(),
                                [](unsigned char byte) { return byte == 0xCD; }));
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

ODBC_TEST_F(JsonResultTest, RestrictedConversionsDoNotConsumeThePendingRow) {
    ASSERT_SQL_OK(Exec("SELECT CAST(N'{\"a\":1}' AS json) AS j,42 AS following"),
                  SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = -123;
    SQLLEN indicator = -99;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_ERROR(SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07006");
    EXPECT_EQ(-123, value);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_ERROR(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07006");
    EXPECT_EQ(-123, value);
    const std::string expected = "{\"a\":1}";
    EXPECT_EQ(std::vector<unsigned char>(expected.begin(), expected.end()), Drain(SQL_C_CHAR, 24));
    Following();
}

// Benefits-from-mock-tds: assert the unread PLP tail is abandoned with ATTENTION.
ODBC_TEST_F(JsonResultTest, ClosingMidValueAndMoreResultsReplaceJsonState) {
    Select("N'[\"' + REPLICATE(CAST(N'x' AS nvarchar(max)),12000) + N'\"]'");
    unsigned char output[8] = {};
    SQLLEN indicator = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_BINARY, output, sizeof(output), &indicator));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(Exec("SELECT CAST(N'[]' AS json); SELECT 7 AS n"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ((std::vector<unsigned char>{'[', ']'}), Drain(SQL_C_BINARY, 24));
    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_INTEGER, Attribute(SQL_DESC_CONCISE_TYPE));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(7, value);
}
