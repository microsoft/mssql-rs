// Copyright (c) Microsoft Corporation. All rights reserved.
// fetch_scroll_test.cpp  –  E2E tests for SQLFetchScroll.
//
// These cover the rowset machinery that does not depend on bound columns:
// cursor advance, *rows_fetched_ptr, the row status array, end-of-set, and the
// forward-only orientation rule. The bound-column fill loop needs SQLBindCol
// (AB#47359) before it can be driven from here; those cases arrive with it.
//
// Verifies:
//   1. NullHandle                          - SQL_NULL_HSTMT → SQL_INVALID_HANDLE
//   2. FreshStatementIsASequenceError      - never executed → HY010 (from the DM)
//   3. OnlyFetchNextIsSupported            - forward-only cursor → HY106
//   4. AdvancesTheCursorLikeFetch          - rowset of 1, then SQLGetData
//   5. ReportsRowsFetched                  - *rows_fetched_ptr per call
//   6. FillsTheRowStatusArray              - SQL_ROW_SUCCESS / SQL_ROW_NOROW
//   7. ReturnsNoDataAtEndOfResultSet
//   8. PartialRowsetAtEndOfResultSet       - fewer rows than the array size
//   9-15. Bound-column fetch: rowset fill, several columns, NULL indicators,
//         truncation, unbind, rebind, and mixed SQLGetData afterwards

#include "odbc_test_fixture.h"
#include "utf16_test_data.h"
#include "cp1252_test_data.h"

#include <algorithm>
#include <cstdlib>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

namespace {
std::string RepeatedPrefix(const std::string& character, size_t capacity) {
    std::string result;
    if (character.empty()) return result;
    while (character.size() <= capacity - result.size()) result += character;
    return result;
}

std::string RepeatedBytePrefix(const std::string& character, size_t capacity) {
    std::string result;
    if (character.empty()) return result;
    result.reserve(capacity);
    while (result.size() < capacity) {
        const size_t remaining = capacity - result.size();
        result.append(character, 0, remaining < character.size() ? remaining : character.size());
    }
    return result;
}

std::string ExpectedBoundClientPrefix(const std::string& character, size_t capacity) {
    const char* target = std::getenv("ODBC_TEST_TARGET");
    if (target && std::string(target) == "msodbcsql") {
        return RepeatedBytePrefix(character, capacity);
    }
    return RepeatedPrefix(character, capacity);
}
}

class FetchScrollLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
        SQLCHAR version[32] = {};
        ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                      SQL_HANDLE_DBC, dbc_);
        RecordProperty("driver_version", reinterpret_cast<const char*>(version));
#ifdef _WIN32
        RecordProperty("client_code_page", static_cast<int>(GetACP()));
#endif
    }

    // Three rows, so a rowset larger than the result set can be exercised.
    void ExecThreeRows() {
        ExecDirect(
            "SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 ORDER BY n");
    }

    bool ServerSupportsNativeJson() {
        SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT CAST(N'{}' AS JSON)");
        const bool ok =
            SQL_SUCCEEDED(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS));
        SQLCloseCursor(stmt_);
        return ok;
    }
};

class FetchScrollUtf16Test : public FetchScrollLiveTest {};

TEST_F(FetchScrollLiveTest, BoundTextRowArraysUseClientEncoding) {
    constexpr size_t row_count = 2;
    unsigned char output[row_count][32] = {};
    SQLLEN lengths[row_count] = {};
    SQLUSMALLINT status[row_count] = {};
    SQLULEN fetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                reinterpret_cast<SQLPOINTER>(row_count), 0), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, output, sizeof(output[0]), lengths), SQL_HANDLE_STMT, stmt_);
    for (const char* expression : {
            "CAST(NCHAR(233) + NCHAR(0x20AC) AS nvarchar(16))",
            "CAST((NCHAR(233) + NCHAR(0x20AC)) COLLATE Latin1_General_100_CI_AS AS varchar(16))",
            "CAST(NCHAR(233) + NCHAR(0x20AC) AS nvarchar(max))",
            "CAST(CAST(NCHAR(233) + NCHAR(0x20AC) AS nvarchar(16)) AS sql_variant)"}) {
        SCOPED_TRACE(expression);
        ExecDirect(std::string("SELECT ") + expression + " FROM (VALUES(1), (2)) AS t(n)");
        ASSERT_EQ(SQL_SUCCESS, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
        ASSERT_EQ(row_count, fetched);
        const auto expected = ODBCTestUtils::Utf8ToNativeClient("\xC3\xA9\xE2\x82\xAC");
        for (size_t row = 0; row < row_count; ++row) {
            EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(output[row]), expected.size()));
            EXPECT_EQ(0, output[row][expected.size()]);
            EXPECT_EQ(static_cast<SQLLEN>(expected.size()), lengths[row]);
            EXPECT_EQ(SQL_ROW_SUCCESS, status[row]);
        }
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
    SQLFreeStmt(stmt_, SQL_UNBIND);
}

TEST_F(FetchScrollLiveTest, BoundClientCodePageLossHonorsWarningAttribute) {
    bool had_loss = false;
    const auto expected = ODBCTestUtils::Utf8ToNativeClient("\xF0\x9F\x98\x80", &had_loss);
    for (SQLULEN warn : {0UL, 1UL}) {
        ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_COPT_SS_WARN_ON_CP_ERROR,
                                       reinterpret_cast<SQLPOINTER>(warn), 0), SQL_HANDLE_DBC, dbc_);
        SQLUINTEGER actual_warn = 99;
        ASSERT_SQL_OK(SQLGetConnectAttr(dbc_, SQL_COPT_SS_WARN_ON_CP_ERROR,
                                       &actual_warn, sizeof(actual_warn), nullptr), SQL_HANDLE_DBC, dbc_);
        ASSERT_EQ(warn, actual_warn);
        for (const char* type : {"nvarchar(16)", "nvarchar(max)"}) {
            SCOPED_TRACE(type);
            SCOPED_TRACE(warn);
            for (SQLLEN capacity : {16, 2}) {
                SCOPED_TRACE(capacity);
                ExecDirect(std::string("SELECT CAST(NCHAR(0xD83D) + NCHAR(0xDE00) AS ") + type + ")");
                unsigned char output[16] = {};
                SQLLEN length = -99;
                ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, output, capacity, &length), SQL_HANDLE_STMT, stmt_);
                const bool truncated = expected.size() >= static_cast<size_t>(capacity);
                EXPECT_EQ(truncated ? SQL_SUCCESS_WITH_INFO : SQL_SUCCESS, SQLFetch(stmt_));
                bool saw_truncation = false;
                bool saw_loss = false;
                for (SQLSMALLINT record = 1;; ++record) {
                    SQLCHAR state[6] = {};
                    SQLCHAR message[256] = {};
                    SQLINTEGER native = 0;
                    SQLSMALLINT size = 0;
                    const auto rc = SQLGetDiagRecA(SQL_HANDLE_STMT, stmt_, record, state,
                                                   &native, message, sizeof(message), &size);
                    if (rc == SQL_NO_DATA) break;
                    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
                    const std::string sqlstate(reinterpret_cast<const char*>(state));
                    saw_truncation |= sqlstate == "01004";
                    saw_loss |= sqlstate == "01000";
                    EXPECT_TRUE(sqlstate == "01004" || sqlstate == "01000");
                }
                EXPECT_EQ(truncated, saw_truncation);
                EXPECT_EQ(truncated && warn && had_loss, saw_loss);
                if (!truncated) {
                    EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(output), expected.size()));
                    EXPECT_EQ(0, output[expected.size()]);
                    EXPECT_EQ(static_cast<SQLLEN>(expected.size()), length);
                } else if (had_loss && expected.size() == 2) {
                    EXPECT_EQ(expected.substr(0, 1), reinterpret_cast<const char*>(output));
                }
                ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
                ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
            }
        }
    }
    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_COPT_SS_WARN_ON_CP_ERROR, nullptr, 0), SQL_HANDLE_DBC, dbc_);
}

TEST_F(FetchScrollUtf16Test, RawUnitsInBoundRowArrays) {
    constexpr size_t row_count = 4;
    struct Column {
        SQLWCHAR before;
        SQLWCHAR text[row_count][9];
        SQLWCHAR after;
        SQLLEN lengths[row_count];
    };
    Column varying;
    Column fixed;
    SQLULEN fetched = 0;
    SQLUSMALLINT status[row_count] = {};
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                         reinterpret_cast<SQLPOINTER>(row_count), 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, SQL_C_WCHAR, varying.text,
                                     sizeof(varying.text[0]), varying.lengths));
    ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 2, SQL_C_WCHAR, fixed.text,
                                     sizeof(fixed.text[0]), fixed.lengths));
    const auto& values = Utf16TestData::Values();
    std::string sql = "SELECT CONVERT(nvarchar(32), v), CONVERT(nchar(8), v) FROM (VALUES ";
    for (size_t i = 0; i < values.size(); ++i) {
        if (i != 0) sql += ", ";
        sql += "(" + std::to_string(i) + ", CONVERT(varbinary(32), " + values[i].hex + "))";
    }
    ExecDirect(sql + ") AS t(ord, v) ORDER BY ord");
    for (size_t start = 0; start < values.size(); start += row_count) {
        for (Column* column : {&varying, &fixed}) {
            column->before = column->after = 0xCCCC;
            for (auto& text : column->text) {
                std::fill(std::begin(text), std::end(text), 0xCCCC);
            }
            std::fill(std::begin(column->lengths), std::end(column->lengths), -99);
        }
        ASSERT_EQ(SQL_SUCCESS, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0))
            << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        const size_t count = (std::min)(row_count, values.size() - start);
        ASSERT_EQ(count, fetched);
        for (Column* column : {&varying, &fixed}) {
            EXPECT_EQ(0xCCCC, column->before);
            EXPECT_EQ(0xCCCC, column->after);
        }
        for (size_t i = 0; i < row_count; ++i) {
            SCOPED_TRACE(start + i);
            EXPECT_EQ(i < count ? SQL_ROW_SUCCESS : SQL_ROW_NOROW, status[i]);
            for (const Column* column : {&varying, &fixed}) {
                const SQLWCHAR* buffer = column->text[i];
                const SQLLEN indicator = column->lengths[i];
                if (i >= count || std::string(values[start + i].hex) == "NULL") {
                    EXPECT_EQ(i >= count ? -99 : SQL_NULL_DATA, indicator);
                    if (i >= count) {
                        EXPECT_TRUE(std::all_of(buffer, buffer + 9,
                                               [](SQLWCHAR unit) { return unit == 0xCCCC; }));
                    }
                } else {
                    const auto expected = Utf16TestData::Expected(values[start + i], column == &fixed);
                    EXPECT_EQ(static_cast<SQLLEN>(expected.size() * sizeof(SQLWCHAR)), indicator);
                    EXPECT_TRUE(std::equal(expected.begin(), expected.end(), buffer));
                    EXPECT_EQ(0, buffer[expected.size()]);
                    EXPECT_TRUE(std::all_of(buffer + expected.size() + 1, buffer + 9,
                                           [](SQLWCHAR unit) { return unit == 0xCCCC; }));
                }
            }
        }
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
}

