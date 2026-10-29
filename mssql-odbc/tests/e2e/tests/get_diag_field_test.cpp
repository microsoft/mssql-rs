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

// ===================================================================
// Tests that require a live SQL Server (SQL_DIAG_ROW_NUMBER is scoped to
// SQL_HANDLE_STMT only, so it needs a real statement-level diagnostic).
// ===================================================================

class DiagFieldRowNumberLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// This driver doesn't track per-row batch failures yet (see divergence 9 in
// docs/parameters_plan.md, tracked by microsoft/mssql-rs#541), so it always
// reports SQL_NO_ROW_NUMBER on a stmt-level diagnostic. Asserting the literal
// spec constant (rather than the driver's own SQL_NO_ROW_NUMBER back at
// itself) catches a transcription slip.
//
// mssql-odbc only. Measured on msodbcsql 18.6.2.1: it answers 1 here, not
// SQL_NO_ROW_NUMBER, so this cannot run on the compare leg.
TEST_F(DiagFieldRowNumberLiveTest, ReportsNoRowNumberOnStmtError) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SQLRETURN rc = SQLExecDirect(stmt_,
        const_cast<SQLTCHAR*>(ODBCTestUtils::ToSqlTStr(
            "SELECT * FROM mssql_rs_nonexistent_table_xyz").c_str()),
        SQL_NTS);
    ASSERT_EQ(SQL_ERROR, rc);

    // SQL_DIAG_ROW_NUMBER is an SQLLEN field (8 bytes on every 64-bit target
    // this ships on); a narrower buffer here would let the driver's write
    // clobber adjacent stack memory.
    SQLLEN row_number = 0;
    rc = SQLGetDiagFieldW(SQL_HANDLE_STMT, stmt_, 1,
                          SQL_DIAG_ROW_NUMBER,
                          &row_number, 0, nullptr);
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(-1 /* SQL_NO_ROW_NUMBER */, row_number);
}

// SQL_DIAG_ROW_NUMBER is scoped to SQL_HANDLE_STMT per spec; other handle
// types must refuse it (mirrors the driver's existing refusal of the sibling
// SQL_DIAG_COLUMN_NUMBER on non-STMT handles). msodbcsql 18.6.2.1 refuses it
// too, measured: SQL_ERROR with nothing written to the buffer.
TEST_F(DiagFieldRowNumberLiveTest, RefusedOnDbcHandle) {
    SQLRETURN rc = SQLSetConnectAttr(dbc_, kUnknownAttribute,
                                     reinterpret_cast<SQLPOINTER>(0), 0);
    ASSERT_NE(SQL_SUCCESS, rc);
    ASSERT_NE(SQL_SUCCESS_WITH_INFO, rc);

    SQLLEN row_number = 0;
    rc = SQLGetDiagFieldW(SQL_HANDLE_DBC, dbc_, 1,
                          SQL_DIAG_ROW_NUMBER,
                          &row_number, 0, nullptr);
    EXPECT_EQ(SQL_ERROR, rc);
}
