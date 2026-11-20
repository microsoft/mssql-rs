// Copyright (c) Microsoft Corporation. All rights reserved.

#include "odbc_concurrent_test.h"
#include "odbc_sql_server_types.h"
#include "odbc_test_fixture.h"

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <iterator>
#include <random>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

namespace {

struct ServerText {
    unsigned code_page;
    const char* collation;
    const char* expression;
    const char* utf8;
    std::vector<SQLWCHAR> utf16;
};

const ServerText kServerText[] = {
    {1252, "Latin1_General_100_CI_AS", "NCHAR(233)", "\xC3\xA9", {0x00E9}},
    {65001, "Latin1_General_100_CI_AS_SC_UTF8",
     "NCHAR(55357) + NCHAR(56832)", "\xF0\x9F\x98\x80", {0xD83D, 0xDE00}},
    {932, "Japanese_XJIS_100_CI_AS", "NCHAR(12354)", "\xE3\x81\x82", {0x3042}},
    {1251, "Cyrillic_General_100_CI_AS", "NCHAR(1046)", "\xD0\x96", {0x0416}},
};

// Each value's server bytes decode to different text under Windows-1252, and
// overlapping pages also disagree (0x9D is CP437 yen but CP850 O-slash; Big5 A4A4
// is a different GBK/UHC character), so a wrong decoder changes the fetched text.
const ServerText kServerCollations[] = {
    {0, "Assamese_100_BIN", nullptr, nullptr, {}},
    {437, "SQL_Latin1_General_CP437_BIN", "NCHAR(165)", "\xC2\xA5", {0x00A5}},
    {850, "SQL_1xCompat_CP850_CI_AS", "NCHAR(216)", "\xC3\x98", {0x00D8}},
    {874, "Thai_100_BIN", "NCHAR(3585)", "\xE0\xB8\x81", {0x0E01}},
    {932, "Japanese_90_BIN", "NCHAR(12354)", "\xE3\x81\x82", {0x3042}},
    {936, "Chinese_PRC_90_BIN", "NCHAR(20320)", "\xE4\xBD\xA0", {0x4F60}},
    {949, "Korean_100_BIN", "NCHAR(54620)", "\xED\x95\x9C", {0xD55C}},
    {950, "Chinese_Hong_Kong_Stroke_90_BIN", "NCHAR(20013)", "\xE4\xB8\xAD", {0x4E2D}},
    {1250, "Albanian_100_CI_AS", "NCHAR(321)", "\xC5\x81", {0x0141}},
    {1251, "Cyrillic_General_100_CI_AS", "NCHAR(1071)", "\xD0\xAF", {0x042F}},
    {1252, "Latin1_General_100_CI_AS", "NCHAR(8364)", "\xE2\x82\xAC", {0x20AC}},
    {1253, "Greek_100_CI_AS", "NCHAR(937)", "\xCE\xA9", {0x03A9}},
    {1254, "Turkish_100_CI_AS", "NCHAR(351)", "\xC5\x9F", {0x015F}},
    {1255, "Hebrew_100_CI_AS", "NCHAR(1488)", "\xD7\x90", {0x05D0}},
    {1256, "Arabic_100_CI_AS", "NCHAR(1576)", "\xD8\xA8", {0x0628}},
    {1257, "Estonian_100_CI_AS", "NCHAR(257)", "\xC4\x81", {0x0101}},
    {1258, "Vietnamese_100_CI_AS", "NCHAR(272)", "\xC4\x90", {0x0110}},
    {65001, "Latin1_General_100_CI_AS_SC_UTF8", "NCHAR(8364)", "\xE2\x82\xAC", {0x20AC}},
};

std::string VarcharQuery(const ServerText& text, const char* type = "varchar(max)") {
    return "SELECT CAST((" + std::string(text.expression) + ") COLLATE " + text.collation +
           " AS " + type + ")";
}

std::string MissingConverterMessage(unsigned code_page);

bool ComparingMsodbcsql() {
    const char* target = std::getenv("ODBC_TEST_TARGET");
    return target != nullptr && std::string(target) == "msodbcsql";
}

std::string Utf16ToUtf8(const std::u16string& units) {
    std::string out;
    for (size_t i = 0; i < units.size(); ++i) {
        char32_t cp = units[i];
        if (cp >= 0xD800 && cp <= 0xDBFF && i + 1 < units.size() && units[i + 1] >= 0xDC00 &&
            units[i + 1] <= 0xDFFF) {
            cp = 0x10000 + ((cp - 0xD800) << 10) + (units[i + 1] - 0xDC00);
            ++i;
        }
        if (cp < 0x80) {
            out.push_back(static_cast<char>(cp));
        } else if (cp < 0x800) {
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

std::string Hex(const std::string& bytes) {
    static const char kDigits[] = "0123456789ABCDEF";
    std::string out;
    for (const unsigned char byte : bytes) {
        if (!out.empty()) {
            out.push_back(' ');
        }
        out.push_back(kDigits[byte >> 4]);
        out.push_back(kDigits[byte & 0xF]);
    }
    return out;
}

constexpr SQLLEN kTextBytes = 1024;

// Returns the problem with a complete text result, or an empty string after
// storing it as UTF-8 (SQL_C_WCHAR) or client bytes (SQL_C_CHAR) in |value|.
std::string DecodeText(const SQLWCHAR* buffer, SQLLEN indicator, SQLSMALLINT c_type,
                       std::string& value) {
    const SQLLEN unit = c_type == SQL_C_CHAR ? 1 : static_cast<SQLLEN>(sizeof(SQLWCHAR));
    if (indicator < 0 || indicator % unit != 0 || indicator + unit > kTextBytes) {
        return "indicator " + std::to_string(indicator);
    }
    if (c_type == SQL_C_CHAR) {
        const char* bytes = reinterpret_cast<const char*>(buffer);
        if (bytes[indicator] != '\0') {
            return "missing terminator";
        }
        value.assign(bytes, static_cast<size_t>(indicator));
        return "";
    }
    const size_t units = static_cast<size_t>(indicator) / sizeof(SQLWCHAR);
    if (buffer[units] != 0) {
        return "missing terminator";
    }
    value = Utf16ToUtf8(std::u16string(buffer, buffer + units));
    return "";
}

SQLRETURN ExecOn(SQLHSTMT stmt, const std::string& sql) {
    SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
    return SQLExecDirect(stmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
}

// Only a successfully created, randomly named database is ever dropped.
struct PrivateDatabase {
    SQLHSTMT stmt;
    std::string name;
    bool created = false;

    explicit PrivateDatabase(SQLHSTMT statement) : stmt(statement) {
        std::random_device device;
        std::mt19937_64 generator(device());
        char suffix[17] = {};
        std::snprintf(suffix, sizeof(suffix), "%016llx",
                      static_cast<unsigned long long>(generator()));
        name = "odbc_db_collation_" + std::string(suffix);
    }

    ~PrivateDatabase() {
        if (!created) {
            return;
        }
        SQLFreeStmt(stmt, SQL_CLOSE);
        SQLFreeStmt(stmt, SQL_UNBIND);
        SQLFreeStmt(stmt, SQL_RESET_PARAMS);
        EXPECT_SQL_OK(ExecOn(stmt, "USE [master]"), SQL_HANDLE_STMT, stmt);
        SQLFreeStmt(stmt, SQL_CLOSE);
        EXPECT_SQL_OK(ExecOn(stmt, "DROP DATABASE [" + name + "]"), SQL_HANDLE_STMT, stmt);
        SQLFreeStmt(stmt, SQL_CLOSE);
    }
};

class ClientCodePageTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }

        const char* value = std::getenv("MSSQL_ODBC_TEST_CLIENT_CODE_PAGE");
        ASSERT_NE(nullptr, value) << "CTest must set the isolated test code page";
        char* end = nullptr;
        const unsigned long parsed = std::strtoul(value, &end, 10);
        ASSERT_NE(value, end);
        ASSERT_EQ('\0', *end);
        ASSERT_TRUE(std::any_of(std::begin(kServerCollations), std::end(kServerCollations),
                                [parsed](const ServerText& text) {
                                    return text.code_page == parsed && text.code_page != 0;
                                }));
        client_code_page_ = static_cast<unsigned>(parsed);

        // msodbcsql has no override; it is a valid reference only when the
        // requested code page is already the process ANSI code page.
        const char* target = std::getenv("ODBC_TEST_TARGET");
        if (target != nullptr && std::string(target) == "msodbcsql") {
#ifdef _WIN32
            if (client_code_page_ != GetACP()) {
                GTEST_SKIP() << "msodbcsql uses ANSI code page " << GetACP();
            }
#else
            GTEST_SKIP() << "msodbcsql uses the process locale, not the test override";
#endif
        }

        if (!ODBCTestUtils::HasCodePageConverter(client_code_page_)) {
            GTEST_SKIP() << MissingConverterMessage(client_code_page_);
        }

        Connect();
        RecordProperty("client_code_page", static_cast<int>(client_code_page_));
    }

    void CheckNarrow(const std::string& sql, const std::string& expected) {
        ASSERT_NO_FATAL_FAILURE(ExecDirect(sql));
        ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
        std::vector<SQLCHAR> output(expected.size() + 1, 0xCC);
        SQLLEN indicator = -1;
        const SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, output.data(),
                                        static_cast<SQLLEN>(output.size()), &indicator);
        ASSERT_EQ(SQL_SUCCESS, rc) << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(static_cast<SQLLEN>(expected.size()), indicator);
        EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(output.data()),
                                        expected.size()));
        EXPECT_EQ(0, output.back());
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }

    void CheckServerText(const ServerText& text) {
        SCOPED_TRACE(text.collation);
        const std::string property =
            "SELECT CONVERT(int, COLLATIONPROPERTY('" + std::string(text.collation) +
            "', 'CodePage'))";
        ASSERT_EQ(SQL_SUCCESS, Execute(property));
        ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
        SQLINTEGER server_code_page = -1;
        ASSERT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_SLONG, &server_code_page,
                                          sizeof(server_code_page), nullptr));
        EXPECT_EQ(static_cast<SQLINTEGER>(text.code_page), server_code_page);
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        if (text.code_page == 0) {
            return;
        }

        const std::string expected =
            ODBCTestUtils::Utf8ToCodePage(text.utf8, client_code_page_);
        // Bounded and PLP values are decoded on separate driver paths.
        for (const char* type : {"varchar(20)", "varchar(max)"}) {
            SCOPED_TRACE(type);
            const std::string query = VarcharQuery(text, type);
            ASSERT_NO_FATAL_FAILURE(CheckNarrow(query, expected));

            ASSERT_EQ(SQL_SUCCESS, Execute(query));
            ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
            std::vector<SQLWCHAR> output(text.utf16.size() + 1, 0xCCCC);
            SQLLEN indicator = -1;
            ASSERT_EQ(SQL_SUCCESS,
                      SQLGetData(stmt_, 1, SQL_C_WCHAR, output.data(),
                                 static_cast<SQLLEN>(output.size() * sizeof(SQLWCHAR)),
                                 &indicator))
                << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(static_cast<SQLLEN>(text.utf16.size() * sizeof(SQLWCHAR)), indicator);
            EXPECT_TRUE(std::equal(text.utf16.begin(), text.utf16.end(), output.begin()));
            EXPECT_EQ(0, output.back());
            ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        }
    }

    unsigned client_code_page_ = 0;

    SQLRETURN Execute(const std::string& sql) { return ExecOn(stmt_, sql); }

    // Reads the first |count| columns of every row as |c_type| through
    // SQLGetData or bound buffers. Each call must return SQL_SUCCESS; returns
    // the first problem, or an empty string.
    std::string ReadRows(const std::string& sql, SQLUSMALLINT count, SQLSMALLINT c_type,
                         bool bound, std::vector<std::vector<std::string>>& rows) {
        rows.clear();
        std::vector<std::vector<SQLWCHAR>> buffers(
            bound ? count : 1, std::vector<SQLWCHAR>(kTextBytes / sizeof(SQLWCHAR)));
        std::vector<SQLLEN> indicators(buffers.size(), 0);
        auto describe = [this](const char* call, SQLRETURN rc) {
            return std::string(call) + " returned " + std::to_string(rc) + ": " +
                   ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        };
        auto read = [&]() -> std::string {
            SQLRETURN rc = Execute(sql);
            if (rc != SQL_SUCCESS) {
                return describe("SQLExecDirect", rc);
            }
            for (SQLUSMALLINT column = 1; bound && column <= count; ++column) {
                rc = SQLBindCol(stmt_, column, c_type, buffers[column - 1].data(), kTextBytes,
                                &indicators[column - 1]);
                if (rc != SQL_SUCCESS) {
                    return describe("SQLBindCol", rc);
                }
            }
            while (true) {
                for (auto& buffer : buffers) {
                    std::fill(buffer.begin(), buffer.end(), static_cast<SQLWCHAR>(0xCCCC));
                }
                rc = SQLFetch(stmt_);
                if (rc == SQL_NO_DATA) {
                    return "";
                }
                if (rc != SQL_SUCCESS) {
                    return describe("SQLFetch", rc);
                }
                std::vector<std::string> row(count);
                for (SQLUSMALLINT column = 1; column <= count; ++column) {
                    const size_t slot = bound ? column - 1 : 0;
                    if (!bound) {
                        std::fill(buffers[0].begin(), buffers[0].end(),
                                  static_cast<SQLWCHAR>(0xCCCC));
                        rc = SQLGetData(stmt_, column, c_type, buffers[0].data(), kTextBytes,
                                        &indicators[0]);
                        if (rc != SQL_SUCCESS) {
                            return describe("SQLGetData", rc);
                        }
                    }
                    const std::string problem =
                        DecodeText(buffers[slot].data(), indicators[slot], c_type, row[column - 1]);
                    if (!problem.empty()) {
                        return "column " + std::to_string(column) + ": " + problem;
                    }
                }
                rows.push_back(std::move(row));
            }
        };
        const std::string problem = read();
        SQLFreeStmt(stmt_, SQL_CLOSE);
        if (bound) {
            SQLFreeStmt(stmt_, SQL_UNBIND);
        }
        return problem;
    }

    // Expected SQL_C_CHAR bytes for UTF-8 text, or the reason there are none.
    std::string NarrowFor(const std::string& utf8, std::string& problem) const {
        try {
            return ODBCTestUtils::Utf8ToCodePage(utf8, client_code_page_);
        } catch (const std::exception& error) {
            problem = error.what();
            return "";
        }
    }
};

