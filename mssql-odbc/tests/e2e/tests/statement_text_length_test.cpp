// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// SQLExecDirectW and SQLPrepareW take TextLength as SQLINTEGER. A driver that
// reads it as 16 bits runs empty text for 40000 or 65536, a 4464-unit prefix
// for 70000, and treats 65533 (0xFFFD) as SQL_NTS.

#include "odbc_concurrent_test.h"
#include "odbc_test_fixture.h"

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
    std::string text = prefix + std::string(static_cast<size_t>(length) - prefix.size() - suffix.size(), 'x') + suffix;
    text += " SELECT -1";
    std::vector<SQLWCHAR> units(text.begin(), text.end());
    units.push_back(0);
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
