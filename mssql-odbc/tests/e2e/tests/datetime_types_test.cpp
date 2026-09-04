// Copyright (c) Microsoft Corporation. All rights reserved.
// datetime_types_test.cpp  –  E2E coverage for the date/time C targets.
//
// The P5 coverage audit found the whole date/time family untested end to end:
// SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_SS_TIME2 and
// SQL_C_SS_TIMESTAMPOFFSET had no e2e test on either path, and
// SQL_C_TYPE_TIMESTAMP was covered only through SQLGetData. They are all
// implemented and unit tested, so what was missing is the parity evidence --
// and parity is what has caught every real divergence in this work.
//
// Each target is exercised on both paths, because they are different code:
// SQLGetData converts one value on demand, SQLBindCol/SQLFetchScroll fills a
// bound buffer from the rowset fill loop.
//
// Fractional seconds are asserted everywhere they exist. The wire carries them
// in units that differ per type, and a units bug survives every unit test that
// builds the struct by hand -- it only shows up against a real server.

#include "odbc_test_fixture.h"

#include <string>

// SQL Server extension C types. Declared here rather than pulled from
// msodbcsql.h: this suite builds on Linux and Windows, where that header lives
// in different places, and adding a platform-conditional include path for two
// structs is worse than declaring the ABI. Layout copied from msodbcsql.h
// (tagSS_TIME2_STRUCT / tagSS_TIMESTAMPOFFSET_STRUCT) and cross-checked against
// the driver's own definitions in api/odbc_types.rs. The static_asserts below
// fail the build rather than silently misread a buffer if either drifts.
#ifndef SQL_C_SS_TIME2
#define SQL_C_TYPES_EXTENDED 0x04000L
#define SQL_C_SS_TIME2 (SQL_C_TYPES_EXTENDED + 0)
#define SQL_C_SS_TIMESTAMPOFFSET (SQL_C_TYPES_EXTENDED + 1)

typedef struct tagSS_TIME2_STRUCT {
    SQLUSMALLINT hour;
    SQLUSMALLINT minute;
    SQLUSMALLINT second;
    SQLUINTEGER fraction;
} SQL_SS_TIME2_STRUCT;

typedef struct tagSS_TIMESTAMPOFFSET_STRUCT {
    SQLSMALLINT year;
    SQLUSMALLINT month;
    SQLUSMALLINT day;
    SQLUSMALLINT hour;
    SQLUSMALLINT minute;
    SQLUSMALLINT second;
    SQLUINTEGER fraction;
    SQLSMALLINT timezone_hour;
    SQLSMALLINT timezone_minute;
} SQL_SS_TIMESTAMPOFFSET_STRUCT;
#endif

static_assert(sizeof(SQL_SS_TIME2_STRUCT) == 12,
              "SQL_SS_TIME2_STRUCT layout does not match the msodbcsql ABI");
static_assert(sizeof(SQL_SS_TIMESTAMPOFFSET_STRUCT) == 20,
              "SQL_SS_TIMESTAMPOFFSET_STRUCT layout does not match the msodbcsql ABI");

class DateTimeTypesLiveTest : public ODBCTest {
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
// SQL_C_TYPE_DATE
// ---------------------------------------------------------------------------

TEST_F(DateTimeTypesLiveTest, DateToDateTargetViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('2024-02-29' AS DATE) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQL_DATE_STRUCT d{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_TYPE_DATE, &d, sizeof(d), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(2024, d.year);
    EXPECT_EQ(2, d.month);
    EXPECT_EQ(29, d.day) << "leap day";
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQL_DATE_STRUCT)), ind);
    SQLCloseCursor(stmt_);
}

TEST_F(DateTimeTypesLiveTest, DateToDateTargetViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('2024-02-29' AS DATE) AS c1"), SQL_HANDLE_STMT, stmt_);

    SQL_DATE_STRUCT d{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_TYPE_DATE, &d, sizeof(d), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(2024, d.year);
    EXPECT_EQ(2, d.month);
    EXPECT_EQ(29, d.day);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQL_DATE_STRUCT)), ind);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// SQL_C_TYPE_TIME — no fractional field, so a TIME(7) source has to truncate
// ---------------------------------------------------------------------------