const ServerText& TextForCodePage(unsigned code_page) {
    const auto find = [code_page](const ServerText* begin, const ServerText* end) {
        return std::find_if(begin, end, [code_page](const ServerText& text) {
            return text.code_page == code_page;
        });
    };
    const auto text = find(std::begin(kServerText), std::end(kServerText));
    if (text != std::end(kServerText)) {
        return *text;
    }
    const auto collation =
        find(std::begin(kServerCollations), std::end(kServerCollations));
    if (collation == std::end(kServerCollations)) {
        throw std::invalid_argument("unsupported test code page");
    }
    return *collation;
}

std::string MissingConverterMessage(unsigned code_page) {
    return "host has no converter for code page " + std::to_string(code_page) +
           " (musl before 1.2.5 has no cp932; RHEL needs glibc-gconv-extra)";
}

struct Route {
    bool bound;
    SQLSMALLINT c_type;
    const char* name;
};

const Route kRoutes[] = {
    {false, SQL_C_WCHAR, "SQLGetData SQL_C_WCHAR"},
    {false, SQL_C_CHAR, "SQLGetData SQL_C_CHAR"},
    {true, SQL_C_WCHAR, "SQLBindCol SQL_C_WCHAR"},
    {true, SQL_C_CHAR, "SQLBindCol SQL_C_CHAR"},
};

