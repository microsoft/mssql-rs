// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Live cases from msodbcsql feature/JsonFeature, json_tvp_edge_test.cpp
// at 88c4dd572852d225448b61173fe56fd59ab22e4c. Keep server CAST as the oracle.

#include "odbc_test_fixture.h"
#include "odbc_sql_server_types.h"

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <ctime>
#include <functional>
#include <optional>

#ifndef SQL_SS_JSON
#define SQL_SS_JSON (-157)
#endif
#ifndef SQL_SS_TABLE
#define SQL_SS_TABLE (-153)
#endif
#ifndef SQL_SOPT_SS_PARAM_FOCUS
#define SQL_SOPT_SS_PARAM_FOCUS 1236
#endif
#ifndef SQL_CA_SS_SCHEMA_NAME
#define SQL_CA_SS_SCHEMA_NAME 1226
#endif
#ifndef SQL_CA_SS_TYPE_NAME
#define SQL_CA_SS_TYPE_NAME 1227
#endif

namespace {
using Bytes = std::vector<uint8_t>;
const std::u16string kUnicodeJsonUtf16 = u"{\"k\":\"\u00E9\u20AC\U0001F600\"}";

std::vector<SQLWCHAR> Wide(const std::u16string& text) {
    return {text.begin(), text.end()};
}

Bytes WideBytes(const std::u16string& text) {
    const auto wide = Wide(text);
    Bytes bytes(wide.size() * sizeof(SQLWCHAR));
    std::memcpy(bytes.data(), wide.data(), bytes.size());
    return bytes;
}

std::string Utf8(const std::u16string& text) {
    std::string out;
    for (size_t i = 0; i < text.size(); ++i) {
        uint32_t cp = text[i];
        if (cp >= 0xD800 && cp <= 0xDBFF && i + 1 < text.size())
            cp = 0x10000 + ((cp - 0xD800) << 10) + (text[++i] - 0xDC00);
        if (cp < 0x80) out.push_back(static_cast<char>(cp));
        else if (cp < 0x800) {
            out.push_back(static_cast<char>(0xC0 | (cp >> 6)));
            out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
        } else if (cp < 0x10000) {
            out.push_back(static_cast<char>(0xE0 | (cp >> 12)));
            out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F)));
            out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
        } else {
            out.push_back(static_cast<char>(0xF0 | (cp >> 18)));
            out.push_back(static_cast<char>(0x80 | ((cp >> 12) & 0x3F)));
            out.push_back(static_cast<char>(0x80 | ((cp >> 6) & 0x3F)));
            out.push_back(static_cast<char>(0x80 | (cp & 0x3F)));
        }
    }
    return out;
}

SQLRETURN ExecText(SQLHSTMT stmt, const std::string& sql) {
    std::string text = sql;
    return SQLExecDirectA(stmt, reinterpret_cast<SQLCHAR*>(text.data()), SQL_NTS);
}

struct ColumnBuffer {
    size_t width;
    Bytes data;
    std::vector<SQLLEN> indicators;
    ColumnBuffer(size_t rows, size_t cellWidth)
        : width(cellWidth), data(rows * cellWidth), indicators(rows, 0) {}
    void Set(size_t row, const void* value, size_t length) {
        ASSERT_LE(length, width);
        std::memcpy(data.data() + row * width, value, length);
        indicators[row] = static_cast<SQLLEN>(length);
    }
    void Set(size_t row, const std::u16string& text) {
        const auto wide = Wide(text);
        Set(row, wide.data(), wide.size() * sizeof(SQLWCHAR));
    }
    void Set(size_t row, const std::string& text) { Set(row, text.data(), text.size()); }
    void SetNull(size_t row) { indicators[row] = SQL_NULL_DATA; }
};

SQLRETURN BindColumn(SQLHSTMT stmt, SQLUSMALLINT column, SQLSMALLINT cType,
                     SQLSMALLINT sqlType, SQLULEN size, ColumnBuffer& buffer) {
    return SQLBindParameter(stmt, column, SQL_PARAM_INPUT, cType, sqlType, size, 0,
                            buffer.data.data(), static_cast<SQLLEN>(buffer.width),
                            buffer.indicators.data());
}