TEST_F(DateTimeTypesLiveTest, TimeToTimeTargetViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('13:45:59' AS TIME(0)) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQL_TIME_STRUCT t{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_TYPE_TIME, &t, sizeof(t), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(13, t.hour);
    EXPECT_EQ(45, t.minute);
    EXPECT_EQ(59, t.second);
    SQLCloseCursor(stmt_);
}

TEST_F(DateTimeTypesLiveTest, TimeToTimeTargetViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('13:45:59' AS TIME(0)) AS c1"), SQL_HANDLE_STMT, stmt_);

    SQL_TIME_STRUCT t{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_TYPE_TIME, &t, sizeof(t), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(13, t.hour);
    EXPECT_EQ(45, t.minute);
    EXPECT_EQ(59, t.second);
    SQLCloseCursor(stmt_);
}

// The truncation itself. TIME(0) above has no fraction to drop, so it never
// reaches this branch -- and ASSERT_SQL_OK would not have caught the difference
// anyway, since it wraps SQL_SUCCEEDED and SQL_SUCCESS_WITH_INFO satisfies it.
// The return code and SQLSTATE are the assertions that matter here.
TEST_F(DateTimeTypesLiveTest, Time7ToTimeTargetTruncatesTheFraction) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('13:45:59.1234567' AS TIME(7)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQL_TIME_STRUCT t{};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_TYPE_TIME, &t, sizeof(t), &ind);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc) << "dropping the fraction is reported, not silent";
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S07");
    EXPECT_EQ(13, t.hour);
    EXPECT_EQ(45, t.minute);
    EXPECT_EQ(59, t.second);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// SQL_C_SS_TIME2 — the SQL Server target that keeps the fraction
// ---------------------------------------------------------------------------

TEST_F(DateTimeTypesLiveTest, Time7ToSsTime2TargetViaGetData) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('13:45:59.1234567' AS TIME(7)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQL_SS_TIME2_STRUCT t{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SS_TIME2, &t, sizeof(t), &ind), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(13, t.hour);
    EXPECT_EQ(45, t.minute);
    EXPECT_EQ(59, t.second);
    // Nanoseconds. TIME(7) resolves to 100 ns ticks, so the wire value has to be
    // scaled rather than copied -- this is the assertion that catches that.
    EXPECT_EQ(123456700u, t.fraction);
    SQLCloseCursor(stmt_);
}

TEST_F(DateTimeTypesLiveTest, Time7ToSsTime2TargetViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('13:45:59.1234567' AS TIME(7)) AS c1"), SQL_HANDLE_STMT,
                  stmt_);

    SQL_SS_TIME2_STRUCT t{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SS_TIME2, &t, sizeof(t), &ind), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(13, t.hour);
    EXPECT_EQ(45, t.minute);
    EXPECT_EQ(59, t.second);
    EXPECT_EQ(123456700u, t.fraction);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// SQL_C_SS_TIMESTAMPOFFSET — a negative offset, so a sign error cannot pass
// ---------------------------------------------------------------------------

TEST_F(DateTimeTypesLiveTest, DatetimeoffsetToSsTimestampoffsetViaGetData) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST('2023-06-15 12:34:56.1234567 -05:30' AS DATETIMEOFFSET(7)) AS c1"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQL_SS_TIMESTAMPOFFSET_STRUCT ts{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SS_TIMESTAMPOFFSET, &ts, sizeof(ts), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2023, ts.year);
    EXPECT_EQ(6, ts.month);
    EXPECT_EQ(15, ts.day);
    EXPECT_EQ(12, ts.hour);
    EXPECT_EQ(34, ts.minute);
    EXPECT_EQ(56, ts.second);
    EXPECT_EQ(123456700u, ts.fraction);
    EXPECT_EQ(-5, ts.timezone_hour);
    EXPECT_EQ(-30, ts.timezone_minute) << "the minute component carries the sign too";
    SQLCloseCursor(stmt_);
}

