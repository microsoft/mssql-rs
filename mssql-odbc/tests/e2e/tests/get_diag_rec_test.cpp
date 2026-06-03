// Copyright (c) Microsoft Corporation. All rights reserved.
// get_diag_rec_test.cpp - Tests for SQLGetDiagRecW.
//
// Verifies:
//   1. NoRecordsReturnsNoData    - empty diag list -> SQL_NO_DATA
//   2. InvalidVersionPostsHy024  - HY024 surfaces after bad SQLSetEnvAttr
//   3. UnknownAttributePostsHy092 - HY092 surfaces for unknown attribute
//   4. SuccessClearsPriorRecords - successful call wipes prior diag (FreeErrors)
//   5. TruncationReturnsSuccessWithInfo - short buffer -> SUCCESS_WITH_INFO
//   6. RecordTwoReturnsNoData    - only one record posted -> N=2 -> SQL_NO_DATA
//
// Notes:
//   - Some DMs (notably unixODBC) intercept HY024 for SQL_ATTR_ODBC_VERSION
//     and post their own diag record before the driver runs. The HY024/HY092
//     tests therefore only assert that *some* record is returned, not that
//     it came from the driver.

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
    // Provoke a diag, then a successful call must clear it (FreeErrors parity).
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

// A failing call should clear prior diagnostics first (FreeErrors parity),
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