// One character from each script the sweep's code pages cover, plus a
// supplementary-plane pair that only the UTF-8 collations can hold.
const char* const kSweepText =
    "NCHAR(65)+NCHAR(8364)+NCHAR(208)+NCHAR(321)+NCHAR(1071)+NCHAR(937)+NCHAR(351)+"
    "NCHAR(1488)+NCHAR(1576)+NCHAR(257)+NCHAR(432)+NCHAR(20320)+NCHAR(20013)+"
    "NCHAR(12354)+NCHAR(54620)+NCHAR(3585)+NCHAR(165)+NCHAR(216)+NCHAR(55357)+"
    "NCHAR(56832)+NCHAR(66)";

// Bytes no conforming encoder produces, with what each driver decodes them to
// (docs/parity-deviations.md, entry 45). Bounded Japanese columns drop a
// trailing partial lead byte on insert, so those cases use varchar(max) only.
struct MalformedValue {
    const char* collation;
    unsigned code_page;
    const char* type;
    const char* hex;
    std::u16string mssql_odbc;
    std::u16string msodbcsql;
};

const MalformedValue kMalformedValues[] = {
    {"Japanese_90_BIN", 932, "varchar(10)", "0x41FF42", u"A\uFFFDB", u"A\uF8F3B"},
    {"Japanese_90_BIN", 932, "varchar(max)", "0x41FF42", u"A\uFFFDB", u"A\uF8F3B"},
    {"Japanese_90_BIN", 932, "varchar(10)", "0x8241", u"\uFFFDA", u"\u30FB"},
    {"Japanese_90_BIN", 932, "varchar(max)", "0x8241", u"\uFFFDA", u"\u30FB"},
    {"Japanese_90_BIN", 932, "varchar(10)", "0xFF", u"\uFFFD", u"\uF8F3"},
    {"Japanese_90_BIN", 932, "varchar(max)", "0xFF", u"\uFFFD", u"\uF8F3"},
    {"Japanese_90_BIN", 932, "varchar(10)", "0x80", u"\x0080", u"\x0080"},
    {"Japanese_90_BIN", 932, "varchar(max)", "0x80", u"\x0080", u"\x0080"},
    {"Latin1_General_100_CI_AS_SC_UTF8", 65001, "varchar(10)", "0xC328", u"\uFFFD(",
     u"\uFFFD("},
    {"Latin1_General_100_CI_AS_SC_UTF8", 65001, "varchar(max)", "0xC328", u"\uFFFD(",
     u"\uFFFD("},
    {"Latin1_General_100_CI_AS_SC_UTF8", 65001, "varchar(10)", "0x41FF42", u"A\uFFFDB",
     u"A\uFFFDB"},
    {"Latin1_General_100_CI_AS_SC_UTF8", 65001, "varchar(max)", "0x41FF42", u"A\uFFFDB",
     u"A\uFFFDB"},
    {"Latin1_General_100_CI_AS_SC_UTF8", 65001, "varchar(10)", "0xEDA080",
     u"\uFFFD\uFFFD\uFFFD", u"\uFFFD\uFFFD"},
    {"Latin1_General_100_CI_AS_SC_UTF8", 65001, "varchar(max)", "0xEDA080",
     u"\uFFFD\uFFFD\uFFFD", u"\uFFFD\uFFFD"},
};

}  // namespace

