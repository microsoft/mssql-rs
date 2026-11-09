// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// SQLExecDirectW and SQLPrepareW take TextLength as SQLINTEGER. A driver that
// reads it as 16 bits runs empty text for 40000 or 65536, a 4464-unit prefix
// for 70000, and treats 65533 (0xFFFD) as SQL_NTS.
//
// The text itself must reach the server unchanged, including non-ASCII and
// surrogate pairs that straddle the driver's read chunks and TDS packets.

#include "odbc_concurrent_test.h"
#include "odbc_test_fixture.h"

#include <algorithm>
#include <mutex>
#include <string>
#include <vector>

namespace {

constexpr SQLINTEGER kLengths[] = {32767, 32768, 40000, 65533, 65536, 70000};

// `SELECT <id>` padded with a comment to exactly `length` units. The units
// after it form a second statement, so reading past `length` returns a second
// result set and stopping short leaves the comment unterminated. The trailing
// NUL keeps a mistaken SQL_NTS scan inside the buffer.
std::vector<SQLWCHAR> PaddedSelect(SQLINTEGER length, int id) {
    const std::string prefix = "SELECT " + std::to_string(id) + " /*";
    const std::string suffix = "*/";
    const size_t fill = static_cast<size_t>(length) - prefix.size() - suffix.size();
    std::string text = prefix + std::string(fill, 'x') + suffix + " SELECT -1";
    std::vector<SQLWCHAR> units(text.begin(), text.end());
    units.push_back(0);
    return units;
}

// Long ASCII runs around a 13-unit period of ASCII, é, 東 and U+1F600. The
// period is coprime with the driver's 64-unit read chunk, so a surrogate pair
// lands on every chunk offset and on packet edges; splitting one there would
// return two U+FFFD.
std::vector<SQLWCHAR> MixedText(const std::string& token) {
    std::vector<SQLWCHAR> units(token.begin(), token.end());
    units.insert(units.end(), 300, 'x');
    for (int i = 0; i < 1600; ++i) {
        for (char c = 'a'; c < 'j'; ++c) {
            units.push_back(static_cast<SQLWCHAR>(c));
        }
        units.insert(units.end(), {0x00E9, 0x6771, 0xD83D, 0xDE00});
    }
    units.insert(units.end(), 300, 'y');
    return units;
}

}  // namespace

class StatementTextLengthTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
            << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        Connect();
    }

    // Exactly one result set holding the one row `id`.
    void ExpectSingleRow(int id) {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER value = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, 0, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(id, value);
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
        EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }
};

ODBC_TEST_F(StatementTextLengthTest, ExecDirectReadsFullTextLength) {
    for (SQLINTEGER length : kLengths) {
        SCOPED_TRACE(length);
        const int id = static_cast<int>(length);
        auto sql = PaddedSelect(length, id);
        ASSERT_SQL_OK(SQLExecDirectW(stmt_, sql.data(), length), SQL_HANDLE_STMT, stmt_);
        ExpectSingleRow(id);
    }
}

ODBC_TEST_F(StatementTextLengthTest, PrepareReadsFullTextLength) {
    for (SQLINTEGER length : kLengths) {
        SCOPED_TRACE(length);
        const int id = static_cast<int>(length);
        auto sql = PaddedSelect(length, id);
        ASSERT_SQL_OK(SQLPrepareW(stmt_, sql.data(), length), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ExpectSingleRow(id);
    }
}

// Benefits-from-mock-tds: a byte-level mock could assert the SQL Batch payload
// itself; the Rust differential tests pin it against the per-unit encoder.
ODBC_TEST_F(StatementTextLengthTest, NonAsciiTextRoundTripsAcrossChunkAndPacketEdges) {
    const auto payload = MixedText(WorkerToken("text"));
    const std::string head = "SELECT N'";
    const std::string tail = "' AS c1";
    std::vector<SQLWCHAR> sql(head.begin(), head.end());
    sql.insert(sql.end(), payload.begin(), payload.end());
    sql.insert(sql.end(), tail.begin(), tail.end());
    const auto length = static_cast<SQLINTEGER>(sql.size());
    sql.push_back(0);

    struct Case {
        const char* name;
        bool prepare;
        SQLINTEGER text_length;
    };
    std::vector<SQLWCHAR> value(payload.size() + 1);
    for (const Case& c : {Case{"ExecDirect SQL_NTS", false, SQL_NTS},
                          Case{"ExecDirect length", false, length},
                          Case{"Prepare SQL_NTS", true, SQL_NTS}}) {
        SCOPED_TRACE(c.name);
        if (c.prepare) {
            ASSERT_SQL_OK(SQLPrepareW(stmt_, sql.data(), c.text_length), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        } else {
            ASSERT_SQL_OK(SQLExecDirectW(stmt_, sql.data(), c.text_length), SQL_HANDLE_STMT,
                          stmt_);
        }
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLLEN ind = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, value.data(),
                                 static_cast<SQLLEN>(value.size() * sizeof(SQLWCHAR)), &ind),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_EQ(static_cast<SQLLEN>(payload.size() * sizeof(SQLWCHAR)), ind);
        const auto diff = std::mismatch(payload.begin(), payload.end(), value.begin());
        EXPECT_EQ(payload.end(), diff.first)
            << "first difference at unit " << (diff.first - payload.begin());
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }
}

// The Driver Manager rejects these before either driver sees them.
ODBC_TEST_F(StatementTextLengthTest, NonPositiveTextLengthOtherThanNtsIsHy090) {
    auto sql = PaddedSelect(16, 1);
    // The DM's HY090 is cleaned up before unixODBC takes its statement lock.
    std::unique_lock<OdbcDiagnosticsMutex> lock(diagnostics_mutex_);
    for (SQLINTEGER length : {SQLINTEGER{0}, SQLINTEGER{-1}}) {
        SCOPED_TRACE(length);
        EXPECT_SQL_ERROR(SQLExecDirectW(stmt_, sql.data(), length));
        EXPECT_EQ("HY090", StmtDiagState());
        EXPECT_SQL_ERROR(SQLPrepareW(stmt_, sql.data(), length));
        EXPECT_EQ("HY090", StmtDiagState());
    }
    SQLULEN timeout = 0;
    ASSERT_SQL_OK(SQLGetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT, &timeout, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
}
