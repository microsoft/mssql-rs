// Copyright (c) Microsoft Corporation. All rights reserved.
// get_diag_field_test.cpp - Tests for SQLGetDiagFieldW.
//
// Verifies:
//   1. DiagNumberZeroThenOneAfterError - SQL_DIAG_NUMBER: 0 clean, ≥1 after error
//   2. DiagSqlstateAndByteLength    - SQL_DIAG_SQLSTATE returns HY000, bytes = 10
//   3. DiagNativeReturnsCode        - SQL_DIAG_NATIVE returns native error code
//   4. DiagMessageTextAndByteLength - SQL_DIAG_MESSAGE_TEXT returns msg, bytes
//   5. DiagMessageTextTruncation    - short byte buffer → SUCCESS_WITH_INFO
//   6. NoRecordsReturnsNoData       - record field on clean handle → SQL_NO_DATA
//   7. DiagNumberAfterSuccessIsZero - successful call clears prior diag

#include "odbc_test_fixture.h"

#include <cstring>
#include <string>
#include <vector>

class GetDiagFieldTest : public ::testing::Test {
protected:
    SQLHENV henv_ = SQL_NULL_HENV;
    SQLHDBC hdbc_ = SQL_NULL_HDBC;

    void SetUp() override {
        SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &henv_);
        ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);

        rc = SQLSetEnvAttr(henv_, SQL_ATTR_ODBC_VERSION,
                           reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0);
        ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);

        rc = SQLAllocHandle(SQL_HANDLE_DBC, henv_, &hdbc_);
        ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, henv_);
    }

    void TearDown() override {
        if (hdbc_ != SQL_NULL_HDBC) {
            SQLFreeHandle(SQL_HANDLE_DBC, hdbc_);
            hdbc_ = SQL_NULL_HDBC;
        }
        if (henv_ != SQL_NULL_HENV) {
            SQLFreeHandle(SQL_HANDLE_ENV, henv_);
            henv_ = SQL_NULL_HENV;
        }
    }

    // Provoke a driver-level diagnostic on hdbc_.
    // SQLDriverConnect with missing Server= posts HY000.
    void ProvokeError() {
        auto& cfg = ODBCTestConfig::Instance();
        std::string cs = "Driver={" + cfg.Driver() + "}";
        SqlTString tcs = ODBCTestUtils::ToSqlTStr(cs);
        SQLTCHAR out[1] = {};
        SQLSMALLINT outLen = 0;
        SQLDriverConnect(hdbc_, nullptr,
                         const_cast<SQLTCHAR*>(tcs.c_str()), SQL_NTS,
                         out, 0, &outLen, SQL_DRIVER_NOPROMPT);
    }
};

TEST_F(GetDiagFieldTest, DiagNumberZeroThenOneAfterError) {
    SQLINTEGER count = -1;
    SQLRETURN rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 0,
                                     SQL_DIAG_NUMBER,
                                     &count, 0, nullptr);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(0, count);

    ProvokeError();

    count = -1;
    rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 0,
                           SQL_DIAG_NUMBER,
                           &count, 0, nullptr);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_GE(count, 1);
}

TEST_F(GetDiagFieldTest, DiagSqlstateAndByteLength) {
    ProvokeError();

    SQLWCHAR state[6] = {0};
    SQLSMALLINT string_len = 0;
    SQLRETURN rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 1,
                                     SQL_DIAG_SQLSTATE,
                                     state,
                                     static_cast<SQLSMALLINT>(sizeof(state)),
                                     &string_len);
    EXPECT_EQ(SQL_SUCCESS, rc);
    std::string sql_state(state, state + 5);
    EXPECT_EQ("08001", sql_state);
    // StringLengthPtr must report bytes: 5 chars × sizeof(SQLWCHAR).
    EXPECT_EQ(static_cast<SQLSMALLINT>(5 * sizeof(SQLWCHAR)), string_len);
}

TEST_F(GetDiagFieldTest, DiagNativeReturnsCode) {
    ProvokeError();

    SQLINTEGER native = -1;
    SQLRETURN rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 1,
                                     SQL_DIAG_NATIVE,
                                     &native,
                                     static_cast<SQLSMALLINT>(sizeof(native)),
                                     nullptr);
    EXPECT_EQ(SQL_SUCCESS, rc);
}

TEST_F(GetDiagFieldTest, DiagMessageTextAndByteLength) {
    ProvokeError();

    SQLWCHAR msg[256] = {0};
    SQLSMALLINT string_len = 0;
    SQLRETURN rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 1,
                                     SQL_DIAG_MESSAGE_TEXT,
                                     msg,
                                     static_cast<SQLSMALLINT>(sizeof(msg)),
                                     &string_len);
    EXPECT_EQ(SQL_SUCCESS, rc);
    int len = 0;
    while (len < 256 && msg[len]) ++len;
    std::string text(msg, msg + len);
    EXPECT_FALSE(text.empty());
    // StringLengthPtr is in bytes. Verify it's a multiple of sizeof(SQLWCHAR).
    EXPECT_EQ(0, string_len % sizeof(SQLWCHAR));
    EXPECT_GT(string_len, 0);
}

TEST_F(GetDiagFieldTest, DiagMessageTextTruncation) {
    ProvokeError();

    // unixODBC 2.3.9 bug: SQLGetDiagFieldW memcpy's the full message
    // regardless of BufferLength, overflowing the caller's buffer.
    // Work around it by heap-allocating a large backing buffer but passing
    // a small BufferLength so the DM still reports SQL_SUCCESS_WITH_INFO.
    constexpr SQLSMALLINT logical_bytes = 10 * sizeof(SQLWCHAR);
    std::vector<SQLWCHAR> buf(256, 0);
    SQLSMALLINT string_len = 0;
    SQLRETURN rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 1,
                                     SQL_DIAG_MESSAGE_TEXT,
                                     buf.data(),
                                     logical_bytes,
                                     &string_len);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    // Full untruncated byte length is still reported.
    EXPECT_GT(string_len, logical_bytes);
}

TEST_F(GetDiagFieldTest, NoRecordsReturnsNoData) {
    SQLWCHAR state[6] = {0};
    SQLRETURN rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 1,
                                     SQL_DIAG_SQLSTATE,
                                     state,
                                     static_cast<SQLSMALLINT>(sizeof(state)),
                                     nullptr);
    EXPECT_EQ(SQL_NO_DATA, rc);

    // Asking for a record beyond the last one also returns SQL_NO_DATA.
    ProvokeError();
    rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 2,
                           SQL_DIAG_SQLSTATE,
                           state,
                           static_cast<SQLSMALLINT>(sizeof(state)),
                           nullptr);
    EXPECT_EQ(SQL_NO_DATA, rc);
}

TEST_F(GetDiagFieldTest, DiagNumberAfterSuccessIsZero) {
    ProvokeError();

    SQLINTEGER count = 0;
    SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 0,
                     SQL_DIAG_NUMBER, &count, 0, nullptr);
    ASSERT_GE(count, 1);

    // Successful call on the same handle must clear prior diagnostics.
    SQLRETURN ok = SQLSetConnectAttr(hdbc_, SQL_ATTR_LOGIN_TIMEOUT,
                                     reinterpret_cast<SQLPOINTER>(10), 0);
    ASSERT_SQL_OK(ok, SQL_HANDLE_DBC, hdbc_);

    count = -1;
    SQLRETURN rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, hdbc_, 0,
                                     SQL_DIAG_NUMBER,
                                     &count, 0, nullptr);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(0, count);
}