// Benefits-from-mock-tds: this test can only observe that exactly one
// 01003/8153 record surfaces and that the fetch returns SQL_SUCCESS_WITH_INFO.
// It cannot see which TDS read consumed the INFO token, so it cannot tell a
// terminal read-ahead promotion apart from the row loop having already drained
// the message. A byte-level mock TDS server would let it assert that the token
// was still unread when the rowset budget was reached and that the release
// peek is what consumed it. `row_fetch_with_terminal_info_returns_success_with_info`
// pins that split in Rust meanwhile.
TEST_F(FetchScrollLiveTest, TerminalAggregateWarningIsReportedExactlyOnce) {
    SQLCHAR version[32] = {};
    ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    RecordProperty("driver_version", reinterpret_cast<const char*>(version));

    // Exactly as many slots as the query returns rows: the fill loop stops on
    // its own budget without probing a further row, so the terminal INFO is
    // still unread on the wire and only the release peek can consume it. A
    // wider rowset would make the loop read past the last row and drain the
    // message through the ordinary row-loop path instead, leaving the
    // terminal read-ahead promotion untested.
    constexpr SQLULEN row_count = 2;
    SQLINTEGER values[row_count] = {};
    SQLLEN indicators[row_count] = {};
    SQLULEN fetched = 0;
    SQLUSMALLINT status[row_count] = {};

    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                         reinterpret_cast<SQLPOINTER>(row_count), 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(values[0]),
                                     indicators));
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SELECT SUM(v) FROM (VALUES (1, CAST(1 AS int)), (1, NULL), (2, 2), (2, NULL)) "
        "AS t(k, v) GROUP BY k ORDER BY k");
    const SQLRETURN execute_rc =
        SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    ASSERT_TRUE(execute_rc == SQL_SUCCESS || execute_rc == SQL_SUCCESS_WITH_INFO)
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    const bool warning_on_execute =
        ODBCTestUtils::HasDiagState(SQL_HANDLE_STMT, stmt_, "01003");
    EXPECT_EQ(warning_on_execute ? SQL_SUCCESS_WITH_INFO : SQL_SUCCESS, execute_rc);
    RecordProperty("warning_stage", warning_on_execute ? "execute" : "fetch");
    SQLTCHAR state[6] = {};
    SQLTCHAR message[256] = {};
    SQLINTEGER native_error = 0;
    if (warning_on_execute) {
        ASSERT_EQ(SQL_SUCCESS,
                  SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state, &native_error, message,
                                static_cast<SQLSMALLINT>(std::size(message)), nullptr));
        EXPECT_EQ("01003", ODBCTestUtils::ToNarrow(SqlTString(state)));
        EXPECT_EQ(8153, native_error);
        EXPECT_EQ(SQL_NO_DATA,
                  SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 2, state, &native_error, message,
                                static_cast<SQLSMALLINT>(std::size(message)), nullptr));
    }

    // Drivers may expose the terminal INFO while executing or fetching,
    // depending on when their TDS path processes it. The parity contract is
    // one 01003/8153 record and one corresponding SQL_SUCCESS_WITH_INFO,
    // followed by clean, non-replaying EOF fetches.
    const SQLRETURN fetch_rc = SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0);
    EXPECT_EQ(warning_on_execute ? SQL_SUCCESS : SQL_SUCCESS_WITH_INFO, fetch_rc);
    EXPECT_EQ(2u, fetched);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[0]);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[1]);

    const SQLRETURN fetch_diag_rc =
        SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state, &native_error, message,
                      static_cast<SQLSMALLINT>(std::size(message)), nullptr);
    if (warning_on_execute) {
        EXPECT_EQ(SQL_NO_DATA, fetch_diag_rc);
    } else {
        ASSERT_EQ(SQL_SUCCESS, fetch_diag_rc);
        EXPECT_EQ("01003", ODBCTestUtils::ToNarrow(SqlTString(state)));
        EXPECT_EQ(8153, native_error);
        EXPECT_EQ(SQL_NO_DATA,
                  SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 2, state, &native_error, message,
                                static_cast<SQLSMALLINT>(std::size(message)), nullptr));
    }

    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(SQL_NO_DATA,
              SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state, &native_error, message,
                            static_cast<SQLSMALLINT>(std::size(message)), nullptr));
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(SQL_NO_DATA,
              SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state, &native_error, message,
                            static_cast<SQLSMALLINT>(std::size(message)), nullptr));
    ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
}

TEST_F(FetchScrollUtf16Test, Cp1252UnalignedColumnArrayFitsExactCapacity) {
    constexpr size_t row_count = 3;
    constexpr size_t capacity = 514;
    SQLULEN fetched = 0;
    SQLUSMALLINT status[row_count] = {};
    SQLLEN bind_offset = 1;
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
        reinterpret_cast<SQLPOINTER>(row_count), 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_BIND_OFFSET_PTR, &bind_offset, 0));
    const size_t stride = capacity;
    std::vector<unsigned char> buffer(1 + row_count * (std::max)(stride, size_t{1}) + 2, 0xCC);
    const size_t indicator_stride = sizeof(SQLLEN);
    std::vector<unsigned char> lengths(1 + row_count * indicator_stride + sizeof(SQLLEN), 0xCC);
    ASSERT_EQ(1u, reinterpret_cast<uintptr_t>(buffer.data() + 1) % alignof(SQLWCHAR));
    ASSERT_EQ(1u, reinterpret_cast<uintptr_t>(lengths.data() + 1) % alignof(SQLLEN));
    ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, SQL_C_WCHAR, buffer.data(), capacity,
        reinterpret_cast<SQLLEN*>(lengths.data())));
    const auto& values = Cp1252TestData::Values();
    ASSERT_FALSE(values.empty());
    ASSERT_EQ(capacity, (values.front().units.size() + 1) * sizeof(SQLWCHAR));
    ASSERT_EQ(0u, values.size() % row_count);
    std::string sql = "SET NOCOUNT ON; DECLARE @t TABLE(ord int, v varchar(256) "
        "COLLATE Latin1_General_100_CI_AS); INSERT @t VALUES ";
    for (size_t i = 0; i < values.size(); ++i) {
        if (i != 0) sql += ",";
        sql += "(" + std::to_string(i) + "," + values[i].hex + ")";
    }
    ExecDirect(sql + "; SELECT v FROM @t ORDER BY ord");
    for (size_t start = 0; start < values.size(); start += row_count) {
        std::fill(buffer.begin(), buffer.end(), 0xCC);
        std::fill(lengths.begin(), lengths.end(), 0xCC);
        ASSERT_EQ(SQL_SUCCESS, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0))
            << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("", StmtDiagState());
        EXPECT_EQ(row_count, fetched);
        auto expected_lengths = std::vector<unsigned char>(lengths.size(), 0xCC);
        for (size_t row = 0; row < row_count; ++row) {
            SCOPED_TRACE(start + row);
            const auto& value = values[start + row];
            const bool is_null = value.hex == "NULL";
            ASSERT_LT(value.units.size(), capacity / sizeof(SQLWCHAR));
            EXPECT_EQ(SQL_ROW_SUCCESS, status[row]);
            const SQLLEN length = is_null ? SQL_NULL_DATA :
                static_cast<SQLLEN>(value.units.size() * 2);
            std::memcpy(expected_lengths.data() + 1 + row * indicator_stride, &length, sizeof(length));
            if (!is_null) {
                const auto* payload = buffer.data() + 1 + row * stride;
                if (!value.units.empty()) {
                    EXPECT_EQ(0, std::memcmp(payload, value.units.data(), value.units.size() * 2));
                }
                SQLWCHAR terminator = 0xCCCC;
                std::memcpy(&terminator, payload + value.units.size() * 2, sizeof(terminator));
                EXPECT_EQ(0, terminator);
            }
        }
        EXPECT_EQ(0xCC, buffer.front());
        EXPECT_EQ(0xCC, buffer.back());
        EXPECT_EQ(expected_lengths, lengths);
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
    ASSERT_EQ(SQL_SUCCESS, SQLFreeStmt(stmt_, SQL_UNBIND));
}

TEST_F(FetchScrollUtf16Test, NullDataPointersStayUnboundWithOffsetAndLiveIndicators) {
    constexpr size_t row_count = 2;
    constexpr size_t capacity = 6;
    SQLLEN bind_offset = 1;
    SQLULEN fetched = 0;
    SQLUSMALLINT status[row_count] = {};
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
        reinterpret_cast<SQLPOINTER>(row_count), 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_BIND_OFFSET_PTR, &bind_offset, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0));
    const SQLWCHAR expected[][3] = {
        {0x20AC, 0x2018, 0}, {0x0041, 0x00E9, 0},
        {0x2019, 0, 0}, {0x00FF, 0x00FE, 0},
    };
    for (bool manual_ard : {false, true}) {
        SCOPED_TRACE(manual_ard);
        std::vector<unsigned char> unused(row_count * capacity + 2, 0xCC);
        std::vector<unsigned char> unused_lengths(row_count * sizeof(SQLLEN) + 2, 0xCC);
        std::vector<unsigned char> bound(unused.size(), 0xCC);
        std::vector<unsigned char> bound_lengths(unused_lengths.size(), 0xCC);
        auto* indicators = reinterpret_cast<SQLLEN*>(unused_lengths.data());
        ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, SQL_C_WCHAR,
            unused.data(), capacity, indicators));
        ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 2, SQL_C_WCHAR,
            bound.data(), capacity, reinterpret_cast<SQLLEN*>(bound_lengths.data())));
        if (manual_ard) {
            SQLHDESC ard = SQL_NULL_HDESC;
            ASSERT_EQ(SQL_SUCCESS, SQLGetStmtAttrW(stmt_, SQL_ATTR_APP_ROW_DESC, &ard, 0, nullptr));
            ASSERT_SQL_OK(SQLSetDescFieldW(ard, 1, SQL_DESC_DATA_PTR, nullptr, 0),
                SQL_HANDLE_DESC, ard);
            SQLPOINTER pointer = unused.data();
            ASSERT_SQL_OK(SQLGetDescFieldW(ard, 1, SQL_DESC_DATA_PTR, &pointer, 0, nullptr),
                SQL_HANDLE_DESC, ard);
            ASSERT_EQ(nullptr, pointer);
            ASSERT_SQL_OK(SQLGetDescFieldW(ard, 1, SQL_DESC_INDICATOR_PTR, &pointer, 0, nullptr),
                SQL_HANDLE_DESC, ard);
            ASSERT_EQ(static_cast<SQLPOINTER>(indicators), pointer);
            ASSERT_SQL_OK(SQLGetDescFieldW(ard, 1, SQL_DESC_OCTET_LENGTH_PTR, &pointer, 0, nullptr),
                SQL_HANDLE_DESC, ard);
            ASSERT_EQ(static_cast<SQLPOINTER>(indicators), pointer);
        } else {
            ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, SQL_C_WCHAR, nullptr, capacity, indicators));
        }
        ExecDirect("SET NOCOUNT ON; DECLARE @t TABLE(ord int, v varchar(2) "
            "COLLATE Latin1_General_100_CI_AS); INSERT @t VALUES "
            "(1,0x8091),(2,0x41E9),(3,0x9200),(4,0xFFFE); SELECT v,v FROM @t ORDER BY ord");
        for (size_t start = 0; start < 4; start += row_count) {
            SCOPED_TRACE(start);
            std::fill(bound.begin(), bound.end(), 0xCC);
            std::fill(bound_lengths.begin(), bound_lengths.end(), 0xCC);
            ASSERT_EQ(SQL_SUCCESS, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0))
                << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ("", StmtDiagState());
            ASSERT_EQ(row_count, fetched);
            EXPECT_EQ(std::vector<unsigned char>(unused.size(), 0xCC), unused);
            EXPECT_EQ(std::vector<unsigned char>(unused_lengths.size(), 0xCC), unused_lengths);
            for (size_t row = 0; row < row_count; ++row) {
                EXPECT_EQ(SQL_ROW_SUCCESS, status[row]);
                EXPECT_EQ(0, std::memcmp(bound.data() + bind_offset + row * capacity,
                    expected[start + row], capacity));
                SQLLEN length = -99;
                std::memcpy(&length, bound_lengths.data() + bind_offset + row * sizeof(SQLLEN),
                    sizeof(length));
                EXPECT_EQ(4, length);
            }
            EXPECT_EQ(0xCC, bound.front());
            EXPECT_EQ(0xCC, bound.back());
            EXPECT_EQ(0xCC, bound_lengths.front());
            EXPECT_EQ(0xCC, bound_lengths.back());
        }
        EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
        EXPECT_EQ(0u, fetched);
        EXPECT_EQ(std::vector<unsigned char>(unused_lengths.size(), 0xCC), unused_lengths);
        ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
        ASSERT_EQ(SQL_SUCCESS, SQLFreeStmt(stmt_, SQL_UNBIND));
    }
}

TEST_F(FetchScrollUtf16Test, BoundedRawUnitsTruncateBoundRows) {
    constexpr size_t row_count = 3;
    struct Column {
        SQLWCHAR before;
        SQLWCHAR text[row_count][3];
        SQLWCHAR after;
        SQLLEN lengths[row_count];
    } column;
    SQLULEN fetched = 0;
    SQLUSMALLINT status[row_count] = {};
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                         reinterpret_cast<SQLPOINTER>(row_count), 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0));
    ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, SQL_C_WCHAR, column.text,
                                     sizeof(column.text[0]), column.lengths));
    const SQLWCHAR expected[3][2] = {{0xD800, 0x0041}, {0xDC00, 0}, {0xD83D, 0xDE00}};
    for (const char* type : {"nvarchar(32)", "nchar(4)"}) {
        SCOPED_TRACE(type);
        ExecDirect("SELECT CONVERT(" + std::string(type) +
            ", v) FROM (VALUES (1, 0x00D84100FFFEFEFF), (2, 0x00DC000041000000), "
            "(3, 0x3DD800DE41000000)) AS t(ord, v) ORDER BY ord");
        column.before = column.after = 0xCCCC;
        for (auto& text : column.text) {
            std::fill(std::begin(text), std::end(text), 0xCCCC);
        }
        std::fill(std::begin(column.lengths), std::end(column.lengths), -99);
        ASSERT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0))
            << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
        ASSERT_EQ(row_count, fetched);
        EXPECT_EQ(0xCCCC, column.before);
        EXPECT_EQ(0xCCCC, column.after);
        for (size_t i = 0; i < row_count; ++i) {
            SCOPED_TRACE(i);
            EXPECT_EQ(SQL_ROW_SUCCESS_WITH_INFO, status[i]);
            EXPECT_EQ(8, column.lengths[i]);
            EXPECT_EQ(expected[i][0], column.text[i][0]);
            EXPECT_EQ(expected[i][1], column.text[i][1]);
            EXPECT_EQ(0, column.text[i][2]);
        }
        EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
        ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
    }
}

