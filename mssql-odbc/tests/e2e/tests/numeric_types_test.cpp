// Copyright (c) Microsoft Corporation. All rights reserved.
// numeric_types_test.cpp  –  E2E coverage for the numeric, bit and GUID targets.
//
// The P5 audit found these untested end to end: SQL_C_SBIGINT, SQL_C_BIT,
// SQL_C_FLOAT, SQL_C_DOUBLE and SQL_C_GUID had no e2e test on either path, and
// SQL_C_TINYINT and SQL_C_SSHORT were covered only through SQLGetData.
//
// Scope is the fetch type map: the SQL type -> C type pairs mssql-python
// actually requests. SQL_C_NUMERIC is deliberately absent — mssql-python fetches
// DECIMAL/NUMERIC as SQL_C_CHAR and parses it, and SQL_C_NUMERIC input binding
// is an explicit scope boundary of this plan.
//
// Values are chosen to fail loudly rather than pass by luck: range ends for the
// integers, a fraction no float can hold exactly for REAL, and a GUID whose
// bytes are all distinct so the mixed-endian SQLGUID layout cannot come back
// looking correct.

#include "odbc_test_fixture.h"

#include <cstring>
#include <limits>
#include <string>

class NumericTypesLiveTest : public ODBCTest {
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
};

// ---------------------------------------------------------------------------
// Integers. TINYINT is unsigned 0..255 on the server, so a value above 127 is
// the interesting one: SQL_C_TINYINT is sign-ambiguous rather than signed, and
// for a tinyint source it copies the byte through without a range check, which
// is how 200 survives. Narrowing to i8 -- and the 22003 that 200 would produce
// -- applies to SQL_C_STINYINT and to any other source. See fetch_convert.rs.
// ---------------------------------------------------------------------------

TEST_F(NumericTypesLiveTest, TinyintToTinyintTargetViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(200 AS TINYINT) AS c1"), SQL_HANDLE_STMT, stmt_);

    SQLCHAR v = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_TINYINT, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(200, static_cast<int>(v));
    EXPECT_EQ(1, ind);
    SQLCloseCursor(stmt_);
}

TEST_F(NumericTypesLiveTest, SmallintToSshortTargetViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(-32768 AS SMALLINT) AS c1"), SQL_HANDLE_STMT, stmt_);

    SQLSMALLINT v = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SSHORT, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(-32768, v) << "SMALLINT lower bound";
    SQLCloseCursor(stmt_);
}

// BIGINT at its lower bound: the one value that cannot be negated, so a
// sign-handling bug in the decode cannot round-trip it by accident.
TEST_F(NumericTypesLiveTest, BigintMinViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(-9223372036854775808 AS BIGINT) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLBIGINT v = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SBIGINT, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ((std::numeric_limits<SQLBIGINT>::min)(), v);
    EXPECT_EQ(8, ind);
    SQLCloseCursor(stmt_);
}

TEST_F(NumericTypesLiveTest, BigintMaxViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(9223372036854775807 AS BIGINT) AS c1"), SQL_HANDLE_STMT,
                  stmt_);

    SQLBIGINT v = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SBIGINT, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ((std::numeric_limits<SQLBIGINT>::max)(), v);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// BIT — both values, because a truthiness bug only shows on one of them
// ---------------------------------------------------------------------------

TEST_F(NumericTypesLiveTest, BitBothValuesViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(1 AS BIT) AS t, CAST(0 AS BIT) AS f"), SQL_HANDLE_STMT,
                  stmt_);

    SQLCHAR t = 0xFF, f = 0xFF;
    SQLLEN tInd = 0, fInd = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_BIT, &t, sizeof(t), &tInd), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 2, SQL_C_BIT, &f, sizeof(f), &fInd), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, static_cast<int>(t));
    EXPECT_EQ(0, static_cast<int>(f));
    SQLCloseCursor(stmt_);
}

TEST_F(NumericTypesLiveTest, BitViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(1 AS BIT) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR v = 0xFF;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BIT, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, static_cast<int>(v));
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// REAL / FLOAT. 0.1 has no exact binary representation, so a target-width
// mistake (reading a REAL as a double or vice versa) cannot round-trip it.
// ---------------------------------------------------------------------------

TEST_F(NumericTypesLiveTest, RealToFloatTargetViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0.1 AS REAL) AS c1"), SQL_HANDLE_STMT, stmt_);

    SQLREAL v = 0.0f;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_FLOAT, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_FLOAT_EQ(0.1f, v);
    EXPECT_EQ(4, ind);
    SQLCloseCursor(stmt_);
}

TEST_F(NumericTypesLiveTest, RealToFloatTargetViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0.1 AS REAL) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLREAL v = 0.0f;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_FLOAT, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_FLOAT_EQ(0.1f, v);
    SQLCloseCursor(stmt_);
}

