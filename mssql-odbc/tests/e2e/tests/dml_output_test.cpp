// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"

class DmlOutputTest : public ODBCTest, public testing::WithParamInterface<bool> {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection());
        Connect();
        SQLCHAR version[32] = {};
        ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                      SQL_HANDLE_DBC, dbc_);
        RecordProperty("driver_version", reinterpret_cast<const char*>(version));
        ExecDirect("SET NOCOUNT ON; CREATE TABLE #dml_output (id int, v int); "
                   "INSERT INTO #dml_output VALUES (1, 0), (2, 0)");
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }

    void Procedure(const std::string& body) {
        ExecDirect("CREATE PROCEDURE #dml_outputs " + body);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    }

    SQLRETURN Call(const std::string& sql = "{call #dml_outputs(?)}") {
        auto text = ODBCTestUtils::ToSqlTStr(sql);
        if (GetParam()) {
            SQLRETURN rc = SQLPrepare(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
            if (!SQL_SUCCEEDED(rc)) return rc;
            return SQLExecute(stmt_);
        }
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    void BindInt(SQLUSMALLINT ordinal, SQLINTEGER& value, SQLLEN& length) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, ordinal, SQL_PARAM_OUTPUT,
                                      SQL_C_SLONG, SQL_INTEGER, 10, 0,
                                      &value, 0, &length),
                      SQL_HANDLE_STMT, stmt_);
    }

    void Count(SQLLEN expected) {
        SQLLEN count = -1;
        ASSERT_SQL_OK(SQLRowCount(stmt_, &count), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, count);
    }

    void NoDiagnostics() {
        SQLTCHAR state[6] = {};
        EXPECT_EQ(SQL_NO_DATA, SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state,
                                             nullptr, nullptr, 0, nullptr));
    }
};

TEST_P(DmlOutputTest, CountsDeferOutputAndReturnStatusUntilFinalMoreResults) {
    Procedure("@v int OUTPUT AS SET NOCOUNT OFF; "
              "UPDATE #dml_output SET v=1; "
              "UPDATE #dml_output SET v=2 WHERE id=1; SET @v=73; RETURN 19");
    SQLINTEGER status = -1, output = -1;
    SQLLEN status_length = -1, output_length = -1;
    BindInt(1, status, status_length);
    BindInt(2, output, output_length);
    ASSERT_SQL_OK(Call("{?=call #dml_outputs(?)}"), SQL_HANDLE_STMT, stmt_);
    Count(2);
    EXPECT_EQ(-1, output);
    EXPECT_EQ(-1, status);
    EXPECT_EQ(-1, output_length);
    EXPECT_EQ(-1, status_length);
    ASSERT_EQ(SQL_SUCCESS, SQLMoreResults(stmt_));
    Count(1);
    EXPECT_EQ(-1, output);
    EXPECT_EQ(-1, status);
    EXPECT_EQ(-1, output_length);
    EXPECT_EQ(-1, status_length);
    ASSERT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    EXPECT_EQ(73, output);
    EXPECT_EQ(19, status);
    EXPECT_EQ(sizeof(SQLINTEGER), output_length);
    EXPECT_EQ(sizeof(SQLINTEGER), status_length);
    output = -2;
    status = -2;
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    EXPECT_EQ(-2, output);
    EXPECT_EQ(-2, status);
}

TEST_P(DmlOutputTest, ZeroAffectedRowsStillDefersOutput) {
    Procedure("@v int OUTPUT AS SET NOCOUNT OFF; "
              "UPDATE #dml_output SET v=1 WHERE id=99; SET @v=73");
    SQLINTEGER output = -1;
    SQLLEN length = -1;
    BindInt(1, output, length);
    ASSERT_EQ(SQL_NO_DATA, Call());
    Count(0);
    EXPECT_EQ(-1, output);
    EXPECT_EQ(-1, length);
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    EXPECT_EQ(73, output);
}

