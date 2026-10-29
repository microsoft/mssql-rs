// Copyright (c) Microsoft Corporation. All rights reserved.
// get_diag_rec_test.cpp - Tests for SQLGetDiagRecW.
//
// Verifies:
//   1. NoRecordsReturnsNoData    - empty diag list -> SQL_NO_DATA
//   2. InvalidVersionPostsHy024  - HY024 surfaces after bad SQLSetEnvAttr
//   3. UnknownAttributePostsHy092 - HY092 surfaces for unknown attribute
//   4. SuccessClearsPriorRecords - successful call wipes prior diag
//   5. TruncationReturnsSuccessWithInfo - short buffer -> SUCCESS_WITH_INFO
//   6. RecordTwoReturnsNoData    - only one record posted -> N=2 -> SQL_NO_DATA
//   7. ParameterizedThrow...     - a server error raised from bound values posts
//                                  one STMT record with that number and message,
//                                  nothing on the DBC, and clears on reuse
//
// Notes:
//   - Some DMs (notably unixODBC) intercept HY024 for SQL_ATTR_ODBC_VERSION
//     and post their own diag record before the driver runs. The HY024/HY092
//     tests therefore only assert that *some* record is returned, not that
//     it came from the driver.

#include "odbc_concurrent_test.h"
#include "odbc_test_fixture.h"

#include <cstring>
#include <string>

class GetDiagRecTest : public ::testing::Test {
protected:
    SQLHENV henv_ = SQL_NULL_HENV;

    void SetUp() override {
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv_);
        ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
        ASSERT_NE(henv_, nullptr);
    }

    void TearDown() override {
        if (henv_ != SQL_NULL_HENV) {
            SQLFreeHandle(SQL_HANDLE_ENV, henv_);
            henv_ = SQL_NULL_HENV;
        }
    }
};

static std::string SqlStateToString(const SQLWCHAR state[6]) {
    std::string out;
    for (int i = 0; i < 5 && state[i] != 0; ++i) {
        out.push_back(static_cast<char>(state[i]));
    }
    return out;
}

TEST_F(GetDiagRecTest, NoRecordsReturnsNoData) {
    SQLWCHAR    state[6]  = {0};
    SQLINTEGER  native    = 0;
    SQLWCHAR    msg[256]  = {0};
    SQLSMALLINT text_len  = -1;

    SQLRETURN rc = SQLGetDiagRecW(SQL_HANDLE_ENV, henv_, 1,
                                  state, &native,
                                  msg, sizeof(msg) / sizeof(msg[0]),
                                  &text_len);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

TEST_F(GetDiagRecTest, InvalidVersionPostsDiag) {
    // Bad ODBC version. DM or driver must post a diag record.
    SQLRETURN rc = SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                                 reinterpret_cast<SQLPOINTER>(9999), 0);
    ASSERT_NE(SQL_SUCCESS, rc);
    ASSERT_NE(SQL_SUCCESS_WITH_INFO, rc);

    SQLWCHAR    state[6]  = {0};
    SQLINTEGER  native    = 0;
    SQLWCHAR    msg[256]  = {0};
    SQLSMALLINT text_len  = 0;

    SQLRETURN diag_rc = SQLGetDiagRecW(SQL_HANDLE_ENV, henv_, 1,
                                       state, &native,
                                       msg, sizeof(msg) / sizeof(msg[0]),
                                       &text_len);
    EXPECT_TRUE(diag_rc == SQL_SUCCESS || diag_rc == SQL_SUCCESS_WITH_INFO);
}

