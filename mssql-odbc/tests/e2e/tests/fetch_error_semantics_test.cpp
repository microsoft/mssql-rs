// Copyright (c) Microsoft Corporation. All rights reserved.
// fetch_error_semantics_test.cpp  –  E2E parity for the fetch path's *error*
// semantics: 22003 (out of range), 01S07 (fractional truncation), 07006
// (restricted conversion) and 22018 (unconvertible character data).
//
// AB#47678. The P5 type-coverage work proved the fetch type map is wired up,
// but the range and truncation rules were unit-tested only: a grep for these
// SQLSTATEs across the e2e suite hit the *parameter* direction in
// execute_test.cpp and almost nothing on the fetch path. These are the rules an
// application actually keys on to tell "no value" from "bad value", so they are
// worth pinning against msodbcsql rather than against our own reading.
//
// Expected values are taken from msodbcsql's own conversion suite
// (ODBCGEN/Tests/Cnvrsnpp) where an equivalent cell exists, but note that
// Cnvrsnpp drives *character* sources: these tests fetch real typed columns, a
// different path, so every case here is confirmed against both drivers rather
// than assumed to follow.

#include "odbc_test_fixture.h"

#include <cstring>
#include <string>

#ifndef SQL_C_SS_TIME2
#define SQL_C_TYPES_EXTENDED 0x04000L
#define SQL_C_SS_TIME2 (SQL_C_TYPES_EXTENDED + 0)

typedef struct tagSS_TIME2_STRUCT {
    SQLUSMALLINT hour;
    SQLUSMALLINT minute;
    SQLUSMALLINT second;
    SQLUINTEGER fraction;
} SQL_SS_TIME2_STRUCT;
#endif

static_assert(sizeof(SQL_SS_TIME2_STRUCT) == 12,
              "SQL_SS_TIME2_STRUCT layout does not match the msodbcsql ABI");

class FetchErrorSemanticsLiveTest : public ODBCTest {
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

    // Executes |sql| and positions on its single row.
    void FetchOne(const std::string& sql) {
        ASSERT_SQL_OK(ExecDirect(sql), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    }
};

// ---------------------------------------------------------------------------
// 22003 — the value does not fit the target's range.
//
// These assert the *value buffer* is left alone, not the indicator. The
// indicator is deliberately not asserted: ODBC leaves it undefined when the
// call fails, and the two drivers genuinely differ — msodbcsql writes 0, this
// driver leaves the caller's value in place. Pinning either would encode
// something an application must not read.
// ---------------------------------------------------------------------------

TEST_F(FetchErrorSemanticsLiveTest, BigintAboveSlongRangeIsOutOfRange) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST(2147483648 AS BIGINT) AS c1"));

    SQLINTEGER v = 4242;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
    EXPECT_EQ(4242, v) << "a range failure must not write a partial value";
    SQLCloseCursor(stmt_);
}

// The same value fits an unsigned target of the same width, so it succeeds --
// the pair is what shows the range check is signedness-aware.
TEST_F(FetchErrorSemanticsLiveTest, BigintAboveSlongRangeFitsUlong) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST(2147483648 AS BIGINT) AS c1"));

    SQLUINTEGER v = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_ULONG, &v, sizeof(v), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2147483648u, v);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, IntAboveSignedByteRangeIsOutOfRange) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST(128 AS INT) AS c1"));

    signed char v = 77;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_STINYINT, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
    EXPECT_EQ(77, static_cast<int>(v)) << "a range failure must not write a partial value";
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, IntAboveSignedByteRangeFitsUtinyint) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST(128 AS INT) AS c1"));

    unsigned char v = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_UTINYINT, &v, sizeof(v), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(128, static_cast<int>(v));
    SQLCloseCursor(stmt_);
}

// A negative value into any unsigned target is out of range, however small.
TEST_F(FetchErrorSemanticsLiveTest, NegativeIntoUnsignedIsOutOfRange) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST(-1 AS INT) AS c1"));

    SQLUINTEGER v = 31337;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_ULONG, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
    EXPECT_EQ(31337u, v) << "a range failure must not write a partial value";
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// 01S07 — the value fits, but a fractional part had to be dropped. This is a
// *success* return with a warning, so the distinction from 22003 is the whole
// point: the caller keeps the truncated value.
// ---------------------------------------------------------------------------

TEST_F(FetchErrorSemanticsLiveTest, FloatFractionIntoIntegerTargetTruncates) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST(1234.99 AS FLOAT) AS c1"));

    SQLINTEGER v = 0;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S07");
    EXPECT_EQ(1234, v) << "truncation is toward zero, not rounding";
    SQLCloseCursor(stmt_);
}