void BindTvp(SQLHSTMT stmt, const std::string& typeName, SQLLEN rows,
             SQLLEN* rowIndicator, const std::function<void()>& bindColumns) {
    *rowIndicator = rows;
    ASSERT_SQL_OK(SQLBindParameter(stmt, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT, SQL_SS_TABLE,
                                   static_cast<SQLULEN>(rows), 0, nullptr, 0, rowIndicator),
                  SQL_HANDLE_STMT, stmt);
    SQLHDESC ipd = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt, SQL_ATTR_IMP_PARAM_DESC, &ipd, 0, nullptr),
                  SQL_HANDLE_STMT, stmt);
    auto type = Wide(std::u16string(typeName.begin(), typeName.end()));
    type.push_back(0);
    auto schema = Wide(u"dbo");
    schema.push_back(0);
    ASSERT_SQL_OK(SQLSetDescFieldW(ipd, 1, SQL_CA_SS_TYPE_NAME, type.data(), SQL_NTS),
                  SQL_HANDLE_DESC, ipd);
    ASSERT_SQL_OK(SQLSetDescFieldW(ipd, 1, SQL_CA_SS_SCHEMA_NAME, schema.data(), SQL_NTS),
                  SQL_HANDLE_DESC, ipd);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt, SQL_SOPT_SS_PARAM_FOCUS,
                                 reinterpret_cast<SQLPOINTER>(1), SQL_IS_INTEGER),
                  SQL_HANDLE_STMT, stmt);
    bindColumns();
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt, SQL_SOPT_SS_PARAM_FOCUS, nullptr, SQL_IS_INTEGER),
                  SQL_HANDLE_STMT, stmt);
}

std::vector<Bytes> Pieces(const Bytes& all, size_t size) {
    std::vector<Bytes> pieces;
    for (size_t from = 0; from < all.size(); from += size)
        pieces.emplace_back(all.begin() + static_cast<std::ptrdiff_t>(from),
                            all.begin() + static_cast<std::ptrdiff_t>((std::min)(all.size(), from + size)));
    return pieces;
}

std::string MismatchCount(const std::string& jsonColumn, const std::string& textColumn) {
    return "SELECT COUNT(*) FROM #sink WHERE NOT ((" + jsonColumn + " IS NULL AND " + textColumn +
           " IS NULL) OR (" + jsonColumn + " IS NOT NULL AND " + textColumn + " IS NOT NULL AND CAST(CAST(" +
           jsonColumn + " AS nvarchar(max)) AS varbinary(max)) = CAST(CAST(CAST(" + textColumn +
           " AS json) AS nvarchar(max)) AS varbinary(max))))";
}
} // namespace