TEST_P(DmlOutputTest, NoCountKeepsImmediateDelivery) {
    Procedure("@v int OUTPUT AS SET NOCOUNT ON; "
              "UPDATE #dml_output SET v=1; SET @v=73; RETURN 19");
    SQLINTEGER status = -1, output = -1;
    SQLLEN status_length = -1, output_length = -1;
    BindInt(1, status, status_length);
    BindInt(2, output, output_length);
    ASSERT_SQL_OK(Call("{?=call #dml_outputs(?)}"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(73, output);
    EXPECT_EQ(19, status);
    EXPECT_EQ(sizeof(SQLINTEGER), output_length);
    EXPECT_EQ(sizeof(SQLINTEGER), status_length);
}

TEST_P(DmlOutputTest, ConversionErrorIsReportedOnceAfterTheCount) {
    Procedure("@v varchar(8) OUTPUT AS SET NOCOUNT OFF; "
              "UPDATE #dml_output SET v=1; SET @v='invalid'");
    SQLINTEGER output = -1;
    SQLLEN length = -1;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_SLONG,
                                  SQL_VARCHAR, 8, 0, &output, 0, &length),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, Call());
    NoDiagnostics();
    Count(2);
    EXPECT_EQ(-1, output);
    EXPECT_EQ(-1, length);
    ASSERT_EQ(SQL_ERROR, SQLMoreResults(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    NoDiagnostics();
}

TEST_P(DmlOutputTest, TruncationWarningIsReportedOnceAfterTheCount) {
    Procedure("@v varchar(8) OUTPUT AS SET NOCOUNT OFF; "
              "UPDATE #dml_output SET v=1; "
              "SET NOCOUNT ON; SELECT 1; SET @v='abcdefgh'");
    SQLCHAR output[4] = "old";
    SQLLEN length = -1;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_CHAR,
                                  SQL_VARCHAR, 8, 0, output, sizeof(output), &length),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, Call());
    NoDiagnostics();
    Count(2);
    EXPECT_STREQ("old", reinterpret_cast<const char*>(output));
    EXPECT_EQ(-1, length);
    ASSERT_EQ(SQL_SUCCESS, SQLMoreResults(stmt_));
    EXPECT_STREQ("old", reinterpret_cast<const char*>(output));
    EXPECT_EQ(-1, length);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE,
                               reinterpret_cast<SQLPOINTER>(2), 0),
                  SQL_HANDLE_STMT, stmt_);
    int warnings = 0;
    auto observe = [&](SQLRETURN rc) {
        if (StmtDiagState().empty()) {
            EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_NO_DATA);
        } else {
            EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
            ++warnings;
        }
    };
    // Drain the tail with a nonempty final rowset so unixODBC also exposes
    // the reference driver's conversion warning instead of hiding it at EOF.
    SQLRETURN rc;
    do {
        rc = SQLFetch(stmt_);
        observe(rc);
    } while (SQL_SUCCEEDED(rc));
    observe(SQLMoreResults(stmt_));
    EXPECT_EQ(1, warnings);
    EXPECT_STREQ("abc", reinterpret_cast<const char*>(output));
    EXPECT_EQ(8, length);
    output[0] = 'x';
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    NoDiagnostics();
    EXPECT_EQ('x', output[0]);
}

TEST_P(DmlOutputTest, ResetBindingsDiscardsPendingOutput) {
    Procedure("@v int OUTPUT AS SET NOCOUNT OFF; "
              "UPDATE #dml_output SET v=1; SET @v=73");
    SQLINTEGER output = -1;
    SQLLEN length = -1;
    BindInt(1, output, length);
    ASSERT_SQL_OK(Call(), SQL_HANDLE_STMT, stmt_);
    Count(2);
    EXPECT_EQ(-1, output);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    EXPECT_EQ(-1, output);
    EXPECT_EQ(-1, length);
}

TEST_P(DmlOutputTest, RebindingRedirectsPendingOutput) {
    Procedure("@v int OUTPUT AS SET NOCOUNT OFF; "
              "UPDATE #dml_output SET v=1; SET @v=73");
    SQLINTEGER old_output = -1, new_output = -2;
    SQLLEN old_length = -1, new_length = -2;
    BindInt(1, old_output, old_length);
    ASSERT_SQL_OK(Call(), SQL_HANDLE_STMT, stmt_);
    Count(2);
    EXPECT_EQ(-1, old_output);
    BindInt(1, new_output, new_length);
    EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
    EXPECT_EQ(-1, old_output);
    EXPECT_EQ(-1, old_length);
    EXPECT_EQ(73, new_output);
    EXPECT_EQ(sizeof(SQLINTEGER), new_length);
}

INSTANTIATE_TEST_SUITE_P(ExecutionRoutes, DmlOutputTest, testing::Bool(),
                        [](const testing::TestParamInfo<bool>& info) {
                            return info.param ? "Prepared" : "Direct";
                        });