// Truncating toward zero puts this in range for an unsigned target, so it is a
// truncation rather than a range error even though the source is negative.
TEST_F(FetchErrorSemanticsLiveTest, SmallNegativeFractionIntoUnsignedTruncates) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST(-0.01 AS FLOAT) AS c1"));

    unsigned char v = 9;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_UTINYINT, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S07");
    EXPECT_EQ(0, static_cast<int>(v));
    SQLCloseCursor(stmt_);
}

// SQL_C_BIT is carved out of that rule: msodbcsql's own table flags this cell
// "this is treated specially" and answers 22003 where the unsigned integers
// answer 01S07.
TEST_F(FetchErrorSemanticsLiveTest, SmallNegativeFractionIntoBitIsOutOfRange) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST(-0.01 AS FLOAT) AS c1"));

    unsigned char v = 9;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_BIT, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
    EXPECT_EQ(9, static_cast<int>(v)) << "a range failure must not write a partial value";
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// 07006 — the source and target are not a legal pairing at all. Distinct from
// 22018, which means the pairing is legal but the data is not convertible.
// ---------------------------------------------------------------------------

TEST_F(FetchErrorSemanticsLiveTest, DateIntoNumericTargetIsRestricted) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST('1996-01-01' AS DATE) AS c1"));

    SQLINTEGER v = 0;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07006");
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, UnconvertibleCharacterDataIsInvalidCharacterValue) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST('abc' AS VARCHAR(10)) AS c1"));

    SQLINTEGER v = 4242;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_SLONG, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
    EXPECT_EQ(4242, v) << "a conversion failure must not write a partial value";
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, DatetimeIntoFloatTargetIsRestricted) {
    ASSERT_NO_FATAL_FAILURE(
        FetchOne("SELECT CAST('1996-01-01 12:00:00' AS DATETIME) AS c1"));

    double v = 0;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_DOUBLE, &v, sizeof(v), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07006");
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// Dropping a non-zero time component when narrowing to a date target is a
// truncation, and the zero-time case is the contrast that shows it is the time
// that triggers it rather than the narrowing itself.
// ---------------------------------------------------------------------------

TEST_F(FetchErrorSemanticsLiveTest, DatetimeWithTimeIntoDateTargetTruncates) {
    ASSERT_NO_FATAL_FAILURE(
        FetchOne("SELECT CAST('1996-01-01 01:00:00.010' AS DATETIME) AS c1"));

    SQL_DATE_STRUCT d{};
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_TYPE_DATE, &d, sizeof(d), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S07");
    EXPECT_EQ(1996, d.year);
    EXPECT_EQ(1, d.month);
    EXPECT_EQ(1, d.day);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, DatetimeWithZeroTimeIntoDateTargetIsClean) {
    ASSERT_NO_FATAL_FAILURE(
        FetchOne("SELECT CAST('1996-01-01 00:00:00' AS DATETIME) AS c1"));

    SQL_DATE_STRUCT d{};
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_TYPE_DATE, &d, sizeof(d), &ind));
    EXPECT_EQ(1996, d.year);
    EXPECT_EQ(1, d.month);
    EXPECT_EQ(1, d.day);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, DatetimeoffsetIntoSsTime2Succeeds) {
    ASSERT_NO_FATAL_FAILURE(FetchOne(
        "SELECT CAST('2023-01-01 12:34:56.1234567 +05:30' AS DATETIMEOFFSET(7)) AS c1"));

    SQL_SS_TIME2_STRUCT t{};
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_SS_TIME2, &t, sizeof(t), &ind));
    EXPECT_LE(t.hour, 23);
    EXPECT_LE(t.minute, 59);
    EXPECT_EQ(56, t.second);
    EXPECT_EQ(123456700u, t.fraction);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(t)), ind);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// 01004 — the value was delivered but did not fit. The indicator carries the
// *untruncated* length, which is how a caller sizes a second buffer, and the
// boundary is the interesting part: a buffer exactly the size of the data is
// one byte short once the NUL is accounted for.
// ---------------------------------------------------------------------------

TEST_F(FetchErrorSemanticsLiveTest, ABufferOneByteShortTruncatesAndReportsFullLength) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST('abcdefghij' AS VARCHAR(10)) AS c1"));

    char buf[10] = {};  // 10 data bytes need 11 with the terminator
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_STREQ("abcdefghi", buf) << "nine characters plus the terminator";
    EXPECT_EQ(10, ind) << "the indicator reports the untruncated length";
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, ABufferThatExactlyFitsSucceeds) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST('abcdefghij' AS VARCHAR(10)) AS c1"));

    char buf[11] = {};  // 10 data bytes + terminator
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind));
    EXPECT_STREQ("abcdefghij", buf);
    EXPECT_EQ(10, ind);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// A conversion failure as the per-row error inside a block fetch. The
