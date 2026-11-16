// Copyright (c) Microsoft Corporation. All rights reserved.

#include "odbc_concurrent_test.h"
#include "odbc_sql_server_types.h"
#include "odbc_test_fixture.h"

#include <algorithm>
#include <cstdlib>
#include <iterator>
#include <stdexcept>
#include <string>
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
        ASSERT_TRUE(parsed == 1252 || parsed == 65001 || parsed == 932 || parsed == 1251);
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

    SQLRETURN Execute(const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }
};

const ServerText& TextForCodePage(unsigned code_page) {
    const auto found = std::find_if(std::begin(kServerText), std::end(kServerText),
                                    [code_page](const ServerText& text) {
                                        return text.code_page == code_page;
                                    });
    if (found == std::end(kServerText)) {
        throw std::invalid_argument("unsupported test code page");
    }
    return *found;
}

std::string MissingConverterMessage(unsigned code_page) {
    return "host iconv cannot convert UTF-8 to code page " + std::to_string(code_page) +
           " (musl before 1.2.5 has no cp932; RHEL needs glibc-gconv-extra)";
}

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
    if (!ODBCTestUtils::HasCodePageConverter(client_code_page_)) {
        GTEST_SKIP() << MissingConverterMessage(client_code_page_);
    }
    const ServerText& text = TextForCodePage(client_code_page_);
    std::string sql = VarcharQuery(text);
    if (client_code_page_ == 1252) {
        sql = "SELECT CAST(NCHAR(65) + NCHAR(8364) + NCHAR(66) COLLATE " +
              std::string(text.collation) + " AS varchar(max))";
    } else if (client_code_page_ == 932) {
        sql = "SELECT CAST(NCHAR(65) + NCHAR(12354) + NCHAR(66) COLLATE " +
              std::string(text.collation) + " AS varchar(max))";
    } else if (client_code_page_ == 1251) {
        sql = "SELECT CAST(NCHAR(65) + NCHAR(1046) + NCHAR(66) COLLATE " +
              std::string(text.collation) + " AS varchar(max))";
    } else {
        sql = "SELECT CAST(NCHAR(65) + NCHAR(55357) + NCHAR(56832) + NCHAR(66) COLLATE " +
              std::string(text.collation) + " AS varchar(max))";
    }
    const std::string utf8_value =
        client_code_page_ == 1252 ? "A\xE2\x82\xAC" "B" :
        client_code_page_ == 932 ? "A\xE3\x81\x82" "B" :
        client_code_page_ == 1251 ? "A\xD0\x96" "B" :
                                    "A\xF0\x9F\x98\x80" "B";
    const std::string encoded =
        ODBCTestUtils::Utf8ToCodePage(utf8_value, client_code_page_);
    // AB#47564: SQL_C_CHAR chunks fill the byte capacity even mid-character, so
    // the multibyte character splits across the first two chunks.
    const bool multibyte_split = client_code_page_ == 65001 || client_code_page_ == 932;
    const std::vector<size_t> chunk_sizes =
        client_code_page_ == 65001 ? std::vector<size_t>{4, 2} :
        client_code_page_ == 932 ? std::vector<size_t>{2, 2} :
                                   std::vector<size_t>{1, 1, 1};
    const SQLLEN buffer_length =
        client_code_page_ == 65001 ? 5 : client_code_page_ == 932 ? 3 : 2;
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