TEST_F(FetchScrollUtf16Test, RawUnitsFitExactBoundCapacity) {
    for (const auto& value : Utf16TestData::Values()) {
        SCOPED_TRACE(value.name);
        for (const char* type : {"nvarchar(32)", "nchar(8)", "nvarchar(max)"}) {
            SCOPED_TRACE(type);
            const auto expected = Utf16TestData::Expected(value, std::string(type) == "nchar(8)");
            const bool is_null = std::string(value.hex) == "NULL";
            const size_t capacity = expected.size() + 1;
            std::vector<SQLWCHAR> buffer(capacity + 2, 0xCCCC);
            SQLLEN indicator = -99;
            ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, SQL_C_WCHAR, buffer.data() + 1,
                static_cast<SQLLEN>(capacity * sizeof(SQLWCHAR)), &indicator));
            ExecDirect("SELECT CONVERT(" + std::string(type) + ", " + value.hex + ")");
            ASSERT_EQ(SQL_SUCCESS, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
            EXPECT_EQ(is_null ? SQL_NULL_DATA :
                      static_cast<SQLLEN>(expected.size() * sizeof(SQLWCHAR)), indicator);
            EXPECT_EQ(0xCCCC, buffer.front());
            EXPECT_EQ(0xCCCC, buffer.back());
            if (!is_null) {
                EXPECT_TRUE(std::equal(expected.begin(), expected.end(), buffer.begin() + 1));
                EXPECT_EQ(0, buffer[capacity]);
            }
            EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
            ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
            ASSERT_EQ(SQL_SUCCESS, SQLFreeStmt(stmt_, SQL_UNBIND));
        }
    }
}

// Live SQL Server reproduction; bound_wide_plp_preserves_units_across_wire_chunks
// in api/fetch_scroll.rs additionally forces a pair across a PLP chunk boundary.
TEST_F(FetchScrollUtf16Test, BoundTruncationPreservesOnlyCompletePairs) {
    struct Boundary {
        const char* hex;
        size_t capacity;
        std::vector<SQLWCHAR> expected;
    };
    const Boundary cases[] = {
        {"0x41003DD800DE4200", 2, {0x0041}},
        {"0x41003DD800DE4200", 3, {0x0041}},
        {"0x41003DD800DE4200", 4, {0x0041, 0xD83D, 0xDE00}},
        {"0x41003DD800DE4200", 5, {0x0041, 0xD83D, 0xDE00, 0x0042}},
        {"0x410000D842004300", 2, {0x0041}},
        {"0x410000D842004300", 3, {0x0041, 0xD800}},
        {"0x410000D842004300", 4, {0x0041, 0xD800, 0x0042}},
        {"0x410000D842004300", 5, {0x0041, 0xD800, 0x0042, 0x0043}},
        {"0x3DD800DE41004200", 2, {}},
        {"0x3DD800DE41004200", 3, {0xD83D, 0xDE00}},
        {"0x3DD800DE41004200", 4, {0xD83D, 0xDE00, 0x0041}},
        {"0x3DD800DE41004200", 5, {0xD83D, 0xDE00, 0x0041, 0x0042}},
    };
    for (const char* type : {"nvarchar(32)", "nchar(4)", "nvarchar(max)"}) {
        SCOPED_TRACE(type);
        for (const auto& boundary : cases) {
            SCOPED_TRACE(boundary.hex);
            SCOPED_TRACE(boundary.capacity);
            std::vector<SQLWCHAR> buffer(boundary.capacity + 2, 0xCCCC);
            SQLLEN indicator = -99;
            ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, SQL_C_WCHAR, buffer.data() + 1,
                static_cast<SQLLEN>(boundary.capacity * sizeof(SQLWCHAR)), &indicator));
            ExecDirect("SELECT CONVERT(" + std::string(type) +
                ", v) FROM (VALUES (1, " + boundary.hex +
                "), (2, 0x4100420043004400)) AS t(ord, v) ORDER BY ord");
            std::vector<SQLWCHAR> ordinary = {0x0041, 0x0042, 0x0043, 0x0044};
            ordinary.resize(boundary.capacity - 1);
            // The second row reuses the binding and buffer without resetting them.
            for (const auto& expected : {boundary.expected, ordinary}) {
                const bool truncated = boundary.capacity < 5;
                ASSERT_EQ(truncated ? SQL_SUCCESS_WITH_INFO : SQL_SUCCESS,
                          SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
                if (truncated) {
                    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
                }
                EXPECT_EQ(8, indicator);
                EXPECT_TRUE(std::equal(expected.begin(), expected.end(), buffer.begin() + 1));
                EXPECT_EQ(0, buffer[expected.size() + 1]);
                EXPECT_EQ(0xCCCC, buffer.front());
                EXPECT_EQ(0xCCCC, buffer.back());
            }
            EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
            ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
            ASSERT_EQ(SQL_SUCCESS, SQLFreeStmt(stmt_, SQL_UNBIND));
        }
    }
}

TEST(FetchScrollTest, NullHandle) {
    EXPECT_EQ(SQL_INVALID_HANDLE, SQLFetchScroll(SQL_NULL_HSTMT, SQL_FETCH_NEXT, 0));
}

// A statement that has never been executed has no result set, and the Driver
// Manager answers this one itself with HY010 without reaching the driver. The
// driver's own 24000 for an executed-but-closed cursor is covered by its unit
// tests, which call it directly.
TEST_F(FetchScrollLiveTest, FreshStatementIsASequenceError) {
    EXPECT_EQ(SQL_ERROR, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

// The cursor is forward-only, so a scrolling orientation is rejected rather
// than quietly treated as SQL_FETCH_NEXT.
TEST_F(FetchScrollLiveTest, OnlyFetchNextIsSupported) {
    ExecThreeRows();
    for (SQLSMALLINT orientation :
         {SQL_FETCH_PRIOR, SQL_FETCH_FIRST, SQL_FETCH_LAST, SQL_FETCH_ABSOLUTE,
          SQL_FETCH_RELATIVE}) {
        EXPECT_EQ(SQL_ERROR, SQLFetchScroll(stmt_, orientation, 0))
            << "orientation " << orientation;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY106");
    }
    SQLCloseCursor(stmt_);
}

// With the default rowset of one and no bound columns, SQLFetchScroll is
// SQLFetch: it positions the cursor and SQLGetData reads the row.
TEST_F(FetchScrollLiveTest, AdvancesTheCursorLikeFetch) {
    ExecThreeRows();
    for (int expected = 1; expected <= 3; ++expected) {
        ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER value = 0;
        SQLLEN indicator = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, value);
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicator);
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    SQLCloseCursor(stmt_);
}

// AB#48943: SQL_ATTR_ROW_ARRAY_SIZE is an alias of the ARD header's
// SQL_DESC_ARRAY_SIZE, so the descriptor spelling must size the rowset the
// fetch actually delivers, and both getters must report one value. The
// pre-fix driver accepted the descriptor write, then fetched a single row and
// reported success. The terminal call is included deliberately: it takes the
// already-exhausted fast path, which sizes its work from that same descriptor
// field.
//
// That path also fills the row status array with SQL_ROW_NOROW, which
// msodbcsql does not do - measured on build 180837, Linux and Windows: the
// reference driver returns SQL_NO_DATA with zero rows fetched and leaves the
// array untouched. The behaviour predates AB#48943 (`mark_no_rows` on the
// exhausted path) and this was simply the first case to compare it against the
// reference. Asserting it here would be asserting one driver's behaviour on a
// shared parity case, so the terminal checks below stay on the return code and
// the fetched count, which both drivers agree on. The status fill itself is
// pinned at the driver boundary by the unit test
// `exhausted_cursor_fast_path_marks_the_status_array_from_the_ard`.
TEST_F(FetchScrollLiveTest, DescriptorArraySizeSizesTheRowset) {
    ExecThreeRows();

    SQLHDESC ard = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC, &ard, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetDescField(ard, 0, SQL_DESC_ARRAY_SIZE,
                                  reinterpret_cast<SQLPOINTER>(4), 0),
                  SQL_HANDLE_DESC, ard);

    SQLULEN reported = 0;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE, &reported, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(4u, reported) << "the two spellings are one value";

    std::vector<SQLINTEGER> values(4, 0);
    std::vector<SQLLEN> indicators(4, 0);
    std::vector<SQLUSMALLINT> status(4, 0xFFFF);
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values.data(), sizeof(SQLINTEGER),
                             indicators.data()),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status.data(), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched) << "the descriptor sized the rowset";
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(3, values[2]);
    EXPECT_EQ(SQL_ROW_NOROW, status[3]);

    rowsFetched = 999;
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(0u, rowsFetched);
    SQLCloseCursor(stmt_);
}

// AB#49060: rowset controls set only through the ARD/IRD descriptor fields
// drive the fetch.
TEST_F(FetchScrollLiveTest, DescriptorSpellingsDriveTheBlockFetch) {
    ExecThreeRows();

    SQLHDESC ard = SQL_NULL_HDESC;
    SQLHDESC ird = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC, &ard, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_IMP_ROW_DESC, &ird, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);

    // The offset skips two SQLINTEGER values and one SQLLEN indicator.
    std::vector<SQLINTEGER> values(5, -1);
    std::vector<SQLLEN> indicators(4, -7);
    std::vector<SQLUSMALLINT> status(3, 0xFFFF);
    SQLULEN rowsFetched = 999;
    SQLLEN offset = sizeof(SQLLEN);

    ASSERT_SQL_OK(SQLSetDescField(ard, 0, SQL_DESC_ARRAY_SIZE,
                                  reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_DESC, ard);
    ASSERT_SQL_OK(SQLSetDescField(ard, 0, SQL_DESC_BIND_OFFSET_PTR, &offset, 0),
                  SQL_HANDLE_DESC, ard);
    ASSERT_SQL_OK(SQLSetDescField(ird, 0, SQL_DESC_ROWS_PROCESSED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_DESC, ird);
    ASSERT_SQL_OK(SQLSetDescField(ird, 0, SQL_DESC_ARRAY_STATUS_PTR, status.data(), 0),
                  SQL_HANDLE_DESC, ird);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values.data(), sizeof(SQLINTEGER),
                             indicators.data()),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched) << "IRD SQL_DESC_ROWS_PROCESSED_PTR received the count";
    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(SQL_ROW_SUCCESS, status[i]) << "IRD status array, row " << i;
    }
    EXPECT_EQ(-1, values[0]) << "ARD SQL_DESC_BIND_OFFSET_PTR displaced the rowset";
    EXPECT_EQ(-1, values[1]);
    EXPECT_EQ(1, values[2]);
    EXPECT_EQ(2, values[3]);
    EXPECT_EQ(3, values[4]);
    EXPECT_EQ(-7, indicators[0]) << "the indicator slack is untouched";
    SQLCloseCursor(stmt_);
}