class JsonTvpTest : public ODBCTest {
protected:
    // Permanent type/procedure names and HasFatalFailure require serial TEST_F.
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) GTEST_SKIP() << "No connection configured";
        ASSERT_NO_FATAL_FAILURE(Connect());
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT,
                                     reinterpret_cast<SQLPOINTER>(60), 0), SQL_HANDLE_STMT, stmt_);
        const SQLRETURN rc = ExecText(stmt_, "CREATE TABLE #json_tvp_probe (j json)");
        if (!SQL_SUCCEEDED(rc)) {
            SQLCHAR state[6]{}, message[SQL_MAX_MESSAGE_LENGTH]{};
            SQLINTEGER native = 0;
            SQLGetDiagRecA(SQL_HANDLE_STMT, stmt_, 1, state, &native, message, SQL_MAX_MESSAGE_LENGTH, nullptr);
            if (std::string(reinterpret_cast<char*>(state)) == "42000" && (native == 2715 || native == 243))
                GTEST_SKIP() << "The server has no json data type: " << message;
            FAIL() << "Could not determine server JSON support: " << native << " " << message;
        }
        Reset();
        static unsigned sequence = 0;
        suffix_ = std::to_string(std::time(nullptr)) + "_" +
                  std::to_string(reinterpret_cast<std::uintptr_t>(this)) + "_" + std::to_string(++sequence);
    }
    void TearDown() override {
        if (stmt_ != SQL_NULL_HSTMT) {
            SQLCancel(stmt_);
            SQLSetStmtAttr(stmt_, SQL_SOPT_SS_PARAM_FOCUS, nullptr, SQL_IS_INTEGER);
            Reset();
            for (auto it = objects_.rbegin(); it != objects_.rend(); ++it)
                EXPECT_SQL_OK(ExecText(stmt_, "DROP " + *it), SQL_HANDLE_STMT, stmt_);
        }
        ODBCTest::TearDown();
    }
    void Reset() {
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    }
    std::string CreateType(const std::string& columns) {
        const std::string name = "json_tvp_" + suffix_ + "_" + std::to_string(objects_.size());
        EXPECT_SQL_OK(ExecText(stmt_, "CREATE TYPE dbo.[" + name + "] AS TABLE (" + columns + ")"),
                      SQL_HANDLE_STMT, stmt_);
        objects_.push_back("TYPE dbo.[" + name + "]");
        Reset();
        return name;
    }
    SQLINTEGER ScalarInt(const std::string& sql) {
        Reset();
        SQLINTEGER value = -1;
        SQLLEN indicator = 0;
        EXPECT_SQL_OK(ExecText(stmt_, sql), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        Reset();
        return indicator == SQL_NULL_DATA ? -2 : value;
    }
    std::optional<std::u16string> ReadWideColumn(SQLUSMALLINT column) {
        std::u16string text;
        std::vector<SQLWCHAR> buffer(8192);
        for (;;) {
            SQLLEN indicator = 0;
            const SQLRETURN rc = SQLGetData(stmt_, column, SQL_C_WCHAR, buffer.data(),
                                            static_cast<SQLLEN>(buffer.size() * sizeof(SQLWCHAR)), &indicator);
            if (rc == SQL_NO_DATA) return text;
            EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
            if (!SQL_SUCCEEDED(rc)) return std::nullopt;
            if (indicator == SQL_NULL_DATA) return std::nullopt;
            const size_t count = rc == SQL_SUCCESS ? static_cast<size_t>(indicator) / sizeof(SQLWCHAR)
                                                   : buffer.size() - 1;
            for (size_t i = 0; i < count; ++i) text.push_back(static_cast<char16_t>(buffer[i]));
            if (rc == SQL_SUCCESS) return text;
        }
    }
    std::optional<std::u16string> ScalarText(const std::string& sql) {
        Reset();
        EXPECT_SQL_OK(ExecText(stmt_, sql), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        auto text = ReadWideColumn(1);
        Reset();
        return text;
    }
    SQLRETURN StreamCells(const std::vector<std::vector<Bytes>>& cells) {
        for (const auto& pieces : cells) {
            SQLPOINTER token = nullptr;
            const SQLRETURN rc = SQLParamData(stmt_, &token);
            if (rc != SQL_NEED_DATA) {
                ADD_FAILURE() << "Expected a DAE cell: " << rc << " "
                              << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
                return rc;
            }
            for (const auto& piece : pieces) {
                const SQLRETURN put = SQLPutData(stmt_, const_cast<uint8_t*>(piece.data()),
                                                static_cast<SQLLEN>(piece.size()));
                if (!SQL_SUCCEEDED(put)) return put;
            }
        }
        SQLPOINTER token = nullptr;
        const SQLRETURN rc = SQLParamData(stmt_, &token);
        if (!SQL_SUCCEEDED(rc)) return rc;
        SQLRETURN more;
        while (SQL_SUCCEEDED(more = SQLMoreResults(stmt_))) {}
        return more == SQL_NO_DATA ? rc : more;
    }
    std::string suffix_;
    std::vector<std::string> objects_;
};

// Benefits-from-mock-tds: Compare JSON TVP metadata/PLP bytes with NVARCHAR(MAX).
TEST_F(JsonTvpTest, JsonColumnsStoreWhatTheServerCastOfTheSameTextStores) {
    const auto type = CreateType("id int NOT NULL, j_w json NULL, j_b json NULL, n nvarchar(max) NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j_w json NULL, j_b json NULL, n nvarchar(max) NULL)"),
                  SQL_HANDLE_STMT, stmt_);
    const std::u16string docs[] = {
        u"{\"a\":1}", u"{ \"a\" : [ 1 , 2.50 , -0.0 , 1e2 ] ,\n\t\"b\" : { } }",
        kUnicodeJsonUtf16, u"{\"esc\":\"q\\\"b\\\\s\\/n\\n\\u0041\\u00e9\"}",
        u"[true,false,null,\"x\",[[[]]]]", u"{\"big\":12345678901234567890123,\"neg\":-1.5E-3}",
    };
    const size_t rows = std::size(docs) + 1;
    ColumnBuffer ids(rows, sizeof(SQLINTEGER)), wide(rows, 512), utf8(rows, 512), text(rows, 512);
    for (size_t row = 0; row < rows; ++row) {
        const SQLINTEGER id = static_cast<SQLINTEGER>(row + 1);
        ids.Set(row, &id, sizeof(id));
        if (row == std::size(docs)) {
            wide.SetNull(row); utf8.SetNull(row); text.SetNull(row);
        } else {
            wide.Set(row, docs[row]); utf8.Set(row, Utf8(docs[row])); text.Set(row, docs[row]);
        }
    }
    SQLLEN rowCount = 0;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, static_cast<SQLLEN>(rows), &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_WCHAR, SQL_SS_JSON, 0, wide), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 3, SQL_C_BINARY, SQL_SS_JSON, 0, utf8), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 4, SQL_C_WCHAR, SQL_WVARCHAR, 0, text), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_SQL_OK(ExecText(stmt_, "INSERT INTO #sink SELECT id, j_w, j_b, n FROM ?"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(static_cast<SQLINTEGER>(rows), ScalarInt("SELECT COUNT(*) FROM #sink"));
    EXPECT_EQ(0, ScalarInt(MismatchCount("j_w", "n")));
    EXPECT_EQ(0, ScalarInt(MismatchCount("j_b", "n")));
    for (size_t row = 0; row < std::size(docs); ++row)
        EXPECT_EQ(docs[row], ScalarText("SELECT n FROM #sink WHERE id = " + std::to_string(row + 1)));
}

// Benefits-from-mock-tds: Verify large cells retain NVARCHAR(MAX) PLP framing.
TEST_F(JsonTvpTest, MultiMegabyteDocumentsBoundAndStreamed) {
    const auto type = CreateType("id int NOT NULL, j json NULL, n nvarchar(max) NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL, n nvarchar(max) NULL)"),
                  SQL_HANDLE_STMT, stmt_);
    std::u16string doc = u"{\"k\":\"";
    while (doc.size() < 1500000) doc += u"abc\u00E9\u20AC\U0001F600 ";
    doc += u"\"}";
    const auto utf8 = Utf8(doc);
    const auto wide = WideBytes(doc);
    for (SQLINTEGER id : {1, 2}) {
        ColumnBuffer ids(1, sizeof(SQLINTEGER)), j(1, utf8.size()), n(1, wide.size());
        ids.Set(0, &id, sizeof(id));
        if (id == 1) { j.Set(0, utf8); n.Set(0, wide.data(), wide.size()); }
        else { j.indicators[0] = SQL_DATA_AT_EXEC; n.indicators[0] = SQL_DATA_AT_EXEC; }
        SQLLEN rowCount = 0;
        ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
            ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_BINARY, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindColumn(stmt_, 3, SQL_C_WCHAR, SQL_WVARCHAR, 0, n), SQL_HANDLE_STMT, stmt_);
        }));
        const SQLRETURN rc = ExecText(stmt_, "INSERT INTO #sink SELECT id, j, n FROM ?");
        if (id == 1) { ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_); }
        else {
            ASSERT_EQ(SQL_NEED_DATA, rc);
            ASSERT_SQL_OK(StreamCells({Pieces(Bytes(utf8.begin(), utf8.end()), 65537),
                                       Pieces(wide, 32769 * sizeof(SQLWCHAR))}), SQL_HANDLE_STMT, stmt_);
        }
        Reset();
    }
    EXPECT_EQ(2, ScalarInt("SELECT COUNT(*) FROM #sink"));
    EXPECT_EQ(0, ScalarInt(MismatchCount("j", "n")));
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #sink WHERE DATALENGTH(n) <> " + std::to_string(doc.size() * 2)));
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(DISTINCT HASHBYTES('SHA2_256', n)) FROM #sink"));
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(DISTINCT HASHBYTES('SHA2_256', CAST(j AS nvarchar(max)))) FROM #sink"));
    EXPECT_EQ(doc, ScalarText("SELECT n FROM #sink WHERE id = 2"));
}

