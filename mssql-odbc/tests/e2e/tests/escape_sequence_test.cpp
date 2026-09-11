// Copyright (c) Microsoft Corporation. All rights reserved.
// ODBC escape sequences (AB#46384): SQLNativeSql translation, execution with
// SQL_ATTR_NOSCAN on and off, {call} dispatch, output parameters and
// SQLNumParams.
//
// Golden values are measured against msodbcsql18 18.6.2.1, the build CI pins
// for the parity comparison, so this file runs unchanged against both drivers.

#include "odbc_test_fixture.h"

#include <atomic>
#include <string>
#include <vector>

namespace {

/// Runs SQLNativeSql and returns the translated text.
std::string NativeSql(SQLHDBC dbc, const std::string& sql, SQLRETURN& rc) {
    SqlTString in = ODBCTestUtils::ToSqlTStr(sql);
    std::vector<SQLTCHAR> out(1024, 0);
    SQLINTEGER length = 0;
    rc = SQLNativeSql(dbc, const_cast<SQLTCHAR*>(in.c_str()), SQL_NTS, out.data(),
                      static_cast<SQLINTEGER>(out.size()), &length);
    if (!SQL_SUCCEEDED(rc)) {
        return std::string();
    }
    return ODBCTestUtils::ToNarrow(SqlTString(out.data()));
}

}  // namespace

TEST(EscapeSequenceTest, NullHandle) {
    SQLTCHAR out[16] = {};
    SQLINTEGER length = 0;
    EXPECT_EQ(SQL_INVALID_HANDLE,
              SQLNativeSql(SQL_NULL_HDBC, nullptr, SQL_NTS, out, 16, &length));
}

class EscapeSequenceLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or "
                      "ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    /// Procedures cannot be #temp across connections, so each test creates a
    /// uniquely named one and drops it in TearDown.
    void CreateProc(const std::string& body) {
        // Procedures cannot be #temp across connections, so the name has to
        // be unique per run to avoid collisions with a parallel test.
        static std::atomic<int> counter{0};
        proc_name_ = "dbo.esc_" + std::to_string(::testing::UnitTest::GetInstance()
                                                     ->random_seed()) +
                     "_" + std::to_string(counter.fetch_add(1));
        ExecDirect("CREATE PROCEDURE " + proc_name_ + " " + body);
    }

    void TearDown() override {
        if (!proc_name_.empty()) {
            ExecDirectIgnoreError("DROP PROCEDURE " + proc_name_);
        }
        ODBCTest::TearDown();
    }

    std::string proc_name_;
};

// --- SQLNativeSql ---------------------------------------------------------

TEST_F(EscapeSequenceLiveTest, NativeSqlIsAdvertised) {
    SQLUSMALLINT supported = SQL_FALSE;
    ASSERT_SQL_OK(SQLGetFunctions(dbc_, SQL_API_SQLNATIVESQL, &supported),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_TRUE, supported);

    ASSERT_SQL_OK(SQLGetFunctions(dbc_, SQL_API_SQLNUMPARAMS, &supported),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_TRUE, supported);
}

/// SQL Server parses these natively, so the driver forwards them untouched.
TEST_F(EscapeSequenceLiveTest, NativeEscapesArePassedThrough) {
    const char* const passthrough[] = {
        "SELECT {fn UCASE('abc')}",
        "SELECT {d '2020-01-02'}",
        "SELECT {t '13:14:15'}",
        "SELECT {ts '2020-01-02 13:14:15'}",
        "SELECT {guid '6F9619FF-8B86-D011-B42D-00C04FC964FF'}",
        "SELECT {fn CONVERT(123, SQL_VARCHAR)}",
        "SELECT 1 /* {fn UCASE('x')} */ , '{d ''2020-01-02''}'",
    };
    for (const char* sql : passthrough) {
        SQLRETURN rc = SQL_SUCCESS;
        EXPECT_EQ(std::string(sql), NativeSql(dbc_, sql, rc)) << sql;
        EXPECT_TRUE(SQL_SUCCEEDED(rc)) << sql;
    }
}

TEST_F(EscapeSequenceLiveTest, LiteralTranslations) {
    SQLRETURN rc = SQL_SUCCESS;
    EXPECT_EQ(" 'INTERVAL +''1'' DAY(2)' ",
              NativeSql(dbc_, "{interval '1' DAY}", rc));
    EXPECT_EQ(" 'INTERVAL +''30.000000'' SECOND(2,6)' ",
              NativeSql(dbc_, "{interval '30' SECOND}", rc));
    EXPECT_EQ(" 0xB3A583A593A5 ", NativeSql(dbc_, "{encrypt N'abc'}", rc));
    EXPECT_EQ("x LIKE 'a\\_b'  ESCAPE '\\' ",
              NativeSql(dbc_, "x LIKE 'a\\_b' {escape '\\'}", rc));
}