// AB#49060: each row-side attribute and its descriptor field are one value.
TEST_F(FetchScrollLiveTest, RowAttributesAndDescriptorFieldsAgree) {
    // Executed first for the IRD cases below: SQLGet/SetDescField on an IRD is
    // HY007 ("associated statement is not prepared") unless the statement is
    // prepared or executed. unixODBC enforces that; the Windows DM does not.
    ExecThreeRows();

    SQLHDESC ard = SQL_NULL_HDESC;
    SQLHDESC ird = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC, &ard, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_IMP_ROW_DESC, &ird, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);

    SQLLEN offset = 0;
    SQLUSMALLINT operations[2] = {};
    SQLUSMALLINT status[2] = {};
    SQLULEN fetched = 0;
    struct Case {
        SQLINTEGER attribute;
        SQLHDESC desc;
        SQLSMALLINT field;
        SQLPOINTER value;
    };
    const Case cases[] = {
        {SQL_ATTR_ROW_BIND_OFFSET_PTR, ard, SQL_DESC_BIND_OFFSET_PTR, &offset},
        {SQL_ATTR_ROW_OPERATION_PTR, ard, SQL_DESC_ARRAY_STATUS_PTR, operations},
        {SQL_ATTR_ROW_STATUS_PTR, ird, SQL_DESC_ARRAY_STATUS_PTR, status},
        {SQL_ATTR_ROWS_FETCHED_PTR, ird, SQL_DESC_ROWS_PROCESSED_PTR, &fetched},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, c.attribute, c.value, 0), SQL_HANDLE_STMT,
                      stmt_);
        SQLPOINTER read = nullptr;
        ASSERT_SQL_OK(SQLGetDescField(c.desc, 0, c.field, &read, 0, nullptr),
                      SQL_HANDLE_DESC, c.desc);
        EXPECT_EQ(c.value, read) << "attribute " << c.attribute << " -> field " << c.field;

        ASSERT_SQL_OK(SQLSetDescField(c.desc, 0, c.field, nullptr, 0), SQL_HANDLE_DESC,
                      c.desc);
        read = &offset;
        ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, c.attribute, &read, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(nullptr, read) << "field " << c.field << " -> attribute " << c.attribute;
    }

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_BIND_TYPE,
                                 reinterpret_cast<SQLPOINTER>(24), 0),
                  SQL_HANDLE_STMT, stmt_);
    SQLINTEGER bindType = 0;
    ASSERT_SQL_OK(SQLGetDescField(ard, 0, SQL_DESC_BIND_TYPE, &bindType, 0, nullptr),
                  SQL_HANDLE_DESC, ard);
    EXPECT_EQ(24, bindType);
    ASSERT_SQL_OK(SQLSetDescField(ard, 0, SQL_DESC_BIND_TYPE,
                                  reinterpret_cast<SQLPOINTER>(SQL_BIND_BY_COLUMN), 0),
                  SQL_HANDLE_DESC, ard);
    SQLULEN reported = 99;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_ROW_BIND_TYPE, &reported, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(static_cast<SQLULEN>(SQL_BIND_BY_COLUMN), reported);
    SQLCloseCursor(stmt_);
}

// AB#49060: SQL_ROWSET_SIZE lives on the ARD (msodbcsql `ADTag::dwRowSetSize`),
// so it follows an ARD association.
TEST_F(FetchScrollLiveTest, RowsetSizeFollowsTheAssociatedArd) {
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ROWSET_SIZE, reinterpret_cast<SQLPOINTER>(5), 0),
                  SQL_HANDLE_STMT, stmt_);
    SQLHDESC implicit_ard = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC, &implicit_ard, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);

    SQLHDESC explicit_ard = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DESC, dbc_, &explicit_ard), SQL_HANDLE_DBC,
                  dbc_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC, explicit_ard, 0),
                  SQL_HANDLE_STMT, stmt_);
    SQLULEN reported = 0;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ROWSET_SIZE, &reported, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1u, reported) << "the newly associated ARD's own default";

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC, implicit_ard, 0),
                  SQL_HANDLE_STMT, stmt_);
    reported = 0;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ROWSET_SIZE, &reported, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5u, reported) << "the implicit ARD kept its value";
    SQLFreeHandle(SQL_HANDLE_DESC, explicit_ard);
}

// AB#49060 parity audit: expectations below were measured on msodbcsql first.

namespace {
SQLHDESC StmtDesc(SQLHSTMT stmt, SQLINTEGER which) {
    SQLHDESC desc = SQL_NULL_HDESC;
    EXPECT_SQL_OK(SQLGetStmtAttr(stmt, which, &desc, 0, nullptr), SQL_HANDLE_STMT, stmt);
    return desc;
}

SQLULEN StmtULen(SQLHSTMT stmt, SQLINTEGER attribute) {
    SQLULEN value = 0xDEADBEEF;
    EXPECT_SQL_OK(SQLGetStmtAttr(stmt, attribute, &value, 0, nullptr), SQL_HANDLE_STMT,
                  stmt);
    return value;
}
} // namespace

// A row that fails conversion still counts as fetched (msodbcsql FetchRows).
TEST_F(FetchScrollLiveTest, RowsFetchedCountsAnErrorRowInTheRowset) {
    ExecDirect("SELECT CAST(1 AS INT) AS c UNION ALL SELECT 300 UNION ALL SELECT 2 "
               "ORDER BY c");
    SQLHDESC ird = StmtDesc(stmt_, SQL_ATTR_IMP_ROW_DESC);
    signed char v[3] = {42, 42, 42};
    SQLLEN ind[3] = {0, 0, 0};
    SQLUSMALLINT status[3] = {0xFFFF, 0xFFFF, 0xFFFF};
    SQLULEN fetched = 99;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetDescField(ird, 0, SQL_DESC_ROWS_PROCESSED_PTR, &fetched, 0),
                  SQL_HANDLE_DESC, ird);
    ASSERT_SQL_OK(SQLSetDescField(ird, 0, SQL_DESC_ARRAY_STATUS_PTR, status, 0),
                  SQL_HANDLE_DESC, ird);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_STINYINT, v, sizeof(signed char), ind),
                  SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
    EXPECT_EQ(3u, fetched) << "the error row is still a fetched row";
    EXPECT_EQ(SQL_ROW_SUCCESS, status[0]);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[1]);
    EXPECT_EQ(SQL_ROW_ERROR, status[2]) << "300 does not fit a signed byte";
    SQLCloseCursor(stmt_);
}

// A truncated row: msodbcsql reports SQL_ROW_ERROR through SQLFetch and
// SQL_ROW_SUCCESS_WITH_INFO through SQLFetchScroll; mssql-odbc reports the
// latter through both (see `RowOutcome::status`). Each leg asserts its own.
TEST_F(FetchScrollLiveTest, TruncatedRowStatusThroughSQLFetchAndSQLFetchScroll) {
    const char* target = std::getenv("ODBC_TEST_TARGET");
    const bool reference = target && std::string(target) == "msodbcsql";
    for (bool scroll : {false, true}) {
        SCOPED_TRACE(scroll ? "SQLFetchScroll" : "SQLFetch");
        ExecDirect("SELECT 'abcdef' AS s UNION ALL SELECT 'gh' ORDER BY s");
        char text[2][4] = {};
        SQLLEN len[2] = {0, 0};
        SQLUSMALLINT status[2] = {0xFFFF, 0xFFFF};
        SQLULEN fetched = 99;
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                     reinterpret_cast<SQLPOINTER>(2), 0),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, text, sizeof(text[0]), len),
                      SQL_HANDLE_STMT, stmt_);

        const SQLRETURN rc = scroll ? SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0)
                                    : SQLFetch(stmt_);
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
        EXPECT_EQ(2u, fetched);
        const SQLUSMALLINT truncated =
            (reference && !scroll) ? SQL_ROW_ERROR : SQL_ROW_SUCCESS_WITH_INFO;
        EXPECT_EQ(truncated, status[0]) << "'abcdef' truncated to 3 bytes";
        EXPECT_EQ(SQL_ROW_SUCCESS, status[1]);
        EXPECT_STREQ("abc", text[0]);
        EXPECT_EQ(6, len[0]) << "the full length is still reported";
        SQLFreeStmt(stmt_, SQL_UNBIND);
        SQLCloseCursor(stmt_);
    }
}

// A fetch refused by validation leaves the rows-fetched buffer alone.
TEST_F(FetchScrollLiveTest, ARefusedFetchLeavesRowsFetchedAlone) {
    ExecThreeRows();
    SQLULEN fetched = 77;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLFetchScroll(stmt_, SQL_FETCH_PRIOR, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY106");
    EXPECT_EQ(77u, fetched);
    SQLCloseCursor(stmt_);
}

// The bind offset is read per fetch, including after repointing it via the ARD.
TEST_F(FetchScrollLiveTest, TheRowBindOffsetIsReadOnEveryFetch) {
    ExecThreeRows();
    SQLHDESC ard = StmtDesc(stmt_, SQL_ATTR_APP_ROW_DESC);
    SQLINTEGER values[3] = {-1, -1, -1};
    SQLLEN ind[3] = {0, 0, 0};
    SQLLEN offset = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_BIND_OFFSET_PTR, &offset, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    offset = sizeof(SQLINTEGER);  // the indicator moves by the same 4 bytes
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLLEN repointed = 2 * sizeof(SQLINTEGER);
    ASSERT_SQL_OK(SQLSetDescField(ard, 0, SQL_DESC_BIND_OFFSET_PTR, &repointed, 0),
                  SQL_HANDLE_DESC, ard);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(3, values[2]);
    SQLCloseCursor(stmt_);
}

// The array size and rows-fetched pointer can change between fetches.
TEST_F(FetchScrollLiveTest, RowsetControlsCanChangeBetweenFetches) {
    ExecDirect("SELECT n FROM (VALUES (1), (2), (3), (4), (5), (6)) AS t(n) ORDER BY n");
    SQLHDESC ard = StmtDesc(stmt_, SQL_ATTR_APP_ROW_DESC);
    SQLHDESC ird = StmtDesc(stmt_, SQL_ATTR_IMP_ROW_DESC);
    SQLINTEGER values[3] = {};
    SQLLEN ind[3] = {};
    SQLULEN fetched = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2u, fetched);
    EXPECT_EQ(2, values[1]);

    ASSERT_SQL_OK(SQLSetDescField(ard, 0, SQL_DESC_ARRAY_SIZE,
                                  reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_DESC, ard);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, fetched);
    EXPECT_EQ(3, values[0]);
    EXPECT_EQ(5, values[2]);

    ASSERT_SQL_OK(SQLSetDescField(ird, 0, SQL_DESC_ROWS_PROCESSED_PTR, nullptr, 0),
                  SQL_HANDLE_DESC, ird);
    fetched = 77;
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(77u, fetched) << "a cleared pointer is no longer written";
    EXPECT_EQ(6, values[0]);
    SQLCloseCursor(stmt_);
}

// One explicit descriptor as one statement's ARD and another's APD has one
// header (from msodbcsql's DescFpp suite).
TEST_F(FetchScrollLiveTest, OneDescriptorAsArdAndApdSharesEveryHeaderAlias) {
    SQLHSTMT other = AllocStmt();
    SQLHDESC shared = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DESC, dbc_, &shared), SQL_HANDLE_DBC, dbc_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC, shared, 0), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(other, SQL_ATTR_APP_PARAM_DESC, shared, 0),
                  SQL_HANDLE_STMT, other);

    SQLLEN offset = 0;
    SQLUSMALLINT operations[2] = {};
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(7), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_BIND_OFFSET_PTR, &offset, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(other, SQL_ATTR_PARAM_BIND_TYPE,
                                 reinterpret_cast<SQLPOINTER>(16), 0),
                  SQL_HANDLE_STMT, other);
    ASSERT_SQL_OK(SQLSetStmtAttr(other, SQL_ATTR_PARAM_OPERATION_PTR, operations, 0),
                  SQL_HANDLE_STMT, other);

    EXPECT_EQ(7u, StmtULen(other, SQL_ATTR_PARAMSET_SIZE));
    EXPECT_EQ(reinterpret_cast<SQLULEN>(&offset),
              StmtULen(other, SQL_ATTR_PARAM_BIND_OFFSET_PTR));
    EXPECT_EQ(16u, StmtULen(stmt_, SQL_ATTR_ROW_BIND_TYPE));
    EXPECT_EQ(reinterpret_cast<SQLULEN>(operations),
              StmtULen(stmt_, SQL_ATTR_ROW_OPERATION_PTR));

    FreeStmt(other);
    SQLFreeHandle(SQL_HANDLE_DESC, shared);
}

// Freeing an associated ARD reverts the row aliases to the implicit ARD's.
TEST_F(FetchScrollLiveTest, FreeingTheAssociatedArdRevertsTheRowAliases) {
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(4), 0),
                  SQL_HANDLE_STMT, stmt_);
    SQLHDESC implicit_ard = StmtDesc(stmt_, SQL_ATTR_APP_ROW_DESC);
    SQLHDESC explicit_ard = SQL_NULL_HDESC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DESC, dbc_, &explicit_ard), SQL_HANDLE_DBC,
                  dbc_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC, explicit_ard, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(9), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_BIND_TYPE,
                                 reinterpret_cast<SQLPOINTER>(24), 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(9u, StmtULen(stmt_, SQL_ATTR_ROW_ARRAY_SIZE));

    ASSERT_SQL_OK(SQLFreeHandle(SQL_HANDLE_DESC, explicit_ard), SQL_HANDLE_DESC,
                  explicit_ard);
    EXPECT_EQ(implicit_ard, StmtDesc(stmt_, SQL_ATTR_APP_ROW_DESC));
    EXPECT_EQ(4u, StmtULen(stmt_, SQL_ATTR_ROW_ARRAY_SIZE));
    EXPECT_EQ(static_cast<SQLULEN>(SQL_BIND_BY_COLUMN),
              StmtULen(stmt_, SQL_ATTR_ROW_BIND_TYPE));
}

