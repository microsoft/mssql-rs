// Copyright (c) Microsoft Corporation. All rights reserved.

#include "odbc_test_fixture.h"

#include <array>
#include <limits>

class ApiContractLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
            << "Set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        Connect();
        SQLTCHAR version[32] = {};
        ASSERT_SQL_OK(SQLGetInfo(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                      SQL_HANDLE_DBC, dbc_);
        RecordProperty("driver_version", ODBCTestUtils::ToNarrow(SqlTString(version)));
    }

    void ExpectCount(SQLSMALLINT expected) {
        SQLSMALLINT count = -1;
        ASSERT_SQL_OK(SQLNumParams(stmt_, &count), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, count);
    }

    void ExpectNoStatement() {
        SQLSMALLINT count = -1;
        ASSERT_EQ(SQL_ERROR, SQLNumParams(stmt_, &count));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
        EXPECT_EQ(-1, count);
    }

    void Prepare(const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        ASSERT_SQL_OK(
            SQLPrepare(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, stmt_);
    }

    void BindInteger(SQLUSMALLINT ordinal, SQLINTEGER& value) {
        ASSERT_SQL_OK(
            SQLBindParameter(stmt_, ordinal, SQL_PARAM_INPUT, SQL_C_SLONG,
                             SQL_INTEGER, 10, 0, &value, 0, nullptr),
            SQL_HANDLE_STMT, stmt_);
    }
};

TEST_F(ApiContractLiveTest, NativeSqlRejectsInvalidInputLengthsAndReplacesDiagnostics) {
    SQLWCHAR input[] = {'S', 'E', 'L', 'E', 'C', 'T', ' ', '1', 0};
    const SQLINTEGER invalid_lengths[] = {-1, -2, -4, (std::numeric_limits<SQLINTEGER>::min)()};
    for (SQLINTEGER invalid : invalid_lengths) {
        SCOPED_TRACE(invalid);
        std::array<SQLWCHAR, 32> out{};
        SQLINTEGER length = -42;
        ASSERT_EQ(SQL_SUCCESS_WITH_INFO,
                  SQLNativeSqlW(dbc_, input, SQL_NTS, out.data(), 2, &length));
        EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "01004");
        out.fill(77);
        length = -42;
        ASSERT_EQ(SQL_ERROR,
                  SQLNativeSqlW(dbc_, input, invalid, out.data(),
                                static_cast<SQLINTEGER>(out.size()), &length));
        EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY090");
        EXPECT_EQ(-42, length);
        for (SQLWCHAR c : out) {
            EXPECT_EQ(77, c);
        }
        SQLWCHAR state[6] = {};
        EXPECT_EQ(SQL_NO_DATA, SQLGetDiagRecW(SQL_HANDLE_DBC, dbc_, 2, state,
                                            nullptr, nullptr, 0, nullptr));
    }
}

TEST_F(ApiContractLiveTest, NativeSqlValidLengthsClearErrors) {
    SQLWCHAR input[] = {'S', 'E', 'L', 'E', 'C', 'T', ' ', '1', 0};
    SQLWCHAR out[32] = {};
    for (SQLINTEGER input_length : {SQLINTEGER(SQL_NTS), SQLINTEGER(8), SQLINTEGER(0)}) {
        SQLINTEGER length = -1;
        ASSERT_EQ(SQL_ERROR,
                  SQLNativeSqlW(dbc_, input, -1, out, 32, &length));
        ASSERT_SQL_OK(SQLNativeSqlW(dbc_, input, input_length, out, 32, &length),
                      SQL_HANDLE_DBC, dbc_);
        EXPECT_EQ(input_length == 0 ? 0 : 8, length);
        EXPECT_EQ(0, out[length]);
        SQLWCHAR state[6] = {};
        EXPECT_EQ(SQL_NO_DATA, SQLGetDiagRecW(SQL_HANDLE_DBC, dbc_, 1, state,
                                            nullptr, nullptr, 0, nullptr));
    }
}

TEST_F(ApiContractLiveTest, NumParamsAfterDirectWithoutMarkers) {
    ExpectNoStatement();
    ExecDirect("SELECT '?' AS literal /* ? */");
    ExpectCount(0);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    ExpectNoStatement();
}

TEST_F(ApiContractLiveTest, NumParamsAfterDirectWithMarkersAndReset) {
    SQLINTEGER value = 17;
    BindInteger(1, value);
    ExecDirect("SELECT ? AS value, '?' AS literal /* ? */");
    ExpectCount(1);
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLINTEGER result = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &result, 0, nullptr),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(value, result);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    ExpectCount(1);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    // Closing direct SQL returns the Driver Manager to its allocated state.
    ExpectNoStatement();
    BindInteger(1, value);
    ExecDirect("SELECT ?");
    ExpectCount(1);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    ExecDirect("SELECT 1");
    ExpectCount(0);
}

TEST_F(ApiContractLiveTest, NumParamsTracksPrepareExecuteAndDirectReuse) {
    SQLINTEGER value = 23;
    BindInteger(1, value);
    Prepare("SELECT ?");
    ExpectCount(1);
    for (int execution = 0; execution < 2; ++execution) {
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ExpectCount(1);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }
    ExecDirect("SELECT 1");
    ExpectCount(0);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    Prepare("SELECT ?, ?");
    ExpectCount(2);
    Prepare("SELECT 1");
    ExpectCount(0);
}

TEST_F(ApiContractLiveTest, NumParamsCanRecoverAfterExecutionError) {
    SQLINTEGER value = 17;
    BindInteger(1, value);
    SqlTString invalid = ODBCTestUtils::ToSqlTStr("SELECT ? +");
    ASSERT_EQ(SQL_ERROR,
              SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(invalid.c_str()), SQL_NTS));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "42000");
    ExpectNoStatement();
    ExecDirect("SELECT ?");
    ExpectCount(1);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    Prepare("SELECT 1");
    ExpectCount(0);
}