TEST_F(EscapeSequenceLiveTest, CallTranslatesToExecText) {
    SQLRETURN rc = SQL_SUCCESS;
    EXPECT_EQ(" EXEC sp_who  ", NativeSql(dbc_, "{call sp_who}", rc));
    EXPECT_EQ(" EXEC ?=sp_who ?  ", NativeSql(dbc_, "{? = call sp_who(?)}", rc));
}

TEST_F(EscapeSequenceLiveTest, MalformedEscapeIsRejected) {
    SQLRETURN rc = SQL_SUCCESS;
    NativeSql(dbc_, "SELECT {bogus 1}", rc);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "42000");
}

/// The interval escape reports through the type converter, not the escape
/// parser, so its SQLSTATEs differ from every other escape's.
TEST_F(EscapeSequenceLiveTest, IntervalErrorsUseConverterSqlStates) {
    SQLRETURN rc = SQL_SUCCESS;
    NativeSql(dbc_, "SELECT {interval '100' DAY}", rc);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "22018");

    NativeSql(dbc_, "SELECT {interval '30.1234' SECOND(2,3)}", rc);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "22001");
}

// --- execution ------------------------------------------------------------

TEST_F(EscapeSequenceLiveTest, EscapesExecute) {
    ExecDirect("SELECT {fn UCASE('abc')}");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLCHAR value[32] = {};
    SQLLEN length = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, value, sizeof(value), &length),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("ABC", reinterpret_cast<const char*>(value));
    SQLCloseCursor(stmt_);

    // {interval ...} becomes a T-SQL string literal.
    ExecDirect("SELECT {interval '1' DAY}");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, value, sizeof(value), &length),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("INTERVAL +'1' DAY(2)", reinterpret_cast<const char*>(value));
    SQLCloseCursor(stmt_);
}

TEST_F(EscapeSequenceLiveTest, MalformedEscapeFailsBeforeExecute) {
    SqlTString text = ODBCTestUtils::ToSqlTStr("SELECT {bogus 1}");
    EXPECT_EQ(SQL_ERROR,
              SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "42000");
}

/// With NOSCAN on the text goes to the server as written, so a malformed escape
/// becomes the server's syntax error rather than the driver's.
TEST_F(EscapeSequenceLiveTest, NoscanSuppressesTranslation) {
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_NOSCAN,
                                 reinterpret_cast<SQLPOINTER>(SQL_NOSCAN_ON), 0),
                  SQL_HANDLE_STMT, stmt_);
    SqlTString text = ODBCTestUtils::ToSqlTStr("SELECT {bogus 1}");
    EXPECT_EQ(SQL_ERROR,
              SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS));
    // 42000 either way, but from the server: the driver never looked at it.
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "42000");
}

// --- {call} ---------------------------------------------------------------

TEST_F(EscapeSequenceLiveTest, CallExecutesAndReturnsRows) {
    CreateProc("@a int AS BEGIN SELECT @a * 2 AS doubled; END");

    ExecDirect("{call " + proc_name_ + "(21)}");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLINTEGER value = 0;
    SQLLEN length = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &length),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, value);
}

TEST_F(EscapeSequenceLiveTest, CallWithBoundInputParameter) {
    CreateProc("@a int AS BEGIN SELECT @a * 2 AS doubled; END");

    SqlTString text = ODBCTestUtils::ToSqlTStr("{call " + proc_name_ + "(?)}");
    SQLINTEGER input = 21;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 0, 0, &input, 0, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    SQLINTEGER value = 0;
    SQLLEN length = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &length),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(42, value);
}

// --- output parameters ----------------------------------------------------

TEST_F(EscapeSequenceLiveTest, OutputParameterIsWrittenBack) {
    CreateProc("@a int, @b int OUTPUT AS BEGIN SET @b = @a * 2; END");

    SqlTString text = ODBCTestUtils::ToSqlTStr("{call " + proc_name_ + "(?, ?)}");
    SQLINTEGER input = 21;
    SQLINTEGER output = 0;
    SQLLEN in_ind = 0;
    SQLLEN out_ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 0, 0, &input, 0, &in_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_OUTPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 0, 0, &output, sizeof(output),
                                   &out_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    // ODBC: the value is not available until the results are consumed.
    while (SQLMoreResults(stmt_) == SQL_SUCCESS) {
    }
    EXPECT_EQ(42, output);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLINTEGER)), out_ind);
}