TEST_F(GetDiagRecTest, UnknownAttributePostsDiag) {
    SQLRETURN rc = SQLSetEnvAttr(henv_, 99999,
                                 reinterpret_cast<SQLPOINTER>(0), 0);
    ASSERT_NE(SQL_SUCCESS, rc);
    ASSERT_NE(SQL_SUCCESS_WITH_INFO, rc);

    SQLWCHAR    state[6]  = {0};
    SQLINTEGER  native    = 0;
    SQLWCHAR    msg[256]  = {0};
    SQLSMALLINT text_len  = 0;

    SQLRETURN diag_rc = SQLGetDiagRecW(SQL_HANDLE_ENV, henv_, 1,
                                       state, &native,
                                       msg, sizeof(msg) / sizeof(msg[0]),
                                       &text_len);
    EXPECT_TRUE(diag_rc == SQL_SUCCESS || diag_rc == SQL_SUCCESS_WITH_INFO);
}

TEST_F(GetDiagRecTest, SuccessClearsPriorRecords) {
    // Provoke a diag, then a successful call must clear it
    SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                  reinterpret_cast<SQLPOINTER>(9999), 0);

    SQLRETURN ok = SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                                 reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80),
                                 0);
    ASSERT_TRUE(ok == SQL_SUCCESS || ok == SQL_SUCCESS_WITH_INFO);

    SQLSMALLINT text_len = -1;
    SQLRETURN diag_rc = SQLGetDiagRecW(SQL_HANDLE_ENV, henv_, 1,
                                       nullptr, nullptr,
                                       nullptr, 0, &text_len);
    EXPECT_EQ(SQL_NO_DATA, diag_rc);
}

TEST_F(GetDiagRecTest, RecordTwoReturnsNoData) {
    SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                  reinterpret_cast<SQLPOINTER>(9999), 0);

    SQLSMALLINT text_len = -1;
    SQLRETURN rc = SQLGetDiagRecW(SQL_HANDLE_ENV, henv_, 2,
                                  nullptr, nullptr,
                                  nullptr, 0, &text_len);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// A failing call should clear prior diagnostics first
// then post its own current diagnostic.
TEST_F(GetDiagRecTest, FailingCallClearsPriorRecordsBeforePostingNewOne) {
    // First failing call: unknown attribute.
    SQLRETURN rc1 = SQLSetEnvAttr(henv_, 99999,
                                  reinterpret_cast<SQLPOINTER>(0), 0);
    ASSERT_NE(SQL_SUCCESS, rc1);
    ASSERT_NE(SQL_SUCCESS_WITH_INFO, rc1);

    SQLWCHAR state1[6] = {0};
    SQLINTEGER native1 = 0;
    SQLWCHAR msg1[256] = {0};
    SQLSMALLINT text_len1 = 0;
    SQLRETURN d1 = SQLGetDiagRecW(SQL_HANDLE_ENV, henv_, 1,
                                  state1, &native1,
                                  msg1, sizeof(msg1) / sizeof(msg1[0]),
                                  &text_len1);
    ASSERT_TRUE(d1 == SQL_SUCCESS || d1 == SQL_SUCCESS_WITH_INFO);
    std::string first_state = SqlStateToString(state1);
    ASSERT_FALSE(first_state.empty());

    // Second failing call: invalid ODBC version.
    SQLRETURN rc2 = SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                                  reinterpret_cast<SQLPOINTER>(9999), 0);
    ASSERT_NE(SQL_SUCCESS, rc2);
    ASSERT_NE(SQL_SUCCESS_WITH_INFO, rc2);

    SQLWCHAR state2[6] = {0};
    SQLINTEGER native2 = 0;
    SQLWCHAR msg2[256] = {0};
    SQLSMALLINT text_len2 = 0;
    SQLRETURN d2 = SQLGetDiagRecW(SQL_HANDLE_ENV, henv_, 1,
                                  state2, &native2,
                                  msg2, sizeof(msg2) / sizeof(msg2[0]),
                                  &text_len2);
    ASSERT_TRUE(d2 == SQL_SUCCESS || d2 == SQL_SUCCESS_WITH_INFO);
    std::string second_state = SqlStateToString(state2);
    ASSERT_FALSE(second_state.empty());

    // The new failing call should have replaced prior diagnostics.
    EXPECT_NE(first_state, second_state);

    // Record 2 should return SQL_NO_DATA coz old diag record was cleared
    SQLSMALLINT text_len = -1;
    SQLRETURN rc = SQLGetDiagRecW(SQL_HANDLE_ENV, henv_, 2,
                                  nullptr, nullptr,
                                  nullptr, 0, &text_len);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class GetDiagRecLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// THROW raised from bound parameter values: the statement holds exactly one