// Benefits-from-mock-tds: a scripted TDS response could assert the exact
// collation metadata independently of SQL Server's COLLATE conversion.
ODBC_TEST_F(ClientCodePageTest, NativeNarrowFetchTranscodesAcrossCollations) {
    if (!ODBCTestUtils::HasCodePageConverter(client_code_page_)) {
        GTEST_SKIP() << MissingConverterMessage(client_code_page_);
    }
    for (const auto& text : kServerCollations) {
        ASSERT_NO_FATAL_FAILURE(CheckServerText(text));
    }
    for (const auto& text : kServerText) {
        ASSERT_NO_FATAL_FAILURE(CheckServerText(text));
    }
}

// Benefits-from-mock-tds: synthetic UTF-8/DBCS PLP payloads could place encoded
// characters at exact TDS chunk boundaries without relying on SQL Server casts.
ODBC_TEST_F(ClientCodePageTest, NarrowChunksFillBufferAndReassemble) {
    const ServerText& text = TextForCodePage(client_code_page_);
    const std::string sql =
        "SELECT CAST((NCHAR(65) + " + std::string(text.expression) + " + NCHAR(66)) COLLATE " +
        text.collation + " AS varchar(max))";
    const std::string encoded_character =
        ODBCTestUtils::Utf8ToCodePage(text.utf8, client_code_page_);
    const std::string encoded = "A" + encoded_character + "B";
    // AB#47564: SQL_C_CHAR chunks fill the byte capacity even mid-character, so
    // the multibyte character splits across the first two chunks.
    const bool multibyte_split = encoded_character.size() > 1;
    const std::vector<size_t> chunk_sizes = multibyte_split
                                                ? std::vector<size_t>{encoded_character.size(), 2}
                                                : std::vector<size_t>{1, 1, 1};
    const SQLLEN buffer_length = static_cast<SQLLEN>(encoded_character.size() + 1);
    // msodbcsql's chunk sizes for a split multibyte character are unmeasured; its
    // leg runs only when the Windows ACP matches, so that leg asserts reassembly only.
    const char* target = std::getenv("ODBC_TEST_TARGET");
    const bool exact_chunks =
        !multibyte_split || target == nullptr || std::string(target) != "msodbcsql";
    ASSERT_EQ(SQL_SUCCESS, Execute(sql));
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    size_t consumed = 0;
    size_t chunk_index = 0;
    while (consumed < encoded.size()) {
        SQLCHAR output[5];
        std::fill(std::begin(output), std::end(output), 0xCC);
        SQLLEN indicator = -1;
        const SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, output, buffer_length, &indicator);
        const size_t written = static_cast<size_t>(
            std::find(std::begin(output), std::begin(output) + buffer_length, 0) -
            std::begin(output));
        ASSERT_GT(written, 0u) << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        ASSERT_LE(written, static_cast<size_t>(buffer_length - 1));
        if (exact_chunks) {
            ASSERT_LT(chunk_index, chunk_sizes.size());
            ASSERT_EQ(chunk_sizes[chunk_index], written);
        }
        EXPECT_EQ(encoded.substr(consumed, written),
                  std::string(reinterpret_cast<const char*>(output), written));
        const size_t remaining = encoded.size() - consumed;
        if (remaining > static_cast<size_t>(buffer_length - 1)) {
            EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc)
                << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
        } else {
            EXPECT_EQ(SQL_SUCCESS, rc) << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        }
        if (indicator != SQL_NO_TOTAL) {
            EXPECT_EQ(static_cast<SQLLEN>(remaining), indicator);
        }
        consumed += written;
        ++chunk_index;
    }
    if (exact_chunks) {
        EXPECT_EQ(chunk_sizes.size(), chunk_index);
    }
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Benefits-from-mock-tds: a mock PLP response can split a UTF-16 surrogate pair
// at the wire chunk edge and prove the carry logic without a SQL Server cast.
ODBC_TEST_F(ClientCodePageTest, WideSurrogatePairSurvivesPlpAndBufferBoundaries) {
    static_assert(sizeof(SQLWCHAR) == 2);
    ASSERT_EQ(SQL_SUCCESS,
              Execute("SELECT (REPLICATE(CAST(N'A' AS nvarchar(max)), 4095) + "
                      "NCHAR(55357) + NCHAR(56832) + N'B') "
                      "COLLATE Latin1_General_100_CI_AS_SC"));
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    SQLWCHAR output[4096] = {};
    SQLLEN indicator = -1;
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO,
              SQLGetData(stmt_, 1, SQL_C_WCHAR, output, sizeof(output), &indicator))
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(static_cast<SQLLEN>(4098 * sizeof(SQLWCHAR)), indicator);
    EXPECT_TRUE(std::all_of(std::begin(output), std::end(output) - 1,
                            [](SQLWCHAR unit) { return unit == 'A'; }));
    EXPECT_EQ(0, output[4095]);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    std::fill(std::begin(output), std::end(output), 0);
    ASSERT_EQ(SQL_SUCCESS,
              SQLGetData(stmt_, 1, SQL_C_WCHAR, output, sizeof(output), &indicator))
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(static_cast<SQLLEN>(3 * sizeof(SQLWCHAR)), indicator);
    EXPECT_EQ(0xD83D, output[0]);
    EXPECT_EQ(0xDE00, output[1]);
    EXPECT_EQ('B', output[2]);
    EXPECT_EQ(0, output[3]);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Benefits-from-mock-tds: exact PLP lengths and empty-vs-NULL payloads are