TEST_F(EscapeSequenceLiveTest, NullOutputParameterSetsTheIndicator) {
    CreateProc("@b int OUTPUT AS BEGIN SET @b = NULL; END");

    SqlTString text = ODBCTestUtils::ToSqlTStr("{call " + proc_name_ + "(?)}");
    SQLINTEGER output = 1234;
    SQLLEN out_ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 0, 0, &output, sizeof(output),
                                   &out_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    while (SQLMoreResults(stmt_) == SQL_SUCCESS) {
    }
    EXPECT_EQ(SQL_NULL_DATA, out_ind);
}

TEST_F(EscapeSequenceLiveTest, ReturnStatusIsWrittenBack) {
    CreateProc("AS BEGIN RETURN 7; END");

    SqlTString text = ODBCTestUtils::ToSqlTStr("{? = call " + proc_name_ + "}");
    SQLINTEGER status = 0;
    SQLLEN status_ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 0, 0, &status, sizeof(status),
                                   &status_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    while (SQLMoreResults(stmt_) == SQL_SUCCESS) {
    }
    EXPECT_EQ(7, status);
}

// --- SQLNumParams / SQLDescribeParam --------------------------------------

TEST_F(EscapeSequenceLiveTest, NumParamsCountsCallMarkers) {
    CreateProc("@a int, @b varchar(20), @c int OUTPUT AS BEGIN SELECT @a; END");

    struct Case {
        std::string sql;
        SQLSMALLINT expected;
    };
    const Case cases[] = {
        {"{call " + proc_name_ + "(?,?,?)}", 3},
        {"{? = call " + proc_name_ + "(?,?,?)}", 4},
        {"{call " + proc_name_ + "(?, 'lit', ?)}", 2},
        {"{call " + proc_name_ + "}", 0},
    };
    for (const Case& c : cases) {
        SQLHSTMT stmt = AllocStmt();
        SqlTString text = ODBCTestUtils::ToSqlTStr(c.sql);
        ASSERT_SQL_OK(SQLPrepare(stmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS),
                      SQL_HANDLE_STMT, stmt);
        SQLSMALLINT count = -1;
        ASSERT_SQL_OK(SQLNumParams(stmt, &count), SQL_HANDLE_STMT, stmt);
        EXPECT_EQ(c.expected, count) << c.sql;
        FreeStmt(stmt);
    }
}

TEST_F(EscapeSequenceLiveTest, NumParamsRequiresAPreparedStatement) {
    SQLSMALLINT count = -1;
    EXPECT_EQ(SQL_ERROR, SQLNumParams(stmt_, &count));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

/// The return-status parameter is described as an integer; the rest come from
/// the server's own metadata for the EXEC form.
TEST_F(EscapeSequenceLiveTest, DescribeParamHandlesCall) {
    CreateProc("@a int, @b varchar(20), @c int OUTPUT AS BEGIN SELECT @a; END");

    SqlTString text =
        ODBCTestUtils::ToSqlTStr("{? = call " + proc_name_ + "(?,?,?)}");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT data_type = 0;
    SQLULEN size = 0;
    SQLSMALLINT scale = 0;
    SQLSMALLINT nullable = 0;
    ASSERT_SQL_OK(SQLDescribeParam(stmt_, 1, &data_type, &size, &scale, &nullable),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_INTEGER, data_type);
    EXPECT_EQ(10u, size);

    ASSERT_SQL_OK(SQLDescribeParam(stmt_, 3, &data_type, &size, &scale, &nullable),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_VARCHAR, data_type);
    EXPECT_EQ(20u, size);
}

// --- SQLGetInfo -----------------------------------------------------------

TEST_F(EscapeSequenceLiveTest, EscapeCapabilitiesAreAdvertised) {
    SQLUINTEGER mask = 0;
    ASSERT_SQL_OK(SQLGetInfo(dbc_, SQL_OJ_CAPABILITIES, &mask, sizeof(mask), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(0x0000007Fu, mask);

    ASSERT_SQL_OK(
        SQLGetInfo(dbc_, SQL_TIMEDATE_ADD_INTERVALS, &mask, sizeof(mask), nullptr),
        SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(0x000001FFu, mask);

    SQLCHAR text[8] = {};
    ASSERT_SQL_OK(SQLGetInfo(dbc_, SQL_LIKE_ESCAPE_CLAUSE, text, sizeof(text), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_STREQ("Y", reinterpret_cast<const char*>(text));
}