// Both spellings store a bind type wider than SQLINTEGER; SQLGetDescField
// reports the low 32 bits.
#if defined(_WIN64) || defined(__LP64__)
TEST_F(FetchScrollLiveTest, AWideBindTypeIsStoredWholeThroughEitherSpelling) {
    SQLHDESC ard = StmtDesc(stmt_, SQL_ATTR_APP_ROW_DESC);
    const SQLULEN wide = (static_cast<SQLULEN>(1) << 32) | 0x18;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_BIND_TYPE,
                                 reinterpret_cast<SQLPOINTER>(wide), 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(wide, StmtULen(stmt_, SQL_ATTR_ROW_BIND_TYPE));
    SQLINTEGER narrow = -1;
    ASSERT_SQL_OK(SQLGetDescField(ard, 0, SQL_DESC_BIND_TYPE, &narrow, 0, nullptr),
                  SQL_HANDLE_DESC, ard);
    EXPECT_EQ(0x18, narrow);

    const SQLULEN over_int32 = static_cast<SQLULEN>(0x80000000u);
    EXPECT_EQ(SQL_SUCCESS, SQLSetDescField(ard, 0, SQL_DESC_BIND_TYPE,
                                           reinterpret_cast<SQLPOINTER>(over_int32), 0))
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_DESC, ard);
    EXPECT_EQ(over_int32, StmtULen(stmt_, SQL_ATTR_ROW_BIND_TYPE));
}
#endif

TEST_F(FetchScrollLiveTest, ReportsRowsFetched) {
    ExecThreeRows();
    SQLULEN rowsFetched = 999;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    for (int i = 0; i < 3; ++i) {
        rowsFetched = 999;
        ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(1u, rowsFetched) << "row " << i;
    }
    rowsFetched = 999;
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(0u, rowsFetched) << "the end-of-set call still reports a count";
    SQLCloseCursor(stmt_);
}

// The rows the fetch did not fill have to be marked, or the application reads
// stale statuses left by a previous, longer rowset.
TEST_F(FetchScrollLiveTest, FillsTheRowStatusArray) {
    ExecThreeRows();
    std::vector<SQLUSMALLINT> status(4, 0xFFFF);
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(4), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status.data(), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[0]);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[1]);
    EXPECT_EQ(SQL_ROW_SUCCESS, status[2]);
    EXPECT_EQ(SQL_ROW_NOROW, status[3]);
    SQLCloseCursor(stmt_);
}

// A rowset wider than what is left returns the partial block, and the call
// after it reports end of set.
TEST_F(FetchScrollLiveTest, PartialRowsetAtEndOfResultSet) {
    ExecThreeRows();
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2u, rowsFetched);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1u, rowsFetched) << "the trailing partial rowset";

    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(0u, rowsFetched);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ReturnsNoDataAtEndOfResultSet) {
    ExecDirect("SELECT 1 AS n WHERE 1 = 0");
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// Bound-column fetch. These are the cases that exercise the rowset fill loop;
// until SQLBindCol existed there was no way to reach it from here.
// ---------------------------------------------------------------------------

// One bound int column over a rowset wider than one row: each row must land at
// its own offset in the array, with its own indicator.
TEST_F(FetchScrollLiveTest, BindsAnIntegerColumnAcrossARowset) {
    ExecThreeRows();
    SQLINTEGER values[4] = {-1, -1, -1, -1};
    SQLLEN indicators[4] = {-99, -99, -99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(4), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), indicators),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(3, values[2]);
    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicators[i]) << "row " << i;
    }
    SQLCloseCursor(stmt_);
}

// SQL_C_DEFAULT is retained by SQLBindCol and resolved from each result
// column's IRD type when the rowset is fetched. This covers both the fixed
// stride of SQL_C_SLONG and the application-sized stride of SQL_C_CHAR.
TEST_F(FetchScrollLiveTest, DefaultTargetResolvesAtFetchTime) {
    SQLINTEGER values[3] = {-1, -1, -1};
    SQLCHAR text[3][8] = {};
    SQLLEN valueIndicators[3] = {-99, -99, -99};
    SQLLEN textIndicators[3] = {-99, -99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_DEFAULT, values, sizeof(SQLINTEGER),
                            valueIndicators),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_DEFAULT, text, sizeof(text[0]), textIndicators),
                  SQL_HANDLE_STMT, stmt_);

    ExecDirect(
        "SELECT n, s FROM (VALUES (1, CAST('one' AS VARCHAR(8))), "
        "(2, 'two'), (3, 'three')) AS t(n, s) ORDER BY n");
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(3, values[2]);
    EXPECT_STREQ("one", reinterpret_cast<const char*>(text[0]));
    EXPECT_STREQ("two", reinterpret_cast<const char*>(text[1]));
    EXPECT_STREQ("three", reinterpret_cast<const char*>(text[2]));
    for (int i = 0; i < 3; ++i) {
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), valueIndicators[i]);
    }
    EXPECT_EQ(3, textIndicators[0]);
    EXPECT_EQ(3, textIndicators[1]);
    EXPECT_EQ(5, textIndicators[2]);
    SQLCloseCursor(stmt_);
}

// The two deliberate deviations from msodbcsql's Sql2CDefault, which the fetch
// path inherits from the resolver it shares with SQLBindParameter: an NVARCHAR
// column resolves to SQL_C_WCHAR and a uniqueidentifier to SQL_C_GUID, where
// msodbcsql resolves both to its ANSI SQL_C_CHAR. The GUID case also pins the
// resulting rowset layout, because a fixed-width target strides by its C type
// rather than by BufferLength. See mssql-odbc/docs/typed-columnar-fetch-plan.md,
// which records the measured msodbcsql values these assertions diverge from.
//
// Skipped on the reference leg by construction: asserting a deviation is the
// point, so comparing it would always report a divergence.
TEST_F(FetchScrollLiveTest, DefaultTargetResolvesWideAndGuidToTypedTargets) {
    SKIP_IF_COMPARING_MSODBCSQL();
    SQLWCHAR wide[2][8] = {};
    // Four slots for a rowset of two, with BufferLength deliberately set to two
    // SQLGUIDs. A BufferLength-driven stride would land row 1 in guids[2]; the
    // C-type stride lands it in guids[1]. Both stay inside the array, so the
    // wrong layout fails an assertion instead of corrupting the stack.
    SQLGUID guids[4] = {};
    SQLLEN wideIndicators[2] = {-99, -99};
    SQLLEN guidIndicators[2] = {-99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_DEFAULT, wide, sizeof(wide[0]), wideIndicators),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_DEFAULT, guids,
                             static_cast<SQLLEN>(2 * sizeof(SQLGUID)), guidIndicators),
                  SQL_HANDLE_STMT, stmt_);

    ExecDirect(
        "SELECT w, g FROM (VALUES "
        "(1, CAST(N'one' AS NVARCHAR(8)), "
        "CAST('01020304-0506-0708-090A-0B0C0D0E0F10' AS UNIQUEIDENTIFIER)), "
        "(2, N'two', CAST('11121314-1516-1718-191A-1B1C1D1E1F20' AS UNIQUEIDENTIFIER))"
        ") AS t(n, w, g) ORDER BY n");
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2u, rowsFetched);

    const SQLWCHAR one[] = {'o', 'n', 'e', 0};
    const SQLWCHAR two[] = {'t', 'w', 'o', 0};
    for (int i = 0; i < 4; ++i) {
        EXPECT_EQ(one[i], wide[0][i]) << "row 0 unit " << i;
        EXPECT_EQ(two[i], wide[1][i]) << "row 1 unit " << i;
    }
    // Bytes of UTF-16, which is what makes the wide resolution observable: the
    // narrow default would report 3.
    EXPECT_EQ(static_cast<SQLLEN>(3 * sizeof(SQLWCHAR)), wideIndicators[0]);
    EXPECT_EQ(static_cast<SQLLEN>(3 * sizeof(SQLWCHAR)), wideIndicators[1]);

    EXPECT_EQ(0x01020304u, guids[0].Data1);
    EXPECT_EQ(0x0506u, guids[0].Data2);
    EXPECT_EQ(0x0708u, guids[0].Data3);
    const unsigned char firstTail[8] = {0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10};
    EXPECT_EQ(0, std::memcmp(guids[0].Data4, firstTail, sizeof(firstTail)));
    EXPECT_EQ(0x11121314u, guids[1].Data1);
    EXPECT_EQ(0x1516u, guids[1].Data2);
    EXPECT_EQ(0x1718u, guids[1].Data3);
    const unsigned char secondTail[8] = {0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20};
    EXPECT_EQ(0, std::memcmp(guids[1].Data4, secondTail, sizeof(secondTail)));
    // Nothing beyond the rowset was written, which is what rules out a
    // BufferLength-driven stride.
    const SQLGUID untouched{};
    EXPECT_EQ(0, std::memcmp(&guids[2], &untouched, sizeof(SQLGUID)));
    // sizeof(SQLGUID), not the 36 characters msodbcsql's SQL_C_CHAR would give.
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLGUID)), guidIndicators[0]);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLGUID)), guidIndicators[1]);
    SQLCloseCursor(stmt_);
}

// The cross-result-set half of the deferred-resolution contract: one binding,
// two result sets whose column 1 has a different SQL type in each. The binding
// must resolve independently per set — SQL_C_SLONG for the int, SQL_C_CHAR for
// the varchar — which only works because the stored binding keeps the
// SQL_C_DEFAULT placeholder rather than being rewritten by the first fetch.
//
// Deliberately uses the two types both drivers resolve identically (int and
// narrow varchar), so this runs on the msodbcsql leg too and compares.
TEST_F(FetchScrollLiveTest, DefaultTargetResolvesPerResultSet) {
    union {
        SQLINTEGER n;
        SQLCHAR text[16];
    } slot = {};
    SQLLEN indicator = -99;

    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_DEFAULT, &slot, sizeof(slot), &indicator),
                  SQL_HANDLE_STMT, stmt_);

    ExecDirect("SELECT CAST(4242 AS INT) AS c1; SELECT CAST('hello' AS VARCHAR(16)) AS c1");

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(4242, slot.n) << "first set must resolve to SQL_C_SLONG";
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicator);

    ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("hello", reinterpret_cast<const char*>(slot.text))
        << "second set must resolve the same binding to SQL_C_CHAR";
    EXPECT_EQ(5, indicator);
    SQLCloseCursor(stmt_);
}

// Two columns of different shapes bound at once, to prove the fill loop walks
// the binding table rather than assuming a single column.
TEST_F(FetchScrollLiveTest, BindsSeveralColumnsOfDifferentTypes) {
    ExecDirect(
        "SELECT 10 AS n, CAST('alpha' AS VARCHAR(20)) AS s"
        " UNION ALL SELECT 20, 'beta' ORDER BY n");
    SQLINTEGER nums[2] = {-1, -1};
    SQLCHAR text[2][32] = {};
    SQLLEN numInd[2] = {-99, -99};
    SQLLEN textInd[2] = {-99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    // Bound out of order on purpose: the fill loop has to visit them ascending.
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_CHAR, text, sizeof(text[0]), textInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, nums, sizeof(SQLINTEGER), numInd),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2u, rowsFetched);
    EXPECT_EQ(10, nums[0]);
    EXPECT_EQ(20, nums[1]);
    EXPECT_STREQ("alpha", reinterpret_cast<const char*>(text[0]));
    EXPECT_STREQ("beta", reinterpret_cast<const char*>(text[1]));
    EXPECT_EQ(5, textInd[0]);
    EXPECT_EQ(4, textInd[1]);
    SQLCloseCursor(stmt_);
}