// easier to generate and assert without SQL Server type conversion behavior.
ODBC_TEST_F(ClientCodePageTest, PlpLengthBoundariesEmptyNullAndBoundColumn) {
    ASSERT_EQ(
        SQL_SUCCESS,
        Execute("SELECT "
                "CAST(REPLICATE(CAST('x' AS varchar(max)), 8191) AS varchar(max)), "
                "CAST(REPLICATE(CAST('x' AS varchar(max)), 8192) AS varchar(max)), "
                "CAST(REPLICATE(CAST('x' AS varchar(max)), 8193) AS varchar(max)), "
                "CAST('' AS varchar(max)), CAST(NULL AS varchar(max))"));
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    for (SQLUSMALLINT column = 1; column <= 3; ++column) {
        const size_t expected_length = 8190 + column;
        size_t consumed = 0;
        std::string value;
        while (consumed < expected_length) {
            SQLCHAR output[1024];
            std::fill(std::begin(output), std::end(output), 0xCC);
            SQLLEN indicator = -1;
            const SQLRETURN rc =
                SQLGetData(stmt_, column, SQL_C_CHAR, output, sizeof(output), &indicator);
            const size_t written = static_cast<size_t>(
                std::find(std::begin(output), std::end(output), 0) - std::begin(output));
            ASSERT_GT(written, 0U);
            if (indicator != SQL_NO_TOTAL) {
                EXPECT_EQ(static_cast<SQLLEN>(expected_length - consumed), indicator);
            }
            if (expected_length - consumed > sizeof(output) - 1) {
                EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
                EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
            } else {
                EXPECT_EQ(SQL_SUCCESS, rc);
            }
            value.append(reinterpret_cast<const char*>(output), written);
            consumed += written;
        }
        EXPECT_EQ(std::string(expected_length, 'x'), value);
    }

    SQLCHAR output[4] = {0xCC, 0xCC, 0xCC, 0xCC};
    SQLLEN indicator = -1;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 4, SQL_C_CHAR, output, sizeof(output), &indicator));
    EXPECT_EQ(0, indicator);
    EXPECT_EQ(0, output[0]);

    std::fill(std::begin(output), std::end(output), 0xCC);
    indicator = -1;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 5, SQL_C_CHAR, output, sizeof(output), &indicator));
    EXPECT_EQ(SQL_NULL_DATA, indicator);
    EXPECT_EQ(0xCC, output[0]);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_SUCCESS,
              Execute("SELECT CAST(REPLICATE(CAST('x' AS varchar(max)), 8193) AS varchar(max))"))
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    SQLCHAR bound[8];
    std::fill(std::begin(bound), std::end(bound), 0xCC);
    indicator = -1;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, bound, sizeof(bound), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(8193, indicator);
    EXPECT_EQ("xxxxxxx", std::string(reinterpret_cast<const char*>(bound), 7));
    EXPECT_EQ(0, bound[7]);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Benefits-from-mock-tds: a mock can isolate length-probe handling from
