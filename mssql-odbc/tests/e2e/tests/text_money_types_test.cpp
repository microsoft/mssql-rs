// Copyright (c) Microsoft Corporation. All rights reserved.
// text_money_types_test.cpp  –  E2E coverage for money and the character sources.
//
// Two P5 gaps in one place.
//
// Money renders through its own formatter, separate from the decimal path, so
// the sub-one leading-zero rule that decimals were just aligned on has to be
// checked here independently rather than assumed to follow.
//
// The character targets showed coverage in the audit, but the audit counted C
// targets and not source types: SQL_C_CHAR and SQL_C_WCHAR appear in many tests
// without VARCHAR, NVARCHAR and XML each being exercised through them. These
// pin the source side.

#include "odbc_test_fixture.h"

#include <string>

class TextMoneyTypesLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLRETURN ExecDirect(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // Renders column 1 of `sql` through SQLGetData as narrow text.
    std::string TextOf(const std::string& sql) {
        EXPECT_SQL_OK(ExecDirect(sql), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        char buf[64] = {};
        SQLLEN ind = 0;
        EXPECT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                      stmt_);
        SQLCloseCursor(stmt_);
        return std::string(buf);
    }
};

// ---------------------------------------------------------------------------
// MONEY / SMALLMONEY as text
// ---------------------------------------------------------------------------

// Money has its own formatter, so the sub-one rule is verified here rather than
// inherited from the decimal path.
TEST_F(TextMoneyTypesLiveTest, MoneySubOneRendering) {
    EXPECT_EQ(".5000", TextOf("SELECT CAST(0.5 AS MONEY) AS c1"));
    EXPECT_EQ("-.5000", TextOf("SELECT CAST(-0.5 AS MONEY) AS c1"));
    EXPECT_EQ(".0001", TextOf("SELECT CAST(0.0001 AS MONEY) AS c1"));
}

// At or above one the leading digit is real, so nothing is stripped. Exact zero
// is stripped too -- msodbcsql applies the rule unconditionally rather than
// treating zero as a special case.
TEST_F(TextMoneyTypesLiveTest, MoneyAtOrAboveOneKeepsItsDigits) {
    EXPECT_EQ("1.5000", TextOf("SELECT CAST(1.5 AS MONEY) AS c1"));
    EXPECT_EQ("-1.5000", TextOf("SELECT CAST(-1.5 AS MONEY) AS c1"));
    EXPECT_EQ(".0000", TextOf("SELECT CAST(0 AS MONEY) AS c1"));
}

// The range ends, where money's 64-bit scaled representation is closest to
// overflowing on the way to text.
TEST_F(TextMoneyTypesLiveTest, MoneyRangeEnds) {
    EXPECT_EQ("922337203685477.5807", TextOf("SELECT CAST(922337203685477.5807 AS MONEY) AS c1"));
    EXPECT_EQ("-922337203685477.5808", TextOf("SELECT CAST(-922337203685477.5808 AS MONEY) AS c1"));
}

TEST_F(TextMoneyTypesLiveTest, SmallmoneyRendering) {
    EXPECT_EQ(".5000", TextOf("SELECT CAST(0.5 AS SMALLMONEY) AS c1"));
    EXPECT_EQ("214748.3647", TextOf("SELECT CAST(214748.3647 AS SMALLMONEY) AS c1"));
}

TEST_F(TextMoneyTypesLiveTest, MoneyToCharViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0.5 AS MONEY) AS c1"), SQL_HANDLE_STMT, stmt_);

    char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ(".5000", buf) << "the bound path must render money the same way";
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// Character sources. The audit counted C targets, not source types.
// ---------------------------------------------------------------------------

TEST_F(TextMoneyTypesLiveTest, VarcharToCharViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('hello' AS VARCHAR(20)) AS c1"), SQL_HANDLE_STMT, stmt_);

    char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("hello", buf);
    EXPECT_EQ(5, ind);
    SQLCloseCursor(stmt_);
}

// NVARCHAR carrying non-ASCII, so a UTF-16 to UTF-8 mistake cannot pass. The
// indicator is in bytes, not characters, which is the other half of the trap.
//
// The character is built with NCHAR(233) rather than written into the literal:
// a \\u escape in a narrow C++ string lands in the query text as UTF-8 bytes,
// so the test would be asserting against mangled SQL rather than the driver.
TEST_F(TextMoneyTypesLiveTest, NvarcharNonAsciiToWcharViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(N'caf' + NCHAR(233) AS NVARCHAR(20)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);

    SQLWCHAR buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(u'c', buf[0]);
    EXPECT_EQ(u'a', buf[1]);
    EXPECT_EQ(u'f', buf[2]);
    EXPECT_EQ(0x00E9u, static_cast<unsigned>(buf[3])) << "e-acute survives as one UTF-16 unit";
    EXPECT_EQ(0, buf[4]);
    EXPECT_EQ(8, ind) << "the indicator counts bytes, not characters";
    SQLCloseCursor(stmt_);
}

TEST_F(TextMoneyTypesLiveTest, NvarcharNonAsciiToWcharViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(N'caf' + NCHAR(233) AS NVARCHAR(20)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(0x00E9u, static_cast<unsigned>(buf[3]));
    EXPECT_EQ(8, ind);
    SQLCloseCursor(stmt_);
}

// XML is a distinct source type that renders as text; the map routes it to
// SQL_C_WCHAR. Covered through SQLGetData only: XML is a max/PLP type, and
// delivering PLP columns into a bound buffer is not implemented yet (Task
// 47361). msodbcsql does deliver it, so a bound version of this test would sit
// in the suite as a permanent divergence rather than a useful assertion -- it
// belongs with 47361.
TEST_F(TextMoneyTypesLiveTest, XmlToWcharViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('<a b=\"1\"/>' AS XML) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);

    EXPECT_EQ(u'<', buf[0]);
    EXPECT_EQ(u'a', buf[1]);
    EXPECT_GT(ind, 0);
    SQLCloseCursor(stmt_);
}