// record carrying SQLSTATE 42000, the bound number as the native error, and the
// bound message. The error must not leak onto the connection handle, and a
// successful reuse of the same statement and bindings clears it.
// Benefits-from-mock-tds: place the ERROR token mid-batch and control its
// number, state, and class directly; SQL Server decides all three here, and
// only a mock can prove nothing was posted on the DBC rather than observing it.
ODBC_TEST_F(GetDiagRecLiveTest, ParameterizedThrowPostsOneStatementRecordAndClearsOnReuse) {
    SQLINTEGER number = 50000 + Worker() * 1000 + Iteration() % 1000;
    const std::string message = WorkerToken("diag-throw");
    SQLCHAR message_buffer[64] = {};
    ASSERT_LT(message.size(), sizeof(message_buffer));
    std::memcpy(message_buffer, message.data(), message.size());
    SQLLEN number_ind = 0;
    SQLLEN message_ind = SQL_NTS;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                   &number, 0, &number_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_WVARCHAR, 200, 0,
                                   message_buffer, sizeof(message_buffer), &message_ind),
                  SQL_HANDLE_STMT, stmt_);

    const SqlTString throw_sql = ODBCTestUtils::ToSqlTStr(
        "DECLARE @n int = ?; DECLARE @m nvarchar(200) = ?; THROW @n, @m, 1;");
    ASSERT_EQ(SQL_ERROR, SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(throw_sql.c_str()), SQL_NTS));

    SQLINTEGER count = -1;
    ASSERT_SQL_OK(SQLGetDiagField(SQL_HANDLE_STMT, stmt_, 0, SQL_DIAG_NUMBER, &count, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, count);

    SQLTCHAR state[6] = {};
    SQLINTEGER native = 0;
    SQLTCHAR text[512] = {};
    SQLSMALLINT text_len = 0;
    ASSERT_EQ(SQL_SUCCESS,
              SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state, &native, text,
                            static_cast<SQLSMALLINT>(sizeof(text) / sizeof(text[0])), &text_len));
    EXPECT_EQ("42000", ODBCTestUtils::ToNarrow(SqlTString(state)));
    EXPECT_EQ(number, native);
    const std::string diagnostic = ODBCTestUtils::ToNarrow(SqlTString(text));
    EXPECT_NE(std::string::npos, diagnostic.find(message)) << diagnostic;

    EXPECT_EQ(SQL_NO_DATA, SQLGetDiagRec(SQL_HANDLE_DBC, dbc_, 1, state, &native, text,
                                         static_cast<SQLSMALLINT>(sizeof(text) / sizeof(text[0])),
                                         &text_len))
        << "statement error leaked onto the connection handle";

    const SqlTString select_sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS n, ? AS m");
    ASSERT_EQ(SQL_SUCCESS,
              SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(select_sql.c_str()), SQL_NTS))
        << ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NO_DATA, SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state, &native, text,
                                         static_cast<SQLSMALLINT>(sizeof(text) / sizeof(text[0])),
                                         &text_len))
        << "successful reuse kept the earlier record";

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER echoed_number = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &echoed_number, 0, nullptr), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(number, echoed_number);
    SQLCHAR echoed_message[64] = {};
    SQLLEN echoed_ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_CHAR, echoed_message, sizeof(echoed_message),
                             &echoed_ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(message, reinterpret_cast<const char*>(echoed_message));

    SQLCloseCursor(stmt_);
    SQLFreeStmt(stmt_, SQL_RESET_PARAMS);
}