// Driver Manager and SQL Server handling.
ODBC_TEST_F(ClientCodePageTest, TerminatorOnlyBuffersAreLengthProbes) {
    ASSERT_EQ(SQL_SUCCESS, Execute("SELECT CAST(N'A' AS nvarchar(max))"))
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    SQLWCHAR output[2] = {0xCCCC, 0xCCCC};
    SQLLEN indicator = -1;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO,
              SQLGetData(stmt_, 1, SQL_C_WCHAR, output, sizeof(SQLWCHAR), &indicator));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(2, indicator);
    EXPECT_EQ(0, output[0]);
    EXPECT_EQ(0xCCCC, output[1]);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_SUCCESS, Execute("SELECT CAST('probe' AS varchar(max))"))
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLCHAR probe = 'Q';
    indicator = -1;
    const SQLRETURN length_probe =
        SQLGetData(stmt_, 1, SQL_C_CHAR, &probe, 0, &indicator);
    EXPECT_TRUE(SQL_SUCCEEDED(length_probe));
    EXPECT_EQ(5, indicator);
    EXPECT_EQ('Q', probe);

    probe = 'Q';
    indicator = -1;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO,
              SQLGetData(stmt_, 1, SQL_C_CHAR, &probe, 1, &indicator));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(5, indicator);
    EXPECT_EQ(0, probe);

    SQLCHAR narrow_output[8] = {};
    indicator = -1;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_CHAR, narrow_output,
                                      sizeof(narrow_output), &indicator));
    EXPECT_EQ(5, indicator);
    EXPECT_EQ("probe",
              std::string(reinterpret_cast<const char*>(narrow_output), 5));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Every distinct server code-page mapping, including each SQL_ sort ID, must
// decode the bytes SQL Server produced the way SQL Server itself decodes them.
// Benefits-from-mock-tds: scripted COLMETADATA could pin each collation's
// code page without SQL Server converting the probe text first.
ODBC_TEST_F(ClientCodePageTest, CollationSweepMatchesServerDecoding) {
    if (!ODBCTestUtils::HasCodePageConverter(client_code_page_)) {
        GTEST_SKIP() << MissingConverterMessage(client_code_page_);
    }
    // Windows collations differing only in sensitivity flags share a code page;
    // SQL_ collations keep their own row because each has its own sort ID.
    const std::string list_sql =
        "SELECT name, CONVERT(int, COLLATIONPROPERTY(name, 'CodePage')), "
        "CONVERT(int, SUBSTRING(CONVERT(varbinary(5), COLLATIONPROPERTY(name, "
        "'TDSCollation')), 5, 1)) FROM (SELECT name, ROW_NUMBER() OVER (PARTITION BY "
        "CASE WHEN name LIKE 'SQL[_]%' THEN name ELSE '' END, "
        "COLLATIONPROPERTY(name, 'LCID'), COLLATIONPROPERTY(name, 'Version'), "
        "CASE WHEN name LIKE '%[_]UTF8%' THEN 1 ELSE 0 END, "
        "CASE WHEN name LIKE '%[_]BIN2%' THEN 2 WHEN name LIKE '%[_]BIN%' THEN 1 ELSE 0 END "
        "ORDER BY name) rn FROM sys.fn_helpcollations() "
        "WHERE COLLATIONPROPERTY(name, 'CodePage') <> 0) c WHERE rn = 1 ORDER BY name";
    std::vector<std::vector<std::string>> rows;
    ASSERT_EQ("", ReadRows(list_sql, 3, SQL_C_CHAR, false, rows));

    struct Collation {
        std::string name;
        std::string code_page;
    };
    std::vector<Collation> collations;
    const bool msodbcsql = ComparingMsodbcsql();
    for (const auto& row : rows) {
        // msodbcsql's sort-ID table lacks the IDs SQL Server 2022 added
        // (get_data_test.cpp, Issue627SqlServerExtendedSortIds*).
        const int sort_id = std::stoi(row[2]);
        if (msodbcsql && (sort_id == 122 || sort_id >= 210)) {
            continue;
        }
        collations.push_back({row[0], row[1]});
    }
    ASSERT_GT(collations.size(), 100u);

    std::vector<std::string> failures;
    constexpr size_t kBatch = 64;
    for (size_t begin = 0; begin < collations.size(); begin += kBatch) {
        const size_t end = (std::min)(begin + kBatch, collations.size());
        // Per collation: SQL Server's own decoding, then bounded and PLP values.
        std::string sql = "SELECT ";
        for (size_t i = begin; i < end; ++i) {
            const std::string value = "t.v COLLATE " + collations[i].name;
            sql += (i == begin ? "" : ", ") + std::string("CAST(CAST(") + value +
                   " AS varchar(200)) AS nvarchar(200)), CAST(" + value +
                   " AS varchar(200)), CAST(" + value + " AS varchar(max))";
        }
        sql += " FROM (SELECT " + std::string(kSweepText) + " AS v) t";
        const auto columns = static_cast<SQLUSMALLINT>((end - begin) * 3);
        const std::string range = collations[begin].name + ".." + collations[end - 1].name;

        std::string problem = ReadRows(sql, columns, SQL_C_WCHAR, false, rows);
        if (problem.empty() && rows.size() != 1) {
            problem = std::to_string(rows.size()) + " rows";
        }
        if (!problem.empty()) {
            failures.push_back(range + " server decoding: " + problem);
            continue;
        }
        const std::vector<std::string> truth = rows[0];

        for (const Route& route : kRoutes) {
            problem = ReadRows(sql, columns, route.c_type, route.bound, rows);
            if (problem.empty() && rows.size() != 1) {
                problem = std::to_string(rows.size()) + " rows";
            }
            if (!problem.empty()) {
                failures.push_back(range + " " + route.name + ": " + problem);
                continue;
            }
            for (size_t i = begin; i < end; ++i) {
                const size_t column = (i - begin) * 3;
                std::string expected = truth[column];
                if (route.c_type == SQL_C_CHAR) {
                    std::string reason;
                    expected = NarrowFor(truth[column], reason);
                    if (!reason.empty()) {
                        failures.push_back(collations[i].name + " " + route.name + ": " + reason);
                        continue;
                    }
                }
                for (size_t offset : {1, 2}) {
                    const std::string& actual = rows[0][column + offset];
                    if (actual != expected) {
                        failures.push_back(collations[i].name + " (" + collations[i].code_page +
                                           ") " + (offset == 1 ? "varchar(200) " : "varchar(max) ") +
                                           route.name + ": expected " + Hex(expected) + " got " +
                                           Hex(actual));
                    }
                }
            }
        }
    }
    std::string report;
    for (size_t i = 0; i < failures.size() && i < 20; ++i) {
        report += "\n  " + failures[i];
    }
    EXPECT_TRUE(failures.empty()) << failures.size() << " mismatches across "
                                  << collations.size() << " collations:" << report;
}

