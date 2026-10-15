// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#pragma once

#include "odbc_test_fixture.h"

#include <string>

class ConnectionBusyLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    static SQLRETURN Run(SQLHSTMT hstmt, const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(hstmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    static SQLRETURN Prepare(SQLHSTMT hstmt, const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(hstmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    static bool IsBusy(SQLSMALLINT handle_type, SQLHANDLE handle) {
        return ODBCTestUtils::GetDiagState(handle_type, handle) == "HY000" &&
               ODBCTestUtils::GetDiagMessage(handle_type, handle).find("Connection is busy") !=
                   std::string::npos;
    }

    static SQLRETURN BindInt(SQLHSTMT hstmt, SQLUSMALLINT ordinal,
                             SQLINTEGER* value, SQLLEN* indicator) {
        return SQLBindParameter(hstmt, ordinal, SQL_PARAM_INPUT, SQL_C_SLONG,
                                SQL_INTEGER, 0, 0, value, 0, indicator);
    }
};
