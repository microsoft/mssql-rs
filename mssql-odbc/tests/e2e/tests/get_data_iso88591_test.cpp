// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"

#include <langinfo.h>

#include <cstdint>
#include <string>

namespace {

class Iso88591GetDataTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured";
        }
        Connect();
    }

    SQLRETURN ExecDirect(const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    std::string StmtDiagState() const {
        return ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
    }
};

TEST_F(Iso88591GetDataTest, SuccessfulSubstitutionHasOnlyTruncationDiagnostic) {
    const std::string codeset = nl_langinfo(CODESET);
    if (codeset != "ISO-8859-1" && codeset != "ISO8859-1") {
        GTEST_SKIP() << "no ISO-8859-1 locale is installed";
    }

    SQLCHAR version[64] = {};
    ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    RecordProperty("SQL_DRIVER_VER", reinterpret_cast<const char*>(version));

    for (const bool warn : {false, true}) {
        ASSERT_EQ(SQL_SUCCESS, SQLSetConnectAttr(
            dbc_, SQL_COPT_SS_WARN_ON_CP_ERROR,
            reinterpret_cast<SQLPOINTER>(static_cast<uintptr_t>(warn)), 0));
        for (const char* type : {"nvarchar(32)", "nvarchar(max)"}) {
            ASSERT_EQ(SQL_SUCCESS, ExecDirect(
                "SELECT CAST(NCHAR(0x4F60)+N'ABC' AS " + std::string(type) + ")"));
            ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

            SQLCHAR bytes[3] = {0xCC, 0xCC, 0xCC};
            SQLLEN indicator = -99;
            ASSERT_EQ(SQL_SUCCESS_WITH_INFO,
                      SQLGetData(stmt_, 1, SQL_C_CHAR, bytes, 2, &indicator));
            EXPECT_EQ("01004", StmtDiagState());
            EXPECT_FALSE(ODBCTestUtils::HasDiagState(SQL_HANDLE_STMT, stmt_, "01000"));
            EXPECT_EQ(ODBCTestUtils::Utf8ToNativeClient("\xE4\xBD\xA0")[0],
                      static_cast<char>(bytes[0]));
            EXPECT_EQ(0, bytes[1]);
            EXPECT_EQ(0xCC, bytes[2]);
            EXPECT_TRUE(indicator == SQL_NO_TOTAL || indicator == 4);
            ASSERT_EQ(SQL_SUCCESS, SQLCloseCursor(stmt_));
        }
    }
}

}  // namespace