// Benefits-from-mock-tds: Verify ColumnSize never selects finite JSON TVP metadata.
TEST_F(JsonTvpTest, ColumnSizeDoesNotLimitAJsonColumn) {
    const auto type = CreateType("id int NOT NULL, j json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL)"), SQL_HANDLE_STMT, stmt_);
    const std::string doc = "{\"k\":\"" + std::string(60, 'x') + "\"}";
    for (SQLINTEGER id : {1, 2}) {
        ColumnBuffer ids(1, sizeof(SQLINTEGER)), j(1, 256);
        ids.Set(0, &id, sizeof(id));
        j.Set(0, std::u16string(doc.begin(), doc.end()));
        if (id == 2) j.indicators[0] = SQL_DATA_AT_EXEC;
        SQLLEN rowCount = 0;
        ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
            ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindColumn(stmt_, 2, id == 1 ? SQL_C_WCHAR : SQL_C_CHAR, SQL_SS_JSON, 4, j),
                          SQL_HANDLE_STMT, stmt_);
        }));
        const SQLRETURN rc = ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?");
        if (id == 1) { ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_); }
        else {
            ASSERT_EQ(SQL_NEED_DATA, rc);
            ASSERT_SQL_OK(StreamCells({Pieces(Bytes(doc.begin(), doc.end()), 10)}), SQL_HANDLE_STMT, stmt_);
        }
        Reset();
    }
    const std::u16string expected(doc.begin(), doc.end());
    EXPECT_EQ(expected, ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink WHERE id = 1"));
    EXPECT_EQ(expected, ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink WHERE id = 2"));
}

// Benefits-from-mock-tds: Verify a rejected row does not invalidate the prepared TVP.
TEST_F(JsonTvpTest, PreparedStatementSurvivesServerRejectionOfOneRow) {
    const auto type = CreateType("id int NOT NULL, j json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL)"), SQL_HANDLE_STMT, stmt_);
    ColumnBuffer ids(2, sizeof(SQLINTEGER)), j(2, 256);
    SQLLEN rowCount = 0;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 2, &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_WCHAR, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
    }));
    SQLCHAR insert[] = "INSERT INTO #sink SELECT id, j FROM ?";
    ASSERT_SQL_OK(SQLPrepareA(stmt_, insert, SQL_NTS), SQL_HANDLE_STMT, stmt_);
    auto setRow = [&](size_t row, SQLINTEGER id, const std::u16string& doc) {
        ids.Set(row, &id, sizeof(id)); j.Set(row, doc);
    };
    setRow(0, 1, u"{\"a\":1}"); setRow(1, 2, u"{\"a\":2}");
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    setRow(0, 3, u"{\"a\":3}"); setRow(1, 4, u"{\"a\":");
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "42000");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    rowCount = 1;
    setRow(0, 5, kUnicodeJsonUtf16);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #sink"));
    EXPECT_EQ(8, ScalarInt("SELECT SUM(id) FROM #sink"));
    EXPECT_EQ(kUnicodeJsonUtf16, ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink WHERE id = 5"));
}