// NULL is reported through the indicator, and must not disturb the data slot of
// a fixed-width target.
TEST_F(FetchScrollLiveTest, BoundNullIsReportedThroughTheIndicator) {
    // Ordered explicitly so the NULL's position is not left to the plan.
    ExecDirect(
        "SELECT n FROM (VALUES (1, 1), (2, NULL), (3, 3)) AS t(ord, n) ORDER BY ord");
    SQLINTEGER values[3] = {7, 7, 7};
    SQLLEN indicators[3] = {-99, -99, -99};
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), indicators),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicators[0]);
    EXPECT_EQ(SQL_NULL_DATA, indicators[1]);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicators[2]);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(7, values[1]) << "a NULL must not disturb its data slot";
    EXPECT_EQ(3, values[2]);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollUtf16Test, BoundNullPreservesSeparateOctetLength) {
    for (const char* type : {"BINARY(3)", "CHAR(8)", "VARBINARY(8)",
                             "VARBINARY(MAX)", "VARCHAR(8)", "NVARCHAR(MAX)"}) {
        SCOPED_TRACE(type);
        for (SQLSMALLINT target : {SQL_C_CHAR, SQL_C_WCHAR}) {
            SCOPED_TRACE(target);
            for (SQLLEN capacity : {0, 1, 2, 3, 32}) {
                SCOPED_TRACE(capacity);
                std::vector<unsigned char> buffer(32, 0x7E);
                SQLLEN indicator = -99;
                SQLLEN octet_length = -98;
                ASSERT_EQ(SQL_SUCCESS, SQLBindCol(stmt_, 1, target, buffer.data(),
                                                 capacity, &indicator));
                SQLHDESC ard = SQL_NULL_HDESC;
                ASSERT_EQ(SQL_SUCCESS, SQLGetStmtAttr(stmt_, SQL_ATTR_APP_ROW_DESC,
                                                     &ard, 0, nullptr));
                ASSERT_EQ(SQL_SUCCESS, SQLSetDescField(
                    ard, 1, SQL_DESC_INDICATOR_PTR, &indicator, 0));
                ASSERT_EQ(SQL_SUCCESS, SQLSetDescField(
                    ard, 1, SQL_DESC_OCTET_LENGTH_PTR, &octet_length, 0));
                ExecDirect("SELECT CAST(NULL AS " + std::string(type) + ")");
                ASSERT_EQ(SQL_SUCCESS, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
                EXPECT_EQ(SQL_NULL_DATA, indicator);
                EXPECT_EQ(-98, octet_length);
                EXPECT_EQ(std::vector<unsigned char>(buffer.size(), 0x7E), buffer);
                EXPECT_EQ("", StmtDiagState());
                ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
                ASSERT_EQ(SQL_SUCCESS, SQLFreeStmt(stmt_, SQL_UNBIND));
            }
        }
    }
}

// A bound column gets one shot at a fixed buffer, so an over-long value is
// truncated with 01004 and the indicator reports the untruncated length.
TEST_F(FetchScrollLiveTest, BoundCharacterDataTruncatesWithInfo) {
    ExecDirect("SELECT CAST('abcdefghij' AS VARCHAR(20)) AS s");
    SQLCHAR text[5] = {};
    SQLLEN indicator = -99;
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, text, sizeof(text), &indicator),
                  SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(1u, rowsFetched);
    EXPECT_STREQ("abcd", reinterpret_cast<const char*>(text));
    EXPECT_EQ(10, indicator) << "the indicator reports the untruncated length";
    SQLCloseCursor(stmt_);
}

// SQLFreeStmt(SQL_UNBIND) drops every binding; mssql-python calls it before
// each fetch, so a fetch afterwards must deliver nothing.
TEST_F(FetchScrollLiveTest, UnbindStopsDelivery) {
    ExecThreeRows();
    SQLINTEGER value = -1;
    SQLLEN indicator = -99;
    SQLULEN rowsFetched = 0;

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, value);

    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);
    value = -1;
    indicator = -99;
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1u, rowsFetched) << "the row is still fetched, just not delivered";
    EXPECT_EQ(-1, value) << "an unbound column must not be written";
    EXPECT_EQ(-99, indicator);
    SQLCloseCursor(stmt_);
}

// Rebinding a column replaces its entry rather than adding a second one, so the
// value lands in the new buffer only.
TEST_F(FetchScrollLiveTest, RebindingAColumnReplacesTheBinding) {
    ExecThreeRows();
    SQLINTEGER first = -1;
    SQLINTEGER second = -1;
    SQLLEN indicator = -99;

    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &first, sizeof(first), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &second, sizeof(second), &indicator),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, first) << "the replaced binding must not be written";
    EXPECT_EQ(1, second);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicator);
    SQLCloseCursor(stmt_);
}

// Unbinding one column leaves the others delivering.
TEST_F(FetchScrollLiveTest, UnbindingOneColumnLeavesTheOthers) {
    ExecDirect("SELECT 10 AS a, 20 AS b");
    SQLINTEGER a = -1;
    SQLINTEGER b = -1;

    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &a, sizeof(a), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &b, sizeof(b), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    // Both null unbinds column 1.
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, nullptr, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, a) << "column 1 was unbound";
    EXPECT_EQ(20, b);
    SQLCloseCursor(stmt_);
}

// A bound fetch of a single row leaves the cursor positioned, so SQLGetData can
// still read a column the fill loop did not take.
TEST_F(FetchScrollLiveTest, GetDataStillWorksAfterASingleRowBoundFetch) {
    ExecDirect("SELECT 10 AS a, 20 AS b");
    SQLINTEGER a = -1;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &a, sizeof(a), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(10, a);

    SQLINTEGER b = -1;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &b, sizeof(b), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(20, b);
    SQLCloseCursor(stmt_);
}

// ODBC defines SQLFetch as the SQL_FETCH_NEXT form of SQLFetchScroll, so the
// classic SQLBindCol + SQLFetch loop must fill the bound buffers too. Keeping a
// second row-reading path is how that silently stops being true.
TEST_F(FetchScrollLiveTest, SQLFetchFillsBoundColumnsToo) {
    ExecThreeRows();
    SQLINTEGER value = -1;
    SQLLEN indicator = -99;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator),
                  SQL_HANDLE_STMT, stmt_);

    for (int expected = 1; expected <= 3; ++expected) {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, value);
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), indicator);
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// SQL_ATTR_ROW_ARRAY_SIZE applies to SQLFetch as well, so it returns a rowset
// rather than a single row.
TEST_F(FetchScrollLiveTest, SQLFetchHonoursTheRowsetSize) {
    ExecThreeRows();
    SQLINTEGER values[3] = {-1, -1, -1};
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), nullptr),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(1, values[0]);
    EXPECT_EQ(2, values[1]);
    EXPECT_EQ(3, values[2]);
    SQLCloseCursor(stmt_);
}

// A bound LOB column is delivered into the fixed buffer (AB#47361), and its
// bytes have to leave the wire either way. Abandoning the PLP stream mid value
// left the row cursor inside the LOB, so the *next* column parsed payload bytes
// as a length prefix -- which segfaulted the driver rather than failing
// cleanly. The second bound column is the part that matters here.
TEST_F(FetchScrollLiveTest, ABoundLobColumnDoesNotDesyncTheRow) {
    ExecDirect(
        "SELECT REPLICATE(CAST('x' AS NVARCHAR(MAX)), 10000) AS lob, 4242 AS n");

    SQLWCHAR lob[64] = {0};
    SQLLEN lobInd = 0;
    SQLINTEGER n = -1;
    SQLLEN nInd = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, lob, sizeof(lob), &lobInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &n, sizeof(n), &nInd),
                  SQL_HANDLE_STMT, stmt_);

    // The LOB truncates into the buffer with 01004, and the column after it
    // still arrives -- the desync this guards against would corrupt that one.
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_EQ(4242, n) << "the column after the LOB must still decode";

    // The row stream has to be intact afterwards: a clean single-row result set
    // ends here rather than returning garbage or faulting.
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// The same hazard through the block path, and with the LOB last so the drain
// has to happen even when no bound column follows it.
TEST_F(FetchScrollLiveTest, ABoundLobIsDrainedAcrossARowset) {
    ExecDirect(
        "SELECT n, REPLICATE(CAST('y' AS NVARCHAR(MAX)), 5000) AS lob "
        "FROM (VALUES (1),(2),(3)) AS t(n) ORDER BY n");

    SQLINTEGER ns[3] = {-1, -1, -1};
    SQLWCHAR lobs[3][32] = {};
    SQLLEN nInd[3] = {0, 0, 0};
    SQLLEN lobInd[3] = {0, 0, 0};
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, ns, sizeof(SQLINTEGER), nInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_WCHAR, lobs, sizeof(lobs[0]), lobInd),
                  SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0);
    EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO || rc == SQL_ERROR)
        << "unexpected rc " << rc;
    // Every row has to have been walked past, whatever happened to the LOBs.
    EXPECT_EQ(3u, rowsFetched);
    EXPECT_EQ(SQL_NO_DATA, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    SQLCloseCursor(stmt_);
}

// Mixed access after a *block* fetch. ODBC expects SQLSetPos to nominate a row
// first, which is not implemented, so the cursor is deliberately left
// unpositioned. The contract that matters is that SQLGetData then fails
// cleanly: an unpositioned cursor must not be read as though a row were there.
TEST_F(FetchScrollLiveTest, GetDataAfterABlockFetchFailsCleanly) {
    ExecThreeRows();
    SQLINTEGER values[3] = {-1, -1, -1};
    SQLULEN rowsFetched = 0;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(3), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &rowsFetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(SQLINTEGER), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(3u, rowsFetched);

    // Must return an error rather than crashing or inventing a value.
    SQLINTEGER scratch = -1;
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_SLONG, &scratch, sizeof(scratch), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    SQLCloseCursor(stmt_);
}

// A null TargetValuePtr unbinds the column outright; the indicator is never
// consulted. The column must therefore stay available to SQLGetData, which is
// what distinguishes an unbind from a binding that delivers nothing.
TEST_F(FetchScrollLiveTest, ANullTargetPointerUnbindsAndLeavesTheColumnReadable) {
    ExecDirect("SELECT CAST(42 AS INT) AS n, CAST('hi' AS VARCHAR(10)) AS s");
    SQLINTEGER v = -1;
    SQLLEN ind = -999;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), &ind),
                  SQL_HANDLE_STMT, stmt_);
    // Rebinding with a null data pointer unbinds, even though the indicator is live.
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, nullptr, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-1, v) << "an unbound column is not delivered";
    EXPECT_EQ(static_cast<SQLLEN>(-999), ind);

    // Column 1 must not have been consumed by a lingering binding.
    SQLINTEGER viaGetData = -1;
    SQLLEN gdInd = 0;
    EXPECT_TRUE(SQL_SUCCEEDED(
        SQLGetData(stmt_, 1, SQL_C_SLONG, &viaGetData, sizeof(viaGetData), &gdInd)));
    EXPECT_EQ(42, viaGetData);
    SQLCloseCursor(stmt_);
}