TEST_F(DateTimeTypesLiveTest, DatetimeoffsetToSsTimestampoffsetViaBoundFetch) {
    ASSERT_SQL_OK(
        ExecDirect("SELECT CAST('2023-06-15 12:34:56.1234567 -05:30' AS DATETIMEOFFSET(7)) AS c1"),
        SQL_HANDLE_STMT, stmt_);

    SQL_SS_TIMESTAMPOFFSET_STRUCT ts{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SS_TIMESTAMPOFFSET, &ts, sizeof(ts), &ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(2023, ts.year);
    EXPECT_EQ(6, ts.month);
    EXPECT_EQ(15, ts.day);
    EXPECT_EQ(12, ts.hour);
    EXPECT_EQ(34, ts.minute);
    EXPECT_EQ(56, ts.second);
    EXPECT_EQ(123456700u, ts.fraction);
    EXPECT_EQ(-5, ts.timezone_hour);
    EXPECT_EQ(-30, ts.timezone_minute);
    SQLCloseCursor(stmt_);
}

// ---------------------------------------------------------------------------
// SQL_C_TYPE_TIMESTAMP on the bound path (SQLGetData is covered in
// get_data_test.cpp)
// ---------------------------------------------------------------------------

TEST_F(DateTimeTypesLiveTest, Datetime2ToTimestampTargetViaBoundFetch) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST('2023-06-15 12:34:56.1234567' AS DATETIME2(7)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    SQL_TIMESTAMP_STRUCT ts{};
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_TYPE_TIMESTAMP, &ts, sizeof(ts), &ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(2023, ts.year);
    EXPECT_EQ(6, ts.month);
    EXPECT_EQ(15, ts.day);
    EXPECT_EQ(12, ts.hour);
    EXPECT_EQ(34, ts.minute);
    EXPECT_EQ(56, ts.second);
    EXPECT_EQ(123456700u, ts.fraction);
    SQLCloseCursor(stmt_);
}

TEST_F(DateTimeTypesLiveTest, LegacyDatetimeRoundsFractionToMillisecondsOnBothPaths) {
    const std::string query =
        "SELECT CAST('2024-05-20T12:34:56.127' AS DATETIME) AS c1";

    ASSERT_SQL_OK(ExecDirect(query), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQL_TIMESTAMP_STRUCT viaGetData{};
    SQLLEN getInd = 0;
    ASSERT_SQL_OK(
        SQLGetData(stmt_, 1, SQL_C_TYPE_TIMESTAMP, &viaGetData, sizeof(viaGetData), &getInd),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2024, viaGetData.year);
    EXPECT_EQ(5, viaGetData.month);
    EXPECT_EQ(20, viaGetData.day);
    EXPECT_EQ(12, viaGetData.hour);
    EXPECT_EQ(34, viaGetData.minute);
    EXPECT_EQ(56, viaGetData.second);
    EXPECT_EQ(127000000u, viaGetData.fraction);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQL_TIMESTAMP_STRUCT)), getInd);
    SQLCloseCursor(stmt_);

    ASSERT_SQL_OK(ExecDirect(query), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    constexpr char expectedText[] = "2024-05-20 12:34:56.127";
    char text[sizeof(expectedText)]{};
    SQLLEN textInd = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, text, sizeof(text), &textInd), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_STREQ(expectedText, text);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(expectedText) - 1), textInd);
    SQLCloseCursor(stmt_);

    ASSERT_SQL_OK(ExecDirect(query), SQL_HANDLE_STMT, stmt_);

    SQL_TIMESTAMP_STRUCT bound{};
    SQLLEN boundInd = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_TYPE_TIMESTAMP, &bound, sizeof(bound), &boundInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2024, bound.year);
    EXPECT_EQ(5, bound.month);
    EXPECT_EQ(20, bound.day);
    EXPECT_EQ(12, bound.hour);
    EXPECT_EQ(34, bound.minute);
    EXPECT_EQ(56, bound.second);
    EXPECT_EQ(127000000u, bound.fraction);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQL_TIMESTAMP_STRUCT)), boundInd);
    SQLCloseCursor(stmt_);
}

// A NULL date/time column must report SQL_NULL_DATA and leave the buffer alone,
// on both paths. Covered once here rather than per type: the NULL path is in
// the delivery layer, not per-conversion.
TEST_F(DateTimeTypesLiveTest, NullDatetimeReportsNullOnBothPaths) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS DATETIME2(7)) AS c1"), SQL_HANDLE_STMT, stmt_);

    SQL_TIMESTAMP_STRUCT bound{};
    bound.year = 1999;
    SQLLEN boundInd = 0;
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_TYPE_TIMESTAMP, &bound, sizeof(bound), &boundInd),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NULL_DATA, boundInd);
    EXPECT_EQ(1999, bound.year) << "a NULL must not overwrite the bound buffer";
    SQLCloseCursor(stmt_);

    // Unbind before the SQLGetData half. A column the fill loop already
    // delivered into a bound buffer is consumed, and SQLGetData on it returns
    // SQL_NO_DATA -- on both drivers.
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_UNBIND), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS DATETIME2(7)) AS c1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQL_TIMESTAMP_STRUCT viaGetData{};
    SQLLEN getInd = 0;
    ASSERT_SQL_OK(
        SQLGetData(stmt_, 1, SQL_C_TYPE_TIMESTAMP, &viaGetData, sizeof(viaGetData), &getInd),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NULL_DATA, getInd);
    SQLCloseCursor(stmt_);
}