// Benefits-from-mock-tds: Verify joined DAE bytes are validated before sending rows.
TEST_F(JsonTvpTest, Utf8MadeInvalidOnlyAcrossPiecesIsRefused) {
    const auto type = CreateType("id int NOT NULL, j json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL)"), SQL_HANDLE_STMT, stmt_);
    const std::pair<std::string, std::string> invalid[] = {
        {"{\"k\":\"\xE2", "\x28\xA1\"}"}, {"{\"k\":\"\xE0", "\x80\x80\"}"},
        {"{\"k\":\"\xED", "\xA0\x80\"}"}, {"{\"k\":\"\xF4", "\x90\x80\x80\"}"},
    };
    ColumnBuffer ids(1, sizeof(SQLINTEGER)), j(1, 16);
    const SQLINTEGER one = 1;
    ids.Set(0, &one, sizeof(one));
    for (const auto& split : invalid) {
        j.indicators[0] = SQL_DATA_AT_EXEC;
        SQLLEN rowCount = 0;
        ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
            ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_BINARY, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
        }));
        ASSERT_EQ(SQL_NEED_DATA, ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"));
        EXPECT_EQ(SQL_ERROR, StreamCells({{Bytes(split.first.begin(), split.first.end()),
                                          Bytes(split.second.begin(), split.second.end())}}));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
        ASSERT_SQL_OK(SQLCancel(stmt_), SQL_HANDLE_STMT, stmt_);
        Reset();
    }
    const std::string first = "{\"k\":\"\xE2", second = "\x82\xAC\"}";
    j.indicators[0] = SQL_DATA_AT_EXEC;
    SQLLEN rowCount = 0;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_BINARY, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_EQ(SQL_NEED_DATA, ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"));
    ASSERT_SQL_OK(StreamCells({{Bytes(first.begin(), first.end()), Bytes(second.begin(), second.end())}}),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #sink"));
    EXPECT_EQ(std::u16string(u"{\"k\":\"\u20AC\"}"), ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink"));
}

// Benefits-from-mock-tds: Verify client-codepage DAE text is transcoded to UTF-16.
TEST_F(JsonTvpTest, ClientCodePageTextStreamedOneByteAtATime) {
    const auto type = CreateType("id int NOT NULL, j json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(ExecText(stmt_, "SELECT CONVERT(varchar(100), N'{\"k\":\"' + NCHAR(233) + NCHAR(8364) + N'\"}'), "
                                  "CONVERT(nvarchar(100), CONVERT(varchar(100), N'{\"k\":\"' + NCHAR(233) + "
                                  "NCHAR(8364) + N'\"}'))"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    char clientText[128]{};
    SQLLEN clientLength = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, clientText, sizeof(clientText), &clientLength),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_GT(clientLength, 0);
    const auto expected = ReadWideColumn(2);
    ASSERT_TRUE(expected.has_value());
    Reset();
    ColumnBuffer ids(1, sizeof(SQLINTEGER)), j(1, 16);
    const SQLINTEGER one = 1;
    ids.Set(0, &one, sizeof(one));
    j.indicators[0] = SQL_DATA_AT_EXEC;
    SQLLEN rowCount = 0;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_CHAR, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_EQ(SQL_NEED_DATA, ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"));
    ASSERT_SQL_OK(StreamCells({Pieces(Bytes(clientText, clientText + clientLength), 1)}), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(*expected, ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink"));
}

TEST_F(JsonTvpTest, DescribesTableTypeJsonColumn) {
    const auto type = CreateType("id int NOT NULL, payload json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "DECLARE @rows dbo.[" + type + "]; SELECT payload FROM @rows WHERE 1 = 0"),
                  SQL_HANDLE_STMT, stmt_);
    SQLSMALLINT columns = 0;
    ASSERT_SQL_OK(SQLNumResultCols(stmt_, &columns), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(1, columns);
    SQLCHAR name[32]{}, typeName[32]{};
    SQLSMALLINT nameLength = 0, dataType = 0, digits = -1, nullable = -1, typeNameLength = 0;
    SQLULEN size = 99;
    ASSERT_SQL_OK(SQLDescribeColA(stmt_, 1, name, sizeof(name), &nameLength, &dataType, &size, &digits, &nullable),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("payload", reinterpret_cast<char*>(name));
    EXPECT_EQ(SQL_SS_JSON, dataType);
    ASSERT_SQL_OK(SQLColAttributeA(stmt_, 1, SQL_DESC_TYPE_NAME, typeName, sizeof(typeName), &typeNameLength, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("json", reinterpret_cast<char*>(typeName));
}

// Benefits-from-mock-tds: Assert focused JSON descriptors survive text transport.
TEST_F(JsonTvpTest, JsonBindingPreservesUnicodeAndNull) {
    const auto type = CreateType("id int NOT NULL, payload json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, payload json NULL)"), SQL_HANDLE_STMT, stmt_);
    ColumnBuffer ids(1, sizeof(SQLINTEGER)), payload(1, 128);
    const SQLINTEGER id = 1;
    ids.Set(0, &id, sizeof(id));
    payload.Set(0, kUnicodeJsonUtf16);
    for (bool null : {false, true}) {
        if (null) payload.SetNull(0);
        SQLLEN rowCount = 0;
        ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
            ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_WCHAR, SQL_SS_JSON, 0, payload), SQL_HANDLE_STMT, stmt_);
        }));
        ASSERT_SQL_OK(ExecText(stmt_, "INSERT INTO #sink SELECT id, payload FROM ?"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_SOPT_SS_PARAM_FOCUS, reinterpret_cast<SQLPOINTER>(1), SQL_IS_INTEGER),
                      SQL_HANDLE_STMT, stmt_);
        SQLHDESC ipd = SQL_NULL_HDESC;
        ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_IMP_PARAM_DESC, &ipd, 0, nullptr), SQL_HANDLE_STMT, stmt_);
        SQLSMALLINT conciseType = 0;
        ASSERT_SQL_OK(SQLGetDescFieldW(ipd, 2, SQL_DESC_CONCISE_TYPE, &conciseType, 0, nullptr), SQL_HANDLE_DESC, ipd);
        EXPECT_EQ(SQL_SS_JSON, conciseType);
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_SOPT_SS_PARAM_FOCUS, nullptr, SQL_IS_INTEGER), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(null ? std::nullopt : std::optional<std::u16string>(kUnicodeJsonUtf16),
                  ScalarText("SELECT CAST(payload AS nvarchar(max)) FROM #sink"));
        ASSERT_SQL_OK(ExecText(stmt_, "DELETE FROM #sink"), SQL_HANDLE_STMT, stmt_);
        Reset();
    }
}