TEST_F(NumericTypesLiveTest, FloatToDoubleTargetViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0.1 AS FLOAT) AS c1"), SQL_HANDLE_STMT, stmt_);

    SQLDOUBLE v = 0.0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_DOUBLE, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_DOUBLE_EQ(0.1, v);
    EXPECT_EQ(8, ind);
    SQLCloseCursor(stmt_);
}

TEST_F(NumericTypesLiveTest, FloatToDoubleTargetViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0.1 AS FLOAT) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLDOUBLE v = 0.0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_DOUBLE, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_DOUBLE_EQ(0.1, v);
    SQLCloseCursor(stmt_);
}

// Float rendered as *text*, which is a different code path from the binary
// targets above and the one place the sub-one leading zero is deliberately
// kept: msodbcsql's DoubleToChar writes it, while its decimal and money paths
// strip it. Asserted here so the asymmetry is verified rather than only
// described in a comment.
TEST_F(NumericTypesLiveTest, FloatToCharKeepsItsLeadingZero) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0.5 AS FLOAT) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("0.5", buf) << "unlike DECIMAL, float keeps the leading zero";
    SQLCloseCursor(stmt_);
}

TEST_F(NumericTypesLiveTest, RealToCharKeepsItsLeadingZero) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(0.5 AS REAL) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("0.5", buf);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// GUID. SQLGUID is mixed-endian: Data1/2/3 are little-endian integers and Data4
// is a byte array, so a driver that memcpy's the wire bytes straight through
// produces a plausible-looking but wrong value. Every byte here is distinct so
// that mistake cannot survive.
// ---------------------------------------------------------------------------

TEST_F(NumericTypesLiveTest, UniqueidentifierToGuidTargetViaBoundFetch) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST('01020304-0506-0708-090A-0B0C0D0E0F10' AS UNIQUEIDENTIFIER) AS c1"),
        SQL_HANDLE_STMT, stmt_);

    SQLGUID g{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_GUID, &g, sizeof(g), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(0x01020304u, g.Data1);
    EXPECT_EQ(0x0506u, g.Data2);
    EXPECT_EQ(0x0708u, g.Data3);
    const unsigned char expected[8] = {0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10};
    EXPECT_EQ(0, std::memcmp(g.Data4, expected, sizeof(expected)));
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQLGUID)), ind);
    SQLCloseCursor(stmt_);
}

TEST_F(NumericTypesLiveTest, UniqueidentifierToGuidTargetViaGetData) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST('01020304-0506-0708-090A-0B0C0D0E0F10' AS UNIQUEIDENTIFIER) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLGUID g{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_GUID, &g, sizeof(g), &ind), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(0x01020304u, g.Data1);
    EXPECT_EQ(0x0506u, g.Data2);
    EXPECT_EQ(0x0708u, g.Data3);
    const unsigned char expected[8] = {0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10};
    EXPECT_EQ(0, std::memcmp(g.Data4, expected, sizeof(expected)));
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// DECIMAL as text. This is the pair the type map actually specifies for
// decimals — mssql-python asks for SQL_C_CHAR and parses it into
// decimal.Decimal — so the rendering has to keep the declared scale rather than
// normalising it away.
// ---------------------------------------------------------------------------

TEST_F(NumericTypesLiveTest, DecimalToCharKeepsScaleViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(123.4500 AS DECIMAL(10,4)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);

    char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("123.4500", buf) << "trailing zeros are significant at DECIMAL(10,4)";
    SQLCloseCursor(stmt_);
}

// A negative decimal, since the sign is carried separately from the digits on
// the wire.
TEST_F(NumericTypesLiveTest, NegativeDecimalToCharViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(-0.0001 AS DECIMAL(10,4)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    char buf[32] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("-.0001", buf);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// NULL with no indicator. A fixed-width target has no in-band way to say NULL,
// so with nowhere to report it the only safe answer is to refuse — otherwise
// the caller's buffer reads back as whatever it happened to hold before.
// ---------------------------------------------------------------------------

TEST_F(NumericTypesLiveTest, NullWithNoIndicatorViaGetDataIs22002) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS INT) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER v = 7;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22002");
    EXPECT_EQ(7, v) << "the stale value stays visible rather than passing as data";
    SQLCloseCursor(stmt_);
}

// The bound path already refuses; pinning both together keeps them from
// drifting apart again.
TEST_F(NumericTypesLiveTest, NullWithNoIndicatorViaBoundFetchIs22002) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS INT) AS c1"), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER v = 7;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), nullptr), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22002");
    EXPECT_EQ(7, v) << "the stale value stays visible rather than passing as data";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// A character target cannot express NULL in band either — an empty string is a
// value — so the refusal is not specific to fixed-width targets.
TEST_F(NumericTypesLiveTest, NullWithNoIndicatorCharTargetIs22002) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS VARCHAR(10)) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    char buf[16] = "stale";
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22002");
    SQLCloseCursor(stmt_);
}