// msodbcsql keys the fetch return code on the rowset size: a single-row fetch
// lets a row error stand as SQL_ERROR, while a block fetch demotes it to
// SQL_SUCCESS_WITH_INFO and leaves the detail in the row status array.
TEST_F(FetchScrollLiveTest, ARowErrorIsSQL_ERRORAtRowsetSizeOne) {
    ExecDirect("SELECT CAST(NULL AS INT) AS z");
    SQLINTEGER v = -1;
    // NULL with no indicator to report it through is a row error.
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// The same row error inside a rowset is demoted, so the application reads the
// per-row detail from the status array instead.
TEST_F(FetchScrollLiveTest, TheSameRowErrorIsDemotedInABlockFetch) {
    ExecDirect("SELECT CAST(NULL AS INT) AS z");
    SQLINTEGER v[2] = {-1, -1};
    SQLUSMALLINT status[2] = {0, 0};
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                                 reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, v, sizeof(SQLINTEGER), nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_EQ(SQL_ROW_ERROR, status[0]);
    SQLCloseCursor(stmt_);
}

// A binding left over from a wider result set is not an error: msodbcsql
// skips it and reports nothing at all -- no diagnostic, plain SQL_SUCCESS.
// Neither 07009 ("invalid descriptor index", which is what bind time uses for
// an out-of-range ordinal) nor 07006 is raised, so a stale binding must not
// start failing fetches here.
TEST_F(FetchScrollLiveTest, AStaleOrdinalPastTheResultSetIsIgnored) {
    ExecDirect("SELECT 1 AS only_column");
    SQLINTEGER v = -1;
    SQLLEN ind = -999;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 5, SQL_C_SLONG, &v, sizeof(v), &ind),
                  SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    // No diagnostic, and the untouched binding stays untouched.
    SQLWCHAR state[6] = {0};
    SQLINTEGER native = 0;
    SQLWCHAR msg[256] = {0};
    SQLSMALLINT msgLen = 0;
    EXPECT_EQ(SQL_NO_DATA, SQLGetDiagRecW(SQL_HANDLE_STMT, stmt_, 1, state,
                                          &native, msg, 256, &msgLen));
    EXPECT_EQ(static_cast<SQLLEN>(-999), ind);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// Bound PLP (max/LOB) delivery — AB#47361.
//
// The indicator rule is the subtle part, and each case below was verified
// against msodbcsql before being asserted: a value that fits reports exactly
// what was produced; a truncated one reports the full length when the target's
// units match the wire's, and SQL_NO_TOTAL when transcoding makes the wire byte
// count the wrong unit to report.
// ---------------------------------------------------------------------------

TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxThatFitsReportsItsLength) {
    ExecDirect("SELECT CAST(N'abcdefghij' AS NVARCHAR(MAX)) AS c1");

    char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_STREQ("abcdefghij", buf);
    EXPECT_EQ(10, ind);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Transcoding UTF-16 to a narrow target means the wire byte count is not the
// delivered byte count, so the full length cannot be reported.
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxTruncatedToCharReportsNoTotal) {
    ExecDirect("SELECT REPLICATE(CAST(N'x' AS NVARCHAR(MAX)), 5000) AS c1");

    char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(SQL_NO_TOTAL, ind);
    EXPECT_EQ(31u, std::strlen(buf)) << "filled to capacity, less the terminator";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Same source into a wide target: the units match the wire, so the full length
// is knowable and is reported.
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxTruncatedToWcharReportsFullLength) {
    ExecDirect("SELECT REPLICATE(CAST(N'x' AS NVARCHAR(MAX)), 5000) AS c1");

    SQLWCHAR buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(10000, ind) << "5000 characters, two bytes each";

    // The indicator alone would pass while the buffer held nothing, so check
    // what actually landed.
    int units = 0;
    while (units < 16 && buf[units] != 0) {
        ++units;
    }
    EXPECT_EQ(15, units) << "filled to capacity, less the terminator";
    EXPECT_EQ(u'x', buf[0]);
    EXPECT_EQ(u'x', buf[14]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundVarcharMaxTruncatedReportsFullLength) {
    ExecDirect("SELECT REPLICATE(CAST('y' AS VARCHAR(MAX)), 5000) AS c1");

    char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(5000, ind) << "same encoding, so the length is knowable";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundVarcharMaxWidensToWchar) {
    ExecDirect("SELECT CAST('abcdefghij' AS VARCHAR(MAX)) AS c1");

    SQLWCHAR buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(20, ind) << "ten UTF-16 code units";
    EXPECT_EQ(u'a', buf[0]);
    EXPECT_EQ(u'j', buf[9]);
    EXPECT_EQ(0, buf[10]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundVarcharMaxUsesItsCollationWhenWidening) {
    ExecDirect(
        "SELECT CAST(NCHAR(233) COLLATE Latin1_General_100_CI_AS AS VARCHAR(MAX)) AS c1");

    SQLWCHAR buf[4] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(2, ind);
    EXPECT_EQ(u'\u00e9', buf[0]);
    EXPECT_EQ(0, buf[1]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Decode through the source collation, then encode for the client (AB#47564).
TEST_F(FetchScrollLiveTest, ABoundVarcharMaxUsesItsCollationForChar) {
    ExecDirect(
        "SELECT CAST(NCHAR(233) COLLATE SQL_Latin1_General_CP1_CI_AS AS VARCHAR(MAX)) AS c1");

    SQLCHAR buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    const auto expected = ODBCTestUtils::Utf8ToNativeClient("\xC3\xA9");
    EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(buf), expected.size()));
    EXPECT_EQ(static_cast<SQLLEN>(expected.size()), ind);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// A value long enough to arrive in several PLP wire chunks, under a DBCS
// collation where each CJK character is two wire bytes. The decoder has to
// carry a character split across a wire chunk boundary; without the carry each
// half becomes U+FFFD and the slot fills with replacement characters.
//
// The token is 11 wire bytes (four GBK characters plus "abc"), deliberately
// coprime with the 8 KiB PLP_BOUND_CHUNK: an 8-byte token would put every
// power-of-two read exactly on a character boundary and the carry this test is
// named for would never be exercised.
//
// Skip is backed by a measurement, per
// .github/instructions/mssql-odbc.instructions.md. Run unskipped on build
// 173873 against the pinned retail msodbcsql leg: this driver passed and
// msodbcsql failed the value comparison. Same mechanism as the SQLGetData twin
// (VarcharMaxDbcsToCharSplitsCharacterAcrossChunks), which shows the corruption
// verbatim: msodbcsql drops a GBK lead byte at a chunk boundary and the
// following bytes decode shifted by one.
TEST_F(FetchScrollLiveTest, ABoundVarcharMaxDbcsCarriesCharactersAcrossWireChunks) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect(
        "SELECT REPLICATE(CAST(NCHAR(0x4F60) + NCHAR(0x597D) + NCHAR(0x4E16) + NCHAR(0x754C) "
        "+ N'abc' COLLATE Chinese_PRC_CI_AS AS VARCHAR(MAX)), 3000) AS c1");

    // 3000 repetitions of 11 wire bytes: 33,000 on the wire, 39,000 as UTF-8.
    std::vector<SQLCHAR> buf(64 * 1024, 0);
    SQLLEN ind = 0;
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 1, SQL_C_CHAR, buf.data(), static_cast<SQLLEN>(buf.size()), &ind),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    const std::string token = ODBCTestUtils::Utf8ToNativeClient("\xE4\xBD\xA0\xE5\xA5\xBD\xE4\xB8\x96\xE7\x95\x8C"
                                        "abc");  // 你好世界abc
    std::string expected;
    expected.reserve(token.size() * 3000);
    for (int i = 0; i < 3000; ++i) {
        expected += token;
    }
    EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(buf.data()), expected.size()));
    EXPECT_EQ(static_cast<SQLLEN>(expected.size()), ind);
    EXPECT_EQ(std::string::npos, expected.find("\xEF\xBF\xBD")) << "no U+FFFD";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundVarcharMaxDbcsTruncationCountsHeldSource) {
    if (ODBCTestUtils::Utf8ToNativeClient("\xE2\x82\xAC") != "\xE2\x82\xAC") {
        GTEST_SKIP() << "This regression pins UTF-8 client length estimates";
    }
    SQLCHAR version[32] = {};
    ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    RecordProperty("driver_version", reinterpret_cast<const char*>(version));

    // GBK prefix: C4 E3 C4 E3 C4 E3 C4 E3 A1 E8 (你你你你¤). A nine-byte
    // source read converts four 你 (12 UTF-8 bytes) and holds the lead of ¤.
    // Both slots truncate; only BufferLength=10 leaves source in the decoder
    // for decode_to_utf8_without_replacement to discard.
    // Classic InternalGetColData (sqlcdata.h, TrimPartialCodePt) reads the trail
    // before conversion. ¤ uses two bytes in both encodings, so that lookahead
    // leaves the same estimate: 214/215 on retail 18.06.0002 under C.UTF-8,
    // not the full converted length of 315.
    for (SQLLEN buffer_length : {7, 10}) {
        SCOPED_TRACE(buffer_length);
        ASSERT_NO_FATAL_FAILURE(ExecDirect(
            "SELECT CAST((REPLICATE(NCHAR(0x4F60), 4) + NCHAR(0x00A4)) "
            "COLLATE Chinese_PRC_CI_AS AS varchar(max)) + "
            "REPLICATE(CAST(NCHAR(0x4F60) COLLATE Chinese_PRC_CI_AS AS varchar(max)), 100) "
            "+ 'Z', 42"));
        std::vector<SQLCHAR> output(static_cast<size_t>(buffer_length) + 2, 0xCC);
        SQLLEN indicator = -99;
        ASSERT_EQ(SQL_SUCCESS, SQLBindCol(
            stmt_, 1, SQL_C_CHAR, output.data() + 1, buffer_length, &indicator));
        ASSERT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
        const std::string character = "\xE4\xBD\xA0";
        const std::string expected = buffer_length == 7 ?
            character + character : character + character + character;
        EXPECT_EQ(0, std::memcmp(output.data() + 1, expected.c_str(), expected.size() + 1));
        EXPECT_EQ(0xCC, output.front());
        EXPECT_EQ(0xCC, output.back());
        EXPECT_EQ(buffer_length == 7 ? 214 : 215, indicator)
            << "211 wire bytes plus expansion of three or four converted characters";

        SQLINTEGER following = 0;
        ASSERT_EQ(SQL_SUCCESS, SQLGetData(
            stmt_, 2, SQL_C_SLONG, &following, sizeof(following), nullptr));
        EXPECT_EQ(42, following);
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
        ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
        ASSERT_EQ(SQL_SUCCESS, SQLFreeStmt(stmt_, SQL_UNBIND));
    }
}

