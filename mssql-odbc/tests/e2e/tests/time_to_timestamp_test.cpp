// Copyright (c) Microsoft Corporation. All rights reserved.
// time_to_timestamp_test.cpp  –  a time value widened to a timestamp (AB#47247).
//
// ODBC Appendix D: converting a time value to a timestamp sets the date fields to
// the current date. msodbcsql reads that from localtime_s at the end of
// ParseDateTime (sqlccnvt.cpp), so these compare against the *local* date.
//
// The date-only targets are covered too: they must keep refusing a time value,
// and with different SQLSTATEs depending on where the mismatch comes from.

#include "odbc_test_fixture.h"

#include <cstring>
#include <ctime>
#include <string>

class TimeToTimestampLiveTest : public ODBCTest {
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
    void FetchOne(const std::string& sql) {
        ASSERT_SQL_OK(ExecDirect(sql), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    }
    // The local date, which is what the driver is expected to have filled in.
    static void LocalToday(SQLSMALLINT* year, SQLUSMALLINT* month, SQLUSMALLINT* day) {
        std::time_t now = std::time(nullptr);
        std::tm local {};
#ifdef _WIN32
        localtime_s(&local, &now);
#else
        localtime_r(&now, &local);
#endif
        *year = static_cast<SQLSMALLINT>(local.tm_year + 1900);
        *month = static_cast<SQLUSMALLINT>(local.tm_mon + 1);
        *day = static_cast<SQLUSMALLINT>(local.tm_mday);
    }
    void ExpectWidenedToToday(const std::string& sql, SQLSMALLINT target, SQLUSMALLINT hour,
                              SQLUSMALLINT minute, SQLUSMALLINT second, SQLUINTEGER fraction) {
        ASSERT_NO_FATAL_FAILURE(FetchOne(sql));
        SQL_TIMESTAMP_STRUCT ts;
        std::memset(&ts, 0, sizeof(ts));
        SQLLEN ind = -1;

        SQLSMALLINT before_year = 0;
        SQLUSMALLINT before_month = 0, before_day = 0;
        LocalToday(&before_year, &before_month, &before_day);
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, target, &ts, sizeof(ts), &ind), SQL_HANDLE_STMT,
                      stmt_);

        SQLSMALLINT after_year = 0;
        SQLUSMALLINT after_month = 0, after_day = 0;
        LocalToday(&after_year, &after_month, &after_day);
        const bool matches_before =
            ts.year == before_year && ts.month == before_month && ts.day == before_day;
        const bool matches_after =
            ts.year == after_year && ts.month == after_month && ts.day == after_day;
        EXPECT_TRUE(matches_before || matches_after) << sql;
        EXPECT_EQ(hour, ts.hour) << sql;
        EXPECT_EQ(minute, ts.minute) << sql;
        EXPECT_EQ(second, ts.second) << sql;
        EXPECT_EQ(fraction, ts.fraction) << sql;
        SQLCloseCursor(stmt_);
    }
};

// A character literal holding only a time.
TEST_F(TimeToTimestampLiveTest, CharacterTimeWidensToTodaysDate) {
    ExpectWidenedToToday("SELECT CAST('12:34:56' AS VARCHAR(32))", SQL_C_TYPE_TIMESTAMP, 12, 34,
                         56, 0);
}

TEST_F(TimeToTimestampLiveTest, CharacterTimeKeepsItsFraction) {
    ExpectWidenedToToday("SELECT CAST('12:34:56.789' AS VARCHAR(32))", SQL_C_TYPE_TIMESTAMP, 12,
                         34, 56, 789000000);
}

// A real time column, which reaches the conversion as decoded parts rather than
// as text, so it is a separate path into the same rule.
TEST_F(TimeToTimestampLiveTest, TimeColumnWidensToTodaysDate) {
    ExpectWidenedToToday("SELECT CAST('12:34:56.789' AS TIME(3))", SQL_C_TYPE_TIMESTAMP, 12, 34,
                         56, 789000000);
}

TEST_F(TimeToTimestampLiveTest, ZeroScaleTimeColumnWidensToTodaysDate) {
    ExpectWidenedToToday("SELECT CAST('12:34:56' AS TIME(0))", SQL_C_TYPE_TIMESTAMP, 12, 34, 56, 0);
}

// time(7) carries 100ns resolution, which the timestamp struct keeps.
TEST_F(TimeToTimestampLiveTest, TimeColumnKeepsHundredNanosecondResolution) {
    ExpectWidenedToToday("SELECT CAST('12:34:56.1234567' AS TIME(7))", SQL_C_TYPE_TIMESTAMP, 12,
                         34, 56, 123456700);
}

// The ODBC 2.x spelling resolves to the same conversion.
TEST_F(TimeToTimestampLiveTest, LegacyTimestampSpellingWidensToo) {
    ExpectWidenedToToday("SELECT CAST('12:34:56' AS TIME(0))", SQL_C_TIMESTAMP, 12, 34, 56, 0);
}

// The date-only targets still have nothing to build a date from. The SQLSTATE
// differs by where the mismatch is: bad text for the target is 22018, while a
// column whose type cannot feed the target at all is 07006.
TEST_F(TimeToTimestampLiveTest, CharacterTimeIntoDateIsStillInvalidCharacterValue) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST('12:34:56' AS VARCHAR(32))"));

    SQL_DATE_STRUCT date;
    std::memset(&date, 0, sizeof(date));
    SQLLEN ind = -1;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_TYPE_DATE, &date, sizeof(date), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
    SQLCloseCursor(stmt_);
}

TEST_F(TimeToTimestampLiveTest, TimeColumnIntoDateIsStillRestrictedDataType) {
    ASSERT_NO_FATAL_FAILURE(FetchOne("SELECT CAST('12:34:56' AS TIME(0))"));

    SQL_DATE_STRUCT date;
    std::memset(&date, 0, sizeof(date));
    SQLLEN ind = -1;
    EXPECT_EQ(SQL_ERROR, SQLGetData(stmt_, 1, SQL_C_TYPE_DATE, &date, sizeof(date), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07006");
    SQLCloseCursor(stmt_);
}