// Benefits-from-mock-tds: Verify each DAE row's pieces and NULL PLP sentinel.
TEST_F(JsonTvpTest, StreamedRowsAroundANullRowReachTheServer) {
    const auto type = CreateType("id int NOT NULL, j json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL)"), SQL_HANDLE_STMT, stmt_);
    const std::u16string texts[] = {kUnicodeJsonUtf16, u"{\"r\":3,\"e\":\"\u20AC\U0001F600\u00E9\"}"};
    for (SQLSMALLINT cType : {static_cast<SQLSMALLINT>(SQL_C_WCHAR), static_cast<SQLSMALLINT>(SQL_C_BINARY)}) {
        ColumnBuffer ids(3, sizeof(SQLINTEGER)), j(3, 16);
        for (SQLINTEGER id = 1; id <= 3; ++id) ids.Set(static_cast<size_t>(id - 1), &id, sizeof(id));
        j.indicators = {SQL_DATA_AT_EXEC, SQL_NULL_DATA, SQL_DATA_AT_EXEC};
        SQLLEN rowCount = 0;
        ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 3, &rowCount, [&] {
            ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindColumn(stmt_, 2, cType, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
        }));
        ASSERT_EQ(SQL_NEED_DATA, ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"));
        std::vector<std::vector<Bytes>> cells;
        for (const auto& text : texts) {
            if (cType == SQL_C_WCHAR) cells.push_back(Pieces(WideBytes(text), sizeof(SQLWCHAR)));
            else {
                const auto utf8 = Utf8(text);
                cells.push_back(Pieces(Bytes(utf8.begin(), utf8.end()), 1));
            }
        }
        ASSERT_SQL_OK(StreamCells(cells), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(texts[0], ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink WHERE id = 1"));
        EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #sink WHERE id = 2 AND j IS NULL"));
        EXPECT_EQ(texts[1], ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink WHERE id = 3"));
        ASSERT_SQL_OK(ExecText(stmt_, "DELETE FROM #sink"), SQL_HANDLE_STMT, stmt_);
        Reset();
    }
}

// Benefits-from-mock-tds: Verify row-wise pointer stride and JSON/text metadata.
TEST_F(JsonTvpTest, RowWiseBoundJsonCellsEncodeLikeAnNvarcharMaxColumn) {
    const auto type = CreateType("id int NOT NULL, j_w json NULL, j_b json NULL, n nvarchar(max) NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j_w json NULL, j_b json NULL, n nvarchar(max) NULL)"),
                  SQL_HANDLE_STMT, stmt_);
    struct Row {
        SQLINTEGER id;
        SQLLEN idLength;
        SQLWCHAR wide[128];
        SQLLEN wideLength;
        char utf8[256];
        SQLLEN utf8Length;
        SQLWCHAR text[128];
        SQLLEN textLength;
    } data[3]{};
    const std::u16string texts[] = {kUnicodeJsonUtf16, u"{\"second\":[2]}"};
    for (size_t row = 0; row < std::size(data); ++row) {
        data[row].id = static_cast<SQLINTEGER>(row + 1);
        if (row == std::size(texts)) {
            data[row].wideLength = data[row].utf8Length = data[row].textLength = SQL_NULL_DATA;
        } else {
            const auto wide = Wide(texts[row]);
            const auto utf8 = Utf8(texts[row]);
            std::copy(wide.begin(), wide.end(), data[row].wide);
            std::copy(wide.begin(), wide.end(), data[row].text);
            std::copy(utf8.begin(), utf8.end(), data[row].utf8);
            data[row].wideLength = data[row].textLength = static_cast<SQLLEN>(wide.size() * sizeof(SQLWCHAR));
            data[row].utf8Length = static_cast<SQLLEN>(utf8.size());
        }
    }
    SQLLEN rowCount = 0;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 3, &rowCount, [&] {
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAM_BIND_TYPE,
                                     reinterpret_cast<SQLPOINTER>(sizeof(Row)), 0), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 10, 0,
                                       &data[0].id, 0, &data[0].idLength), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_SS_JSON, 0, 0,
                                       data[0].wide, sizeof(data[0].wide), &data[0].wideLength), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 3, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_SS_JSON, 0, 0,
                                       data[0].utf8, sizeof(data[0].utf8), &data[0].utf8Length), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 4, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR, 0, 0,
                                       data[0].text, sizeof(data[0].text), &data[0].textLength), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_SQL_OK(ExecText(stmt_, "INSERT INTO #sink SELECT id, j_w, j_b, n FROM ?"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, ScalarInt("SELECT COUNT(*) FROM #sink"));
    EXPECT_EQ(0, ScalarInt(MismatchCount("j_w", "n")));
    EXPECT_EQ(0, ScalarInt(MismatchCount("j_b", "n")));
    for (size_t row = 0; row < std::size(texts); ++row)
        EXPECT_EQ(texts[row], ScalarText("SELECT n FROM #sink WHERE id = " + std::to_string(row + 1)));
}