// A slot too small for the converted value truncates on a character boundary.
// Retail uses converted output plus a 1:1 estimate for unread source
// (sqlcdata.h:1230): Linux package 18.6.2.1-1, SQL_DRIVER_VER 18.06.0002, reports
// 5008 under C.UTF-8. Bound delivery uses the same estimate and raw-drains the
// discarded tail instead of converting all 10,000 UTF-8 bytes.
TEST_F(FetchScrollLiveTest, ABoundVarcharMaxTruncatedToCharKeepsConcreteLength) {
    if (ODBCTestUtils::Utf8ToNativeClient("\xE2\x82\xAC") != "\xE2\x82\xAC") {
        GTEST_SKIP() << "This regression pins UTF-8 client length estimates";
    }
    ExecDirect(
        "SELECT REPLICATE(CAST(NCHAR(233) COLLATE SQL_Latin1_General_CP1_CI_AS AS VARCHAR(MAX)), "
        "5000) AS c1");

    // 8 payload bytes: four whole two-byte characters, and no room for a fifth.
    SQLCHAR buf[9] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    EXPECT_EQ(RepeatedPrefix("\xC3\xA9", sizeof(buf) - 1),
              reinterpret_cast<const char*>(buf));
    EXPECT_EQ(5008, ind) << "4992 unread bytes + 16 converted bytes (emitted and carry)";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// A UTF-8 collation is already in the target encoding, so it is delivered by a
// verbatim byte copy rather than through the decoder. That makes it the one
// SQL_C_CHAR shape whose slot boundary can land mid-sequence, so the partial
// tail has to be trimmed exactly as it is for `json`. The token is a 3-byte
// character against an 8-byte payload slot: two whole characters fit, and the
// third does not.
//
// The truncation contract is asserted on both legs; only the tail diverges.
// Measured on build 173919: msodbcsql fills all 8 payload bytes and returns
// "\xE4\xBD\xA0\xE4\xBD\xA0\xE4\xBD", ending mid-character, where this driver
// stops at 6. That is the same deliberate deviation already registered for the
// SQL_C_WCHAR surrogate-pair case in
// .github/instructions/mssql-odbc.instructions.md (item 8, AB#47767): this
// driver trims a bound `max` column to a whole character where msodbcsql fills
// the slot. Split per-leg rather than skipped so the shared part -- that both
// drivers truncate, report 01004, and deliver a prefix of the value -- stays
// measured against msodbcsql.
TEST_F(FetchScrollLiveTest, ABoundUtf8CollationVarcharMaxTruncatesOnACharacterBoundary) {
    ExecDirect(
        "SELECT REPLICATE(CAST(NCHAR(0x4F60) "
        "COLLATE Latin1_General_100_CI_AS_SC_UTF8 AS VARCHAR(MAX)), 500) AS c1");

    // 8 payload bytes: two whole 3-byte characters, and no room for a third.
    SQLCHAR buf[9] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    const auto native_character = ODBCTestUtils::Utf8ToNativeClient("\xE4\xBD\xA0");
    if (native_character != "\xE4\xBD\xA0") {
        const auto expected = RepeatedPrefix(native_character, sizeof(buf) - 1);
        EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(buf), expected.size()));
        EXPECT_EQ(0, buf[expected.size()]);
        SQLFreeStmt(stmt_, SQL_UNBIND);
        SQLCloseCursor(stmt_);
        return;
    }
    const std::string got(reinterpret_cast<const char*>(buf));
    const std::string kSource = "\xE4\xBD\xA0\xE4\xBD\xA0\xE4\xBD\xA0";  // 你你你

    // Shared on both drivers: the value truncated to a prefix that fits.
    EXPECT_LE(got.size(), 8u);
    EXPECT_EQ(kSource.substr(0, got.size()), got) << "delivered bytes must be a prefix";

    const char* target = std::getenv("ODBC_TEST_TARGET");
    if (target && std::string(target) == "msodbcsql") {
        // Fills the slot, ending mid-character (build 173919).
        EXPECT_EQ(8u, got.size());
    } else {
        EXPECT_EQ("\xE4\xBD\xA0\xE4\xBD\xA0", got) << "a partial character must not be delivered";
        EXPECT_EQ(6u, got.size()) << "6 bytes of whole characters, not 8 ending mid-sequence";
    }
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundVarcharMaxTruncatedToWcharReportsNoTotal) {
    ExecDirect("SELECT REPLICATE(CAST('y' AS VARCHAR(MAX)), 5000) AS c1");

    SQLWCHAR buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(SQL_NO_TOTAL, ind) << "the converted UTF-16 length is not known while streaming";
    EXPECT_EQ(u'y', buf[0]);
    EXPECT_EQ(u'y', buf[14]);
    EXPECT_EQ(0, buf[15]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundJsonWidensToWchar) {
    if (!ServerSupportsNativeJson()) {
        GTEST_SKIP() << "server has no native json type";
    }
    ExecDirect("SELECT CAST(N'[\"' + NCHAR(233) + N'\"]' AS JSON) AS c1");

    SQLWCHAR buf[16] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(10, ind) << "five UTF-16 code units";
    EXPECT_EQ(u'[', buf[0]);
    EXPECT_EQ(u'\u00e9', buf[2]);
    EXPECT_EQ(u']', buf[4]);
    EXPECT_EQ(0, buf[5]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundUtf8VarcharMaxDoesNotSplitASurrogatePairWhenWidening) {
    // msodbcsql leaves the high surrogate in the ninth payload slot here. The
    // Rust driver deliberately applies the whole-character truncation rule used
    // by its existing nvarchar(max) bound delivery instead. Recorded as a
    // deliberate deviation in .github/instructions/mssql-odbc.instructions.md
    // and tracked in AB#47767.
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect(
        "SELECT CAST(REPLICATE((NCHAR(0xD83D) + NCHAR(0xDE00)) "
        "COLLATE Latin1_General_100_CI_AS_SC_UTF8, 500) AS VARCHAR(MAX)) AS c1");

    // 9 usable units: four whole pairs, and no room for the fifth pair.
    SQLWCHAR buf[10] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(SQL_NO_TOTAL, ind);
    for (int i = 0; i < 8; i += 2) {
        EXPECT_GE(buf[i], 0xD800) << "high surrogate at " << i;
        EXPECT_LT(buf[i], 0xDC00) << "high surrogate at " << i;
        EXPECT_GE(buf[i + 1], 0xDC00) << "low surrogate at " << (i + 1);
        EXPECT_LT(buf[i + 1], 0xE000) << "low surrogate at " << (i + 1);
    }
    EXPECT_EQ(0, buf[8]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundVarcharMaxConvertsToTypedCTargetLikeNonMax) {
    ExecDirect(
        "SELECT CAST('42' AS VARCHAR(50)), CAST('42' AS VARCHAR(MAX)), "
        "CAST(N'42' AS NVARCHAR(MAX))");

    SQLINTEGER regular = 0;
    SQLINTEGER varcharMax = 0;
    SQLINTEGER nvarcharMax = 0;
    SQLLEN regularInd = 0;
    SQLLEN varcharMaxInd = 0;
    SQLLEN nvarcharMaxInd = 0;
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 1, SQL_C_SLONG, &regular, sizeof(regular), &regularInd),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 2, SQL_C_SLONG, &varcharMax, sizeof(varcharMax), &varcharMaxInd),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 3, SQL_C_SLONG, &nvarcharMax, sizeof(nvarcharMax), &nvarcharMaxInd),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(regular, varcharMax);
    EXPECT_EQ(regular, nvarcharMax);
    EXPECT_EQ(42, varcharMax);
    EXPECT_EQ(regularInd, varcharMaxInd);
    EXPECT_EQ(regularInd, nvarcharMaxInd);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), varcharMaxInd);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundVarcharMaxTypedConversionErrorStillDrainsTheRow) {
    ExecDirect(
        "SELECT CAST(v AS VARCHAR(MAX)), n FROM "
        "(VALUES (1, 'not-a-number'), (2, '8')) AS t(n, v) ORDER BY n");

    SQLINTEGER converted = 0;
    SQLINTEGER tail = 0;
    SQLLEN convertedInd = 0;
    SQLLEN tailInd = 0;
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 1, SQL_C_SLONG, &converted, sizeof(converted), &convertedInd),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &tail, sizeof(tail), &tailInd),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");

    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(8, converted) << "the failed PLP conversion must leave the next row synchronized";
    EXPECT_EQ(2, tail);
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, AOversizedBoundVarcharMaxTypedConversionIsRefusedAndDrained) {
    // The 1 MiB materialization cap is this driver's own resource bound and has
    // no msodbcsql counterpart, so parity cannot hold here by construction.
    // Recorded as a deliberate deviation in
    // .github/instructions/mssql-odbc.instructions.md and tracked in AB#47767.
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect(
        "SELECT v, n FROM ("
        "SELECT REPLICATE(CAST('1' AS VARCHAR(MAX)), 1048577) AS v, 1 AS n "
        "UNION ALL SELECT CAST('8' AS VARCHAR(MAX)), 2) AS t ORDER BY n");

    SQLINTEGER converted = 0;
    SQLINTEGER n = 0;
    SQLLEN convertedInd = 0;
    SQLLEN nInd = 0;
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 1, SQL_C_SLONG, &converted, sizeof(converted), &convertedInd),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_SLONG, &n, sizeof(n), &nInd), SQL_HANDLE_STMT,
                  stmt_);

    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_EQ(8, converted) << "the oversized PLP must be drained before the next row";
    EXPECT_EQ(2, n);
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Non-ASCII proves that the output is transcoded to the native client encoding.
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxTranscodesNonAscii) {
    ExecDirect("SELECT CAST(REPLICATE(NCHAR(233), 4) AS NVARCHAR(MAX)) AS c1");

    unsigned char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    const auto expected = ODBCTestUtils::Utf8ToNativeClient("\xC3\xA9\xC3\xA9\xC3\xA9\xC3\xA9");
    EXPECT_EQ(static_cast<SQLLEN>(expected.size()), ind);
    EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(buf), expected.size()));
    EXPECT_EQ(0, buf[expected.size()]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// This driver keeps complete client characters, while msodbcsql truncates at
// the output byte capacity even when that splits a multibyte character.
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxTruncatesOnACharacterBoundary) {
    ExecDirect("SELECT REPLICATE(CAST(NCHAR(233) AS NVARCHAR(MAX)), 5000) AS c1");

    unsigned char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    const auto expected = ExpectedBoundClientPrefix(ODBCTestUtils::Utf8ToNativeClient("\xC3\xA9"),
                                                    sizeof(buf) - 1);
    EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(buf), expected.size()));
    EXPECT_EQ(0, buf[expected.size()]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchScrollLiveTest, ABoundJsonTruncatesOnACharacterBoundary) {
    if (!ServerSupportsNativeJson()) {
        GTEST_SKIP() << "server has no native json type";
    }
    ExecDirect(
        "SELECT CAST(N'[\"' + REPLICATE(NCHAR(233), 20) + N'\"]' AS JSON) AS c1");

    unsigned char buf[10] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    const auto prefix = ODBCTestUtils::Utf8ToNativeClient("[\"");
    const auto expected = prefix + ExpectedBoundClientPrefix(ODBCTestUtils::Utf8ToNativeClient("\xC3\xA9"),
                                                             sizeof(buf) - 1 - prefix.size());
    EXPECT_EQ(expected, std::string(reinterpret_cast<const char*>(buf), expected.size()));
    EXPECT_EQ(0, buf[expected.size()]);
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// The wide-target equivalent: a surrogate pair must not be split across the
// capacity boundary. U+1F600 is two code units, so an odd-sized buffer would
// otherwise end on a lone high surrogate. msodbcsql trims it too
// (GetColDataSurrogateSafe).
TEST_F(FetchScrollLiveTest, ABoundNvarcharMaxDoesNotSplitASurrogatePair) {
    ExecDirect(
        "SELECT REPLICATE(CAST(NCHAR(0xD83D) + NCHAR(0xDE00) AS NVARCHAR(MAX)), 500) AS c1");

    // 9 usable units: four whole pairs, and no room for the ninth's low half.
    SQLWCHAR buf[10] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");

    int units = 0;
    while (units < 10 && buf[units] != 0) {
        ++units;
    }
    EXPECT_EQ(8, units) << "four whole pairs; the fifth high surrogate is dropped";
    ASSERT_EQ(0, units % 2);
    for (int i = 0; i < units; i += 2) {
        EXPECT_GE(buf[i], 0xD800) << "high surrogate at " << i;
        EXPECT_LT(buf[i], 0xDC00) << "high surrogate at " << i;
        EXPECT_GE(buf[i + 1], 0xDC00) << "low surrogate at " << (i + 1);
        EXPECT_LT(buf[i + 1], 0xE000) << "low surrogate at " << (i + 1);
    }
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// Bound VARBINARY(MAX) delivery across a rowset with a trailing scalar, so a
// mis-sized drain would corrupt the following column and row.
TEST_F(FetchScrollLiveTest, ABoundVarbinaryMaxDeliversAcrossARowset) {
    // Two rows and a trailing scalar: a bound LOB is drained into the caller's
    // buffer, and the value after it -- and the row after that -- still have to
    // decode, which is what a mis-sized drain would break.
    //
    // The large value is intentional to exercise repeated PLP chunk reads and
    // draining across packets. PLP metadata selects deliver_bound_plp regardless
    // of the value's size.
    ExecDirect(
        "SELECT n, CAST(REPLICATE(CAST(0x41 AS VARBINARY(MAX)), 1100000) AS VARBINARY(MAX)) "
        "AS lob, n * 11 AS tail "
        "FROM (VALUES (1),(2)) AS t(n) ORDER BY n");
    SQLSMALLINT lobType = 0;
    ASSERT_SQL_OK(SQLDescribeCol(stmt_, 2, nullptr, 0, nullptr, &lobType, nullptr, nullptr, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_VARBINARY, lobType);

    SQLINTEGER n = -1;
    unsigned char buf[32] = {};
    SQLINTEGER tail = -1;
    SQLLEN nInd = 0, ind = 0, tailInd = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &n, sizeof(n), &nInd), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_BINARY, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 3, SQL_C_SLONG, &tail, sizeof(tail), &tailInd),
                  SQL_HANDLE_STMT, stmt_);

    // The LOB does not fit, so the row truncates and reports the full length.
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_EQ(1, n);
    EXPECT_EQ(1100000, ind) << "the untruncated byte count";
    // Every byte of the slot is payload: a terminator would cost the last one.
    for (size_t i = 0; i < sizeof(buf); ++i) {
        EXPECT_EQ(0x41, buf[i]) << "byte " << i << " of the bound binary slot";
    }
    EXPECT_EQ(11, tail) << "the column after the LOB must still decode";

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetch(stmt_));
    EXPECT_EQ(2, n);
    EXPECT_EQ(22, tail);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// The typed-conversion path reaches the same refusal by a different route: a
// binary PLP column has no source encoding to convert from, so it is drained and
// refused before any conversion is attempted. Same AB#47239 gap as above, so it
// likewise asserts our own answer rather than parity. Measured on msodbcsql
// 18.06.0001, varbinary(max) into SQL_C_SLONG answers 07006 instead of this
// driver's HYC00; AB#47239 tracks implementing the missing conversion here.
//
// PLP metadata selects the streaming path regardless of the value's size. This
// test keeps a large value to verify that refusing a binary PLP before typed
// conversion still drains every chunk before the trailing column and next row.
TEST_F(FetchScrollLiveTest, ABoundStreamedVarbinaryMaxToTypedCTargetIsStillUnsupported) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect(
        "SELECT n, CONVERT(varbinary(max), REPLICATE(CONVERT(varchar(max), 'a'), 1100000)) "
        "AS lob, n * 11 AS tail FROM (VALUES (1),(2)) AS t(n) ORDER BY n");

    SQLINTEGER n = -1;
    SQLINTEGER converted = -1;
    SQLINTEGER tail = -1;
    SQLLEN nInd = 0, convertedInd = 0, tailInd = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &n, sizeof(n), &nInd), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(
        SQLBindCol(stmt_, 2, SQL_C_SLONG, &converted, sizeof(converted), &convertedInd),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 3, SQL_C_SLONG, &tail, sizeof(tail), &tailInd),
                  SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
    EXPECT_EQ(11, tail) << "the column after a refused LOB must still decode";

    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_EQ(2, n) << "the drain must leave the next row synchronized";
    EXPECT_EQ(22, tail);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}