// mechanism is already covered via 22002; this is the same path reached by a
// range error instead, with the rest of the rowset still delivering.
// ---------------------------------------------------------------------------

TEST_F(FetchErrorSemanticsLiveTest, ARangeErrorIsARowErrorInABlockFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(1 AS INT) AS c1 UNION ALL SELECT CAST(300 AS INT) "
                            "ORDER BY c1"),
                  SQL_HANDLE_STMT, stmt_);

    signed char v[2] = {42, 42};
    SQLUSMALLINT status[2] = {SQL_ROW_NOROW, SQL_ROW_NOROW};
    SQLLEN ind[2] = {0, 0};
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_ARRAY_SIZE, reinterpret_cast<SQLPOINTER>(2),
                                 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROW_STATUS_PTR, status, 0), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_STINYINT, v, sizeof(signed char), ind),
                  SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLFetchScroll(stmt_, SQL_FETCH_NEXT, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
    EXPECT_EQ(1, static_cast<int>(v[0])) << "the in-range row still delivers";
    EXPECT_EQ(SQL_ROW_SUCCESS, status[0]);
    EXPECT_EQ(42, static_cast<int>(v[1])) << "a range failure must not write a partial value";
    EXPECT_EQ(SQL_ROW_ERROR, status[1]) << "300 does not fit a signed byte";
    SQLFreeStmt(stmt_, SQL_UNBIND);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// uniqueidentifier rendered as text. Neither msodbcsql's conversion suite nor
// its GetData tests pin the braces or the case, so this is a live parity
// question rather than a ported assertion -- the same shape as the decimal
// rendering divergence, which no unit test could have reached.
// ---------------------------------------------------------------------------

TEST_F(FetchErrorSemanticsLiveTest, GuidRendersAsUppercaseWithoutBracesViaGetDataChar) {
    ASSERT_NO_FATAL_FAILURE(
        FetchOne("SELECT CAST('0123ABCD-4567-89EF-0123-456789ABCDEF' AS UNIQUEIDENTIFIER) AS c1"));

    char buf[64] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_STREQ("0123ABCD-4567-89EF-0123-456789ABCDEF", buf);
    EXPECT_EQ(36, ind);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, GuidRendersAsUppercaseWithoutBracesViaGetDataWchar) {
    ASSERT_NO_FATAL_FAILURE(
        FetchOne("SELECT CAST('0123ABCD-4567-89EF-0123-456789ABCDEF' AS UNIQUEIDENTIFIER) AS c1"));

    SQLWCHAR buf[37] = {};
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_SUCCESS, SQLGetData(stmt_, 1, SQL_C_WCHAR, buf, sizeof(buf), &ind));
    constexpr char expected[] = "0123ABCD-4567-89EF-0123-456789ABCDEF";
    for (size_t i = 0; i < sizeof(expected); ++i) {
        EXPECT_EQ(static_cast<SQLWCHAR>(expected[i]), buf[i]);
    }
    EXPECT_EQ(72, ind);
    SQLCloseCursor(stmt_);
}

TEST_F(FetchErrorSemanticsLiveTest, GuidRendersAsUppercaseWithoutBracesViaBoundFetchChar) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST('0123ABCD-4567-89EF-0123-456789ABCDEF' AS UNIQUEIDENTIFIER) AS c1"),
        SQL_HANDLE_STMT, stmt_);

    char buf[37] = {};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_));
    EXPECT_STREQ("0123ABCD-4567-89EF-0123-456789ABCDEF", buf);
    EXPECT_EQ(36, ind);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// datetime's lower bound. The wire value is a day offset from 1900, so any
// date before then is negative -- a signed path nothing else in the suite
// reaches.
// ---------------------------------------------------------------------------

TEST_F(FetchErrorSemanticsLiveTest, DatetimeAtItsLowerBoundDecodes) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST('1753-01-01' AS DATETIME) AS c1"));

    SQL_TIMESTAMP_STRUCT ts{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_TYPE_TIMESTAMP, &ts, sizeof(ts), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1753, ts.year);
    EXPECT_EQ(1, ts.month);
    EXPECT_EQ(1, ts.day);
    SQLCloseCursor(stmt_);
}