// Benefits-from-mock-tds: Verify procedure-call TVP column order and mixed types.
TEST_F(JsonTvpTest, ProcedureReceivesJsonColumnsAroundOtherColumns) {
    const auto type = CreateType("j1 json NULL, id int NOT NULL, label nvarchar(20) NULL, j2 json NULL");
    const auto procedure = "json_tvp_proc_" + suffix_;
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE PROCEDURE dbo.[" + procedure + "] @rows dbo.[" + type +
                                  "] READONLY AS SELECT id, JSON_VALUE(j1, '$.k'), label, JSON_VALUE(j2, '$.k'), "
                                  "CAST(j1 AS nvarchar(max)), CAST(j2 AS nvarchar(max)) FROM @rows ORDER BY id"),
                  SQL_HANDLE_STMT, stmt_);
    objects_.push_back("PROCEDURE dbo.[" + procedure + "]");
    Reset();
    const std::u16string second = u"{\"k\":\"second\"}", third = u"{\"k\":42}";
    ColumnBuffer j1(2, 64), ids(2, sizeof(SQLINTEGER)), labels(2, 64), j2(2, 64);
    j1.Set(0, Utf8(kUnicodeJsonUtf16)); j1.SetNull(1);
    for (SQLINTEGER id = 1; id <= 2; ++id) ids.Set(static_cast<size_t>(id - 1), &id, sizeof(id));
    labels.Set(0, std::u16string(u"first")); labels.SetNull(1);
    j2.Set(0, second); j2.Set(1, third);
    SQLLEN rowCount = 0;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 2, &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_BINARY, SQL_SS_JSON, 0, j1), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 3, SQL_C_WCHAR, SQL_WVARCHAR, 20, labels), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 4, SQL_C_WCHAR, SQL_SS_JSON, 0, j2), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_SQL_OK(ExecText(stmt_, "{call dbo.[" + procedure + "](?)}"), SQL_HANDLE_STMT, stmt_);
    using Text = std::optional<std::u16string>;
    const std::vector<std::vector<Text>> expected = {
        {u"1", u"\u00E9\u20AC\U0001F600", u"first", u"second", kUnicodeJsonUtf16, second},
        {u"2", std::nullopt, std::nullopt, u"42", std::nullopt, third},
    };
    for (const auto& row : expected) {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        for (size_t column = 0; column < row.size(); ++column)
            EXPECT_EQ(row[column], ReadWideColumn(static_cast<SQLUSMALLINT>(column + 1))) << "column " << column + 1;
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
}