// Columns without COLLATE take the database default, which differs from the
// server default here; narrow parameters must also follow the new database.
// Each run creates a database, so the concurrent modes would create hundreds.
// Benefits-from-mock-tds: a scripted ENVCHANGE could switch the database
// collation without CREATE DATABASE permission.
ODBC_TEST_F_MODES(ClientCodePageTest, DatabaseDefaultCollationDiffersFromServer,
                  odbc_concurrency::kSingle | odbc_concurrency::kCrossThread) {
    if (!ODBCTestUtils::HasCodePageConverter(client_code_page_)) {
        GTEST_SKIP() << MissingConverterMessage(client_code_page_);
    }
    std::vector<std::vector<std::string>> rows;
    ASSERT_EQ("", ReadRows("SELECT CONVERT(int, COLLATIONPROPERTY(CONVERT(sysname, "
                           "SERVERPROPERTY('Collation')), 'CodePage'))",
                           1, SQL_C_CHAR, false, rows));
    ASSERT_EQ(1u, rows.size());

    struct DatabaseText {
        const char* collation;
        const char* code_page;
        const char* expression;
        const char* utf8;
        const char* stored;
        SQLWCHAR unit;
    };
    const DatabaseText text =
        rows[0][0] == "949"
            ? DatabaseText{"Cyrillic_General_100_CI_AS", "1251", "NCHAR(1071)", "\xD0\xAF",
                           "0xDF", 0x042F}
            : DatabaseText{"Korean_100_CI_AS", "949", "NCHAR(54620)", "\xED\x95\x9C", "0xC7D1",
                           0xD55C};

    PrivateDatabase database(stmt_);
    SQLRETURN rc =
        Execute("CREATE DATABASE [" + database.name + "] COLLATE " + text.collation);
    if (!SQL_SUCCEEDED(rc)) {
        const std::string reason = ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        SQLFreeStmt(stmt_, SQL_CLOSE);
        GTEST_SKIP() << "CREATE DATABASE unavailable: " << reason;
    }
    database.created = true;
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(Execute("USE [" + database.name + "]"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ("", ReadRows("SELECT CONVERT(int, COLLATIONPROPERTY(CONVERT(sysname, "
                           "DATABASEPROPERTYEX(DB_NAME(), 'Collation')), 'CodePage'))",
                           1, SQL_C_CHAR, false, rows));
    ASSERT_EQ(1u, rows.size());
    ASSERT_EQ(text.code_page, rows[0][0]);

    // Real tables: #temp columns would take tempdb's collation instead.
    const std::string expression = text.expression;
    ASSERT_SQL_OK(Execute("CREATE TABLE dbo.t(d varchar(20), o varchar(20) COLLATE "
                          "Greek_100_CI_AS, m varchar(max)); INSERT dbo.t VALUES(" +
                          expression + ", NCHAR(937), " + expression + ")"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);

    const std::vector<std::string> wide = {text.utf8, "\xCE\xA9", text.utf8, text.utf8};
    std::vector<std::string> narrow;
    for (const auto& value : wide) {
        narrow.push_back(ODBCTestUtils::Utf8ToCodePage(value, client_code_page_));
    }
    const std::string select =
        "SELECT d, o, m, CAST(" + expression + " AS varchar(20)) FROM dbo.t";
    for (const Route& route : kRoutes) {
        SCOPED_TRACE(route.name);
        ASSERT_EQ("", ReadRows(select, 4, route.c_type, route.bound, rows));
        ASSERT_EQ(1u, rows.size());
        const auto& expected = route.c_type == SQL_C_CHAR ? narrow : wide;
        for (size_t column = 0; column < expected.size(); ++column) {
            EXPECT_EQ(Hex(expected[column]), Hex(rows[0][column])) << "column " << column + 1;
        }
    }

    ASSERT_SQL_OK(Execute("CREATE TABLE dbo.p(v varchar(20))"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR value[2] = {text.unit, 0};
    SQLLEN indicator = SQL_NTS;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_VARCHAR, 20, 0,
                                   value, sizeof(value), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    rc = Execute("INSERT dbo.p VALUES(?)");
    const std::string reason = ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    SQLFreeStmt(stmt_, SQL_CLOSE);
    SQLFreeStmt(stmt_, SQL_RESET_PARAMS);
    ASSERT_TRUE(SQL_SUCCEEDED(rc)) << reason;
    ASSERT_EQ("", ReadRows("SELECT v, CONVERT(varchar(20), CONVERT(varbinary(20), v), 1) "
                           "FROM dbo.p",
                           2, SQL_C_WCHAR, false, rows));
    ASSERT_EQ(1u, rows.size());
    EXPECT_EQ(Hex(text.utf8), Hex(rows[0][0]));
    EXPECT_EQ(text.stored, rows[0][1]);
}

// sql_variant values carry their own collation, so one column can need a
// different decoder on every row.
// Benefits-from-mock-tds: a scripted sql_variant payload could pair any
// collation with any bytes, including ones SQL Server would not store.
ODBC_TEST_F(ClientCodePageTest, SqlVariantCarriesPerValueCollation) {
    if (!ODBCTestUtils::HasCodePageConverter(client_code_page_)) {
        GTEST_SKIP() << MissingConverterMessage(client_code_page_);
    }
    std::vector<const ServerText*> texts;
    for (const auto& text : kServerCollations) {
        if (text.code_page != 0) {
            texts.push_back(&text);
        }
    }
    for (const auto& text : kServerText) {
        texts.push_back(&text);
    }
    std::string sql = "SELECT n, v FROM (VALUES ";
    for (size_t i = 0; i < texts.size(); ++i) {
        sql += (i == 0 ? "(" : ", (") + std::to_string(i) + ", CAST(CAST((" +
               texts[i]->expression + ") COLLATE " + texts[i]->collation +
               " AS varchar(20)) AS sql_variant))";
    }
    sql += ") t(n, v) ORDER BY n";

    for (const Route& route : kRoutes) {
        SCOPED_TRACE(route.name);
        std::vector<std::vector<std::string>> rows;
        ASSERT_EQ("", ReadRows(sql, 2, route.c_type, route.bound, rows));
        ASSERT_EQ(texts.size(), rows.size());
        for (size_t i = 0; i < texts.size(); ++i) {
            SCOPED_TRACE(texts[i]->collation);
            EXPECT_EQ(std::to_string(i), rows[i][0]);
            const std::string expected =
                route.c_type == SQL_C_CHAR
                    ? ODBCTestUtils::Utf8ToCodePage(texts[i]->utf8, client_code_page_)
                    : texts[i]->utf8;
            EXPECT_EQ(Hex(expected), Hex(rows[i][1]));
        }
    }
}

// Invalid server bytes decode with replacement characters and SQL_SUCCESS on
// both drivers; neither raises 22018 (docs/parity-deviations.md, entry 45).
// Benefits-from-mock-tds: a mock could send sequences SQL Server refuses to
// store, such as a trailing CP932 lead byte or a lone UTF-8 lead byte.
ODBC_TEST_F(ClientCodePageTest, MalformedServerBytesDecodeWithoutError) {
    if (!ODBCTestUtils::HasCodePageConverter(client_code_page_)) {
        GTEST_SKIP() << MissingConverterMessage(client_code_page_);
    }
    const bool msodbcsql = ComparingMsodbcsql();
    for (const auto& value : kMalformedValues) {
        SCOPED_TRACE(std::string(value.collation) + " " + value.type + " " + value.hex);
        const std::string prefix = "SET NOCOUNT ON; DECLARE @t TABLE(v " +
                                   std::string(value.type) + " COLLATE " + value.collation +
                                   "); INSERT @t VALUES(" + value.hex + "); SELECT ";
        std::vector<std::vector<std::string>> rows;
        ASSERT_EQ("", ReadRows(prefix + "CONVERT(varchar(20), CONVERT(varbinary(20), v), 1) "
                                        "FROM @t",
                               1, SQL_C_CHAR, false, rows));
        ASSERT_EQ(1u, rows.size());
        ASSERT_EQ(value.hex, rows[0][0]) << "SQL Server changed the stored bytes";

        const std::string wide = Utf16ToUtf8(msodbcsql ? value.msodbcsql : value.mssql_odbc);
        for (const Route& route : kRoutes) {
            // Same-page narrow output is not pinned: it may pass the server
            // bytes through rather than replace them.
            if (route.c_type == SQL_C_CHAR && value.code_page == client_code_page_) {
                continue;
            }
            SCOPED_TRACE(route.name);
            const std::string expected =
                route.c_type == SQL_C_CHAR
                    ? ODBCTestUtils::Utf8ToCodePage(wide, client_code_page_)
                    : wide;
            ASSERT_EQ("", ReadRows(prefix + "v FROM @t", 1, route.c_type, route.bound, rows));
            ASSERT_EQ(1u, rows.size());
            EXPECT_EQ(Hex(expected), Hex(rows[0][0]));
        }
    }
}