// Benefits-from-mock-tds: Verify invalid bound UTF-8 never enters the TVP RPC.
TEST_F(JsonTvpTest, JsonBindingRejectsInvalidUtf8Binary) {
    const auto type = CreateType("id int NOT NULL, j json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL)"), SQL_HANDLE_STMT, stmt_);
    const std::string invalid[] = {
        "{\"k\":\"\xff\"}", "{\"k\":\"\xc0\x80\"}", "{\"k\":\"\xed\xa0\x80\"}",
        "{\"k\":\"\xf4\x90\x80\x80\"}", "{\"k\":\"\xe2",
    };
    ColumnBuffer ids(1, sizeof(SQLINTEGER)), j(1, 64);
    const SQLINTEGER one = 1;
    ids.Set(0, &one, sizeof(one));
    for (const auto& value : invalid) {
        j.Set(0, value);
        SQLLEN rowCount = 0;
        ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
            ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_BINARY, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
        }));
        EXPECT_EQ(SQL_ERROR, ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
        Reset();
    }
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #sink"));
}

// Benefits-from-mock-tds: Verify SQL_C_DEFAULT resolves to UTF-16 and DAE NULL framing.
TEST_F(JsonTvpTest, JsonBindingDefaultCTypeAndStreamedNull) {
    const auto type = CreateType("id int NOT NULL, j json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL)"), SQL_HANDLE_STMT, stmt_);
    ColumnBuffer ids(1, sizeof(SQLINTEGER)), j(1, 128);
    const SQLINTEGER one = 1;
    ids.Set(0, &one, sizeof(one));
    j.Set(0, kUnicodeJsonUtf16);
    SQLLEN rowCount = 0;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_DEFAULT, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_SQL_OK(ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(kUnicodeJsonUtf16, ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink"));
    ASSERT_SQL_OK(ExecText(stmt_, "DELETE FROM #sink"), SQL_HANDLE_STMT, stmt_);
    Reset();
    j.indicators[0] = SQL_DATA_AT_EXEC;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_DEFAULT, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_EQ(SQL_NEED_DATA, ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"));
    SQLPOINTER token = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &token));
    ASSERT_SQL_OK(SQLPutData(stmt_, nullptr, SQL_NULL_DATA), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLParamData(stmt_, &token), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(std::nullopt, ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink"));
}

// Benefits-from-mock-tds: Verify cancelling a partial TVP does not send its rows.
TEST_F(JsonTvpTest, CancelledJsonCellAllowsStatementReuse) {
    const auto type = CreateType("id int NOT NULL, j json NULL");
    ASSERT_SQL_OK(ExecText(stmt_, "CREATE TABLE #sink (id int NOT NULL, j json NULL)"), SQL_HANDLE_STMT, stmt_);
    ColumnBuffer ids(1, sizeof(SQLINTEGER)), j(1, 128);
    const SQLINTEGER one = 1;
    ids.Set(0, &one, sizeof(one));
    j.indicators[0] = SQL_DATA_AT_EXEC;
    SQLLEN rowCount = 0;
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_BINARY, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_EQ(SQL_NEED_DATA, ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"));
    SQLPOINTER token = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &token));
    char partial[] = "{\"k\":\"\xf0\x9f";
    ASSERT_SQL_OK(SQLPutData(stmt_, partial, sizeof(partial) - 1), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLCancel(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, ScalarInt("SELECT COUNT(*) FROM #sink"));
    j.Set(0, Utf8(kUnicodeJsonUtf16));
    ASSERT_NO_FATAL_FAILURE(BindTvp(stmt_, type, 1, &rowCount, [&] {
        ASSERT_SQL_OK(BindColumn(stmt_, 1, SQL_C_SLONG, SQL_INTEGER, 10, ids), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindColumn(stmt_, 2, SQL_C_BINARY, SQL_SS_JSON, 0, j), SQL_HANDLE_STMT, stmt_);
    }));
    ASSERT_SQL_OK(ExecText(stmt_, "INSERT INTO #sink SELECT id, j FROM ?"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, ScalarInt("SELECT COUNT(*) FROM #sink"));
    EXPECT_EQ(kUnicodeJsonUtf16, ScalarText("SELECT CAST(j AS nvarchar(max)) FROM #sink"));
}
