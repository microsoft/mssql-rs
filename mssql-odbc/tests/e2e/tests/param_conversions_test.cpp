// Copyright (c) Microsoft Corporation. All rights reserved.
// param_conversions_test.cpp  -  E2E tests for parameter conversion outside the
// same-family character and binary cases.
//
// Two groups:
//   CrossConversionLiveTest  - an integer C type bound against a character
//                              ParameterType, and the reverse.
//   ScalarConversionLiveTest - the scalar rows of AB#47500: bit, float, decimal,
//                              GUID, temporal, xml and sql_variant.
//
// Same-family character conversions live in param_char_conversions_test.cpp,
// binary in param_binary_conversions_test.cpp, and statement lifecycle in
// execute_test.cpp.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <cstdint>
#include <cstring>
#include <limits>
#include <string>
#include <vector>

// SQL Server extension types, declared here rather than pulled from
// msodbcsql.h for the reason datetime_types_test.cpp gives: this suite builds
// on Linux and Windows, where that header lives in different places. Layout and
// values copied from msodbcsql.h and cross-checked against the driver's own
// definitions in api/odbc_types.rs; the static_asserts fail the build rather
// than silently misread a buffer if either drifts.
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

#ifndef SQL_SS_VARIANT
#define SQL_SS_VARIANT (-150)
#endif
#ifndef SQL_SS_XML
#define SQL_SS_XML (-152)
#endif
#ifndef SQL_SS_TIME2
#define SQL_SS_TIME2 (-154)
#endif
#ifndef SQL_SS_TIMESTAMPOFFSET
#define SQL_SS_TIMESTAMPOFFSET (-155)
#endif

static_assert(sizeof(SQL_SS_TIME2_STRUCT) == 12,
              "SQL_SS_TIME2_STRUCT layout does not match the msodbcsql ABI");
static_assert(sizeof(SQL_SS_TIMESTAMPOFFSET_STRUCT) == 20,
              "SQL_SS_TIMESTAMPOFFSET_STRUCT layout does not match the msodbcsql ABI");

class CrossConversionLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLRETURN Prepare(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // Bind |text| as a narrow or wide character buffer, per |c_type|. The
    // buffers are members so they outlive the SQLExecute that reads them.
    SQLRETURN BindText(SQLSMALLINT c_type, const std::string& text,
                       SQLSMALLINT sql_type, SQLULEN column_size = 0) {
        // Reserve before assigning so data() is non-null even for "": an empty
        // vector may return nullptr, which SQLBindParameter reads as a null
        // ParameterValuePtr and answers HY009 instead of the state under test.
        narrow_.reserve(text.size() + 1);
        wide_.reserve(text.size() + 1);
        narrow_.assign(text.begin(), text.end());
        wide_.clear();
        void* data;
        if (c_type == SQL_C_WCHAR) {
            for (unsigned char ch : text) {
                // Widened per byte, not transcoded: a UTF-8 sequence would bind
                // as its individual bytes - U+00C2 U+00A0 for a non-breaking
                // space. Non-ASCII belongs in a narrow-only case until this
                // helper learns to transcode.
                EXPECT_LT(ch, 0x80) << "BindText cannot widen non-ASCII text";
                wide_.push_back(ch);
            }
            indicator_ = static_cast<SQLLEN>(wide_.size() * sizeof(SQLWCHAR));
            data = wide_.data();
        } else {
            indicator_ = static_cast<SQLLEN>(narrow_.size());
            data = narrow_.data();
        }
        return SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, c_type, sql_type,
                                column_size, 0, data, indicator_, &indicator_);
    }

    // Read column 1 of the current row as a narrow string.
    std::string GetColumnChar(SQLUSMALLINT col = 1) {
        SQLCHAR buf[512] = {0};
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, col, SQL_C_CHAR, buf, sizeof(buf), &ind);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        if (ind == SQL_NULL_DATA) {
            return std::string();
        }
        return std::string(reinterpret_cast<const char*>(buf));
    }

    // Runs `SELECT ? AS v` with the parameter already bound and returns the
    // round-tripped value as text.
    std::string ExecuteAndReadBack() {
        EXPECT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        std::string v = GetColumnChar();
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        return v;
    }

    void ResetParams() {
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    }

    std::vector<SQLCHAR> narrow_;
    std::vector<SQLWCHAR> wide_;
    SQLLEN indicator_ = 0;
};

// ---------------------------------------------------------------------------
// Integer C type -> character SQL type
// ---------------------------------------------------------------------------

// The wire type follows ParameterType, not the C type that was bound, so an
// integer buffer arrives declared as the character type the application asked
// for. SQL_VARIANT_PROPERTY reports what the server actually received, which the
// round-tripped digits alone cannot show.
TEST_F(CrossConversionLiveTest, IntegerParamDeclaresTheCharacterParameterType) {
    struct Case {
        SQLSMALLINT sql_type;
        const char* base_type;
    };
    const Case cases[] = {
        {SQL_VARCHAR, "varchar"},
        {SQL_WVARCHAR, "nvarchar"},
        {SQL_CHAR, "char"},
        {SQL_WCHAR, "nchar"},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT CAST(SQL_VARIANT_PROPERTY(CAST(? AS SQL_VARIANT),"
                              " 'BaseType') AS VARCHAR(32))"),
                      SQL_HANDLE_STMT, stmt_);
        SQLINTEGER value = 12;
        SQLLEN ind = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                       c.sql_type, 8, 0, &value, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.base_type, ExecuteAndReadBack()) << "sql type " << c.sql_type;
        ResetParams();
    }
}

// Base 10, no padding, and a sign only when negative - the shape _ltoa_s and
// BigintToChar produce in msodbcsql's ConvertToChar (sqlccnvt.cpp:1634).
TEST_F(CrossConversionLiveTest, IntegerParamFormatsBaseTen) {
    struct Case {
        SQLBIGINT value;
        const char* expected;
    };
    const Case cases[] = {
        {0, "0"},
        {7, "7"},
        {-42, "-42"},
        {2147483647, "2147483647"},
        {-9223372036854775807LL - 1, "-9223372036854775808"},
    };
    for (const Case& c : cases) {
        for (SQLSMALLINT sql_type : {SQL_VARCHAR, SQL_WVARCHAR}) {
            ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
            SQLBIGINT value = c.value;
            SQLLEN ind = 0;
            ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SBIGINT,
                                           sql_type, 32, 0, &value, 0, &ind),
                          SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(c.expected, ExecuteAndReadBack())
                << "value " << c.value << " sql type " << sql_type;
            ResetParams();
        }
    }
}

// SQL_C_UBIGINT is read unsigned, so a value past i64::MAX formats as itself
// rather than wrapping negative.
TEST_F(CrossConversionLiveTest, UnsignedBigintParamFormatsUnsigned) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    SQLUBIGINT value = 18446744073709551615ULL;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_UBIGINT,
                                   SQL_VARCHAR, 32, 0, &value, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("18446744073709551615", ExecuteAndReadBack());
}

// Digits are never blanks, so the trailing-blank exemption cannot absorb them:
// a formatted value longer than ColumnSize is 22001.
//
// Split out because msodbcsql applies no length check here at all - an integer
// C type enters no arm of ParamToSQLType that has one (sqlcfunc.cpp:2586, :2854,
// :3165, :3177) - and what happens instead is undefined, varying by build.
// Binding 12345 as SQL_C_SLONG to a SQL_VARCHAR ColumnSize 3 has been observed
// three ways: retail 18.05.0002 returns SQL_SUCCESS with no diagnostic and the
// server sees varchar(3) holding "123"; debug 18.06.0002 aborts on
// assert(*pstMaxLen > 0 && *pstMaxLen >= stLen) (sqlcmisc.cpp:7458); retail
// 18.6.2.1 hangs in SQLExecute. ColumnSize 32 returns "12345" on all three.
//
// Do not re-derive this from the source: the retail fallthrough at :7459,
// `stMaxLen = (*pstMaxLen >= stLen) ? *pstMaxLen : stLen`, widens the
// declaration to fit, which matches none of the three. Only the assert
// reproduces.
TEST_F(CrossConversionLiveTest, IntegerParamTooWideForColumnSizeIs22001) {
    SKIP_IF_COMPARING_MSODBCSQL();
    for (SQLSMALLINT sql_type : {SQL_VARCHAR, SQL_WVARCHAR, SQL_CHAR, SQL_WCHAR}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER value = 12345;
        SQLLEN ind = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                       sql_type, 3, 0, &value, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "sql type " << sql_type;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
        ResetParams();
    }
}

// The sign counts against the declared length like any other character. Skipped
// on the compare leg for the same reason as IntegerParamTooWideForColumnSizeIs22001:
// retail msodbcsql silently truncates instead, turning -123 into "-12".
TEST_F(CrossConversionLiveTest, NegativeSignCountsAgainstColumnSize) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = -123;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_VARCHAR, 3, 0, &value, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
}

// The other half of the sign rule, split out because both drivers agree on it:
// one more character of room and the same value fits. Kept on the compare leg -
// only the over-long case diverges.
TEST_F(CrossConversionLiveTest, NegativeSignFitsWhenColumnSizeAllowsIt) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = -123;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_VARCHAR, 4, 0, &value, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("-123", ExecuteAndReadBack());
}

// ---------------------------------------------------------------------------
// Character C type -> integer SQL type
// ---------------------------------------------------------------------------

// The integer ParameterType names the wire type here too.
TEST_F(CrossConversionLiveTest, CharParamDeclaresTheIntegerParameterType) {
    struct Case {
        SQLSMALLINT sql_type;
        const char* base_type;
    };
    const Case cases[] = {
        {SQL_TINYINT, "tinyint"},
        {SQL_SMALLINT, "smallint"},
        {SQL_INTEGER, "int"},
        {SQL_BIGINT, "bigint"},
    };
    for (const Case& c : cases) {
        for (SQLSMALLINT c_type : {SQL_C_CHAR, SQL_C_WCHAR}) {
            ASSERT_SQL_OK(Prepare("SELECT CAST(SQL_VARIANT_PROPERTY(CAST(? AS SQL_VARIANT),"
                                  " 'BaseType') AS VARCHAR(32))"),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindText(c_type, "12", c.sql_type), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(c.base_type, ExecuteAndReadBack())
                << "c type " << c_type << " sql type " << c.sql_type;
            ResetParams();
        }
    }
}

// A leading sign is accepted and blanks are padding on both ends, matching
// msodbcsql's CharToBigint (sqlccnvt.cpp:7758).
TEST_F(CrossConversionLiveTest, CharParamParsesIntegerLiteral) {
    struct Case {
        const char* text;
        const char* expected;
    };
    const Case cases[] = {
        {"0", "0"},
        {"42", "42"},
        {"-42", "-42"},
        {"+42", "42"},
        {"   42   ", "42"},
        {"9223372036854775807", "9223372036854775807"},
        {"-9223372036854775808", "-9223372036854775808"},
    };
    for (const Case& c : cases) {
        for (SQLSMALLINT c_type : {SQL_C_CHAR, SQL_C_WCHAR}) {
            ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindText(c_type, c.text, SQL_BIGINT), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(c.expected, ExecuteAndReadBack())
                << "c type " << c_type << " text " << c.text;
            ResetParams();
        }
    }
}

// Dropping a fraction on the way to the server is an error, not the 01S07
// warning the fetch direction reports: msodbcsql's ParamToSQLType rewrites the
// truncation to IDS_22_001 for any non-2.x application (sqlcfunc.cpp:3348).
TEST_F(CrossConversionLiveTest, CharParamDroppedFractionIs22001) {
    for (const char* text : {"12.7", "-0.5", "0.001"}) {
        for (SQLSMALLINT c_type : {SQL_C_CHAR, SQL_C_WCHAR}) {
            ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindText(c_type, text, SQL_INTEGER), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "c type " << c_type << " text " << text;
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
            ResetParams();
        }
    }
}

// Only a non-zero dropped digit is truncation - msodbcsql flags the same way,
// `if (c != '0') Error = CVT_FRACT_TRUNC` (sqlccnvt.cpp:7823) - so a fraction
// that loses nothing converts cleanly.
TEST_F(CrossConversionLiveTest, CharParamZeroFractionConverts) {
    for (const char* text : {"12.", "12.0", "12.000"}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindText(SQL_C_CHAR, text, SQL_INTEGER), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("12", ExecuteAndReadBack()) << "text " << text;
        ResetParams();
    }
}

// A magnitude the target cannot hold is 22003, including a negative into
// tinyint, the one unsigned SQL Server integer.
TEST_F(CrossConversionLiveTest, CharParamOutOfRangeIs22003) {
    struct Case {
        SQLSMALLINT sql_type;
        const char* text;
    };
    const Case cases[] = {
        {SQL_TINYINT, "256"},
        {SQL_TINYINT, "-1"},
        {SQL_SMALLINT, "32768"},
        {SQL_INTEGER, "2147483648"},
        {SQL_BIGINT, "9223372036854775808"},
        // Well formed but past every accumulator, so an overflow rather than a
        // syntax error (sqlccnvt.cpp:7840 vs :7809).
        {SQL_BIGINT, "9999999999999999999999999999999999999999999"},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindText(SQL_C_CHAR, c.text, c.sql_type), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_))
            << "sql type " << c.sql_type << " text " << c.text;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
        ResetParams();
    }
}

// Overflow outranks a dropped fraction: the narrowing runs before msodbcsql's
// fraction rewrite can fire, so a value that does both reports 22003.
TEST_F(CrossConversionLiveTest, CharParamOverflowOutranksFraction) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindText(SQL_C_CHAR, "999.5", SQL_TINYINT), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
}

// ColumnSize describes a character declaration, so it has no say over an
// integer target - the value is bounded by the target's range instead.
TEST_F(CrossConversionLiveTest, ColumnSizeDoesNotBoundAnIntegerTarget) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindText(SQL_C_CHAR, "1234567", SQL_INTEGER, 1), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1234567", ExecuteAndReadBack());
}

// A numeric literal reaches the parser through the same indicator rules as any
// other character buffer, SQL_NTS included - which is how an application most
// often binds one.
TEST_F(CrossConversionLiveTest, CharParamAcceptsNtsIndicator) {
    for (SQLSMALLINT c_type : {SQL_C_CHAR, SQL_C_WCHAR}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        std::vector<SQLCHAR> narrow = {'-', '4', '2', '\0'};
        std::vector<SQLWCHAR> wide = {'-', '4', '2', '\0'};
        SQLLEN ind = SQL_NTS;
        void* data = c_type == SQL_C_WCHAR ? static_cast<void*>(wide.data())
                                           : static_cast<void*>(narrow.data());
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, c_type, SQL_INTEGER,
                                       0, 0, data, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("-42", ExecuteAndReadBack()) << "c type " << c_type;
        ResetParams();
    }
}

// The parse is locale-independent. msodbcsql keeps an entire NLS suite for
// numeric parameters (testsrc/.../ODBCNLS/src/TCSQLBindParamNonChar.cpp) and
// still binds the invariant spelling: CharToBigint accepts ASCII digits and a
// leading sign only, so a grouped or comma-decimal literal is rejected however
// the thread locale is set.
TEST_F(CrossConversionLiveTest, LocaleFormattedNumbersAreRejected) {
    // A non-breaking space is padding to no one: msodbcsql trims ' ' alone.
    for (const char* text : {"1,234", "1,5", "1 234", "1.234,5", "\xc2\xa0" "12"}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindText(SQL_C_CHAR, text, SQL_INTEGER), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "text " << text;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018") << "text " << text;
        ResetParams();
    }
}

// An unbounded ColumnSize selects varchar(max) for a formatted integer exactly
// as it does for a character source, so the digits are never length-checked.
// Asserted as a round trip rather than through SQL_VARIANT_PROPERTY, because
// sql_variant cannot hold a varchar(max) at all.
//
// Benefits-from-mock-tds: the declaration is the point here and is the one thing
// this cannot see. A byte-level mock would assert the RPC carries varchar(max)
// rather than a varchar(n) wide enough to hold 20 digits; today only the round
// trip is observable, so a wrong-but-wide declaration would still pass.
TEST_F(CrossConversionLiveTest, IntegerParamReachesMaxWhenColumnSizeIsUnbounded) {
    for (SQLSMALLINT sql_type : {SQL_VARCHAR, SQL_WVARCHAR}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        SQLBIGINT value = -9223372036854775807LL - 1;
        SQLLEN ind = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SBIGINT, sql_type,
                                       0, 0, &value, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);
        // 20 characters under a ColumnSize of 0: a zero-length declaration would
        // have raised 22001 instead.
        EXPECT_EQ("-9223372036854775808", ExecuteAndReadBack()) << "sql type " << sql_type;
        ResetParams();
    }
}

// Scientific notation is accepted, matching msodbcsql's split to CharToDouble
// once it spots an e/E (sqlccnvt.cpp:5088).
TEST_F(CrossConversionLiveTest, CharParamAcceptsScientificNotation) {
    struct Case {
        const char* text;
        const char* expected;
    };
    const Case cases[] = {{"1e3", "1000"}, {"-1.5E2", "-150"}};
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindText(SQL_C_CHAR, c.text, SQL_INTEGER), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.expected, ExecuteAndReadBack()) << "text " << c.text;
        ResetParams();
    }
}

// A string that is not a numeric literal is 22018, "Invalid character value for
// cast specification". msodbcsql agrees: CharToBigint returns CVT_ERROR
// (sqlccnvt.cpp:7809) = IDS_22_005 (sqlcprot.h:950), which reaches the
// std_error branch of SQL_DIAG_SQLSTATE (sqlcerr.cpp:990) and resolves through
// the driver-generated-error map (cli_common/src/clntcomn.cpp:1015,
// IDS_22_005 -> L"2200522018"); a 3.x application takes the second half.
TEST_F(CrossConversionLiveTest, CharParamInvalidLiteralIs22018) {
    // Only blanks are padding, so other whitespace and interior blanks are
    // invalid rather than trimmed. The empty buffer runs first deliberately: it
    // is the case whose bound pointer would be null if BindText did not reserve.
    for (const char* text : {"", "abc", "   ", "1 2", "\t12", "--1", "1.2.3", "0x1F", "12abc"}) {
        for (SQLSMALLINT c_type : {SQL_C_CHAR, SQL_C_WCHAR}) {
            // The one pairing msodbcsql answers differently; see
            // BlankOnlyWideLiteralIs22018.
            if (c_type == SQL_C_WCHAR && std::string(text) == "   ") {
                continue;
            }
            ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(BindText(c_type, text, SQL_INTEGER), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "c type " << c_type << " text " << text;
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018")
                << "c type " << c_type << " text \"" << text << "\" len "
                << strlen(text);
            ResetParams();
        }
    }
}

// A wide buffer of nothing but blanks is the single input where msodbcsql
// disagrees: measured against retail 18.05.0002 it answers HY000, where the
// same blanks as SQL_C_CHAR - and every other invalid literal in either width,
// including a zero-length wide buffer - answer 22018. Blanks are the only
// padding CharToBigint trims (sqlccnvt.cpp:7777), so trimming this input leaves
// nothing, and a wide source reaches that parser through an extra transcode the
// narrow one skips; which of the two produces the different exit is not
// established, so the mechanism here is unverified and only the state is.
TEST_F(CrossConversionLiveTest, BlankOnlyWideLiteralIs22018) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindText(SQL_C_WCHAR, "   ", SQL_INTEGER), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
}

// A literal with more digits than an exact i128 mantissa. The parser reduces it
// to an integer part plus a dropped-fraction flag for integer targets, so the
// fraction is reported rather than rounded away. The fetch direction shares that
// parser and is asserted by WideDecimalColumnKeepsPrecisionForADoubleTarget in
// get_data_test.cpp.
TEST_F(CrossConversionLiveTest, WideDecimalLiteralReportsTruncation) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindText(SQL_C_CHAR, "1.234567890123456789012345678901234567890",
                           SQL_INTEGER),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    ResetParams();

    // All-zero past the mantissa drops nothing, so it converts.
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindText(SQL_C_CHAR, "7.000000000000000000000000000000000000000",
                           SQL_INTEGER),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("7", ExecuteAndReadBack());
}

// The cross-family pairings are bindable now, so a data-at-execution indicator
// on one is refused at execute rather than at bind: the DAE indicator is only
// read when the parameter list is built.
//
// msodbcsql returns SQL_NEED_DATA for this pairing at SQLExecute, but does not
// actually stream it: SQLPutData itself then rejects with HY019 ("Processing
// of fixed length targets cannot be spread over multiple calls to
// SQLPutData"). Both drivers agree the pairing cannot stream through -- they
// just detect it one call apart -- so the parity run stays skipped rather
// than comparing error codes that differ by construction.
TEST_F(CrossConversionLiveTest, CrossFamilyDataAtExecutionIsRejectedAtExecute) {
    SKIP_IF_COMPARING_MSODBCSQL();

    for (SQLSMALLINT c_type : {SQL_C_CHAR, SQL_C_WCHAR}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        SQLLEN ind = SQL_DATA_AT_EXEC;
        SQLCHAR token = 0;
        // The bind itself is accepted - that is the change from before.
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, c_type, SQL_INTEGER,
                                       0, 0, &token, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);

        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "c type " << c_type;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
        ResetParams();
    }
}

// Binary stays outside the character/integer composition, so neither direction
// gains a binary pairing.
TEST_F(CrossConversionLiveTest, BinaryPairingsStayRejectedAtBind) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SQLINTEGER value = 1;
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR,
              SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_VARBINARY,
                               8, 0, &value, 0, &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");

    EXPECT_EQ(SQL_ERROR,
              SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_BINARY, SQL_INTEGER,
                               0, 0, &value, sizeof(value), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// ---------------------------------------------------------------------------
// Scalar rows of AB#47500: bit, float, decimal, GUID, temporal, xml, variant.
//
// Values are read back through server-side CONVERT to character text rather than
// SQLGetData with the matching C target, so a case exercises the parameter
// direction only and runs unchanged on both drivers.
// ---------------------------------------------------------------------------

class ScalarConversionLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLRETURN Prepare(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // Copies |value| into a member buffer so it outlives the SQLExecute that
    // reads it, then binds it. Fixed-width C types take their size from the C
    // type, so the indicator is set but not consulted.
    template <typename T>
    SQLRETURN BindFixed(SQLSMALLINT c_type, SQLSMALLINT sql_type, const T& value,
                        SQLULEN column_size = 0, SQLSMALLINT decimal_digits = 0) {
        static_assert(sizeof(T) <= sizeof(storage_), "widen storage_");
        std::memcpy(storage_, &value, sizeof(T));
        indicator_ = static_cast<SQLLEN>(sizeof(T));
        return SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, c_type, sql_type, column_size,
                                decimal_digits, storage_, sizeof(T), &indicator_);
    }

    SQLRETURN BindNarrow(SQLSMALLINT sql_type, const std::string& text,
                         SQLULEN column_size = 0, SQLSMALLINT decimal_digits = 0) {
        narrow_.reserve(text.size() + 1);
        narrow_.assign(text.begin(), text.end());
        indicator_ = static_cast<SQLLEN>(narrow_.size());
        return SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, sql_type, column_size,
                                decimal_digits, narrow_.data(),
                                static_cast<SQLLEN>(narrow_.size()), &indicator_);
    }

    std::string GetColumnChar(SQLUSMALLINT col = 1) {
        SQLCHAR buf[512] = {0};
        SQLLEN ind = 0;
        EXPECT_SQL_OK(SQLGetData(stmt_, col, SQL_C_CHAR, buf, sizeof(buf), &ind),
                      SQL_HANDLE_STMT, stmt_);
        if (ind == SQL_NULL_DATA) {
            return "<null>";
        }
        return std::string(reinterpret_cast<const char*>(buf));
    }

    std::string ExecuteAndReadBack() {
        EXPECT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        std::string v = GetColumnChar();
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        return v;
    }

    void ResetParams() {
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    }

    // sql_variant is how a parameter's declared type is observable end to end
    // without a mock server.
    static const char* kBaseTypeQuery;

    alignas(8) SQLCHAR storage_[64] = {0};
    std::vector<SQLCHAR> narrow_;
    SQLLEN indicator_ = 0;
};

const char* ScalarConversionLiveTest::kBaseTypeQuery =
    "SELECT CAST(SQL_VARIANT_PROPERTY(CAST(? AS SQL_VARIANT), 'BaseType') AS VARCHAR(32))";

// Every scalar row declares the type the application named, not one derived
// from the C buffer. Before AB#47500 each of these was HYC00 at bind.
TEST_F(ScalarConversionLiveTest, ScalarParamsDeclareTheParameterType) {
    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_BIT, SQL_BIT, static_cast<SQLCHAR>(1)), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ("bit", ExecuteAndReadBack());
    ResetParams();

    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_FLOAT, SQL_REAL, 1.5f), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("real", ExecuteAndReadBack());
    ResetParams();

    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_DOUBLE, SQL_DOUBLE, 1.5), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("float", ExecuteAndReadBack());
    ResetParams();

    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_DECIMAL, "1.5", 10, 2), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("decimal", ExecuteAndReadBack());
    ResetParams();

    SQLGUID guid = {0x01234567, 0x89AB, 0xCDEF, {1, 2, 3, 4, 5, 6, 7, 8}};
    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_GUID, SQL_GUID, guid), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("uniqueidentifier", ExecuteAndReadBack());
    ResetParams();

    SQL_DATE_STRUCT date = {2024, 6, 15};
    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_DATE, SQL_TYPE_DATE, date), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("date", ExecuteAndReadBack());
    ResetParams();

    SQL_TIMESTAMP_STRUCT ts = {2024, 6, 15, 12, 30, 0, 0};
    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, ts, 0, 3),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("datetime2", ExecuteAndReadBack());
    ResetParams();

    SQL_SS_TIME2_STRUCT t2 = {13, 45, 30, 0};
    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIME2, SQL_SS_TIME2, t2, 0, 3), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("time", ExecuteAndReadBack());
    ResetParams();

    SQL_SS_TIMESTAMPOFFSET_STRUCT dto = {2024, 6, 15, 12, 30, 0, 0, 5, 30};
    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIMESTAMPOFFSET, SQL_SS_TIMESTAMPOFFSET, dto, 0, 3),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("datetimeoffset", ExecuteAndReadBack());
    ResetParams();
}

TEST_F(ScalarConversionLiveTest, BitParamRoundTrips) {
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(4), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_BIT, SQL_BIT, static_cast<SQLCHAR>(1)), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ("1", ExecuteAndReadBack());
    ResetParams();

    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(4), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_BIT, SQL_BIT, static_cast<SQLCHAR>(0)), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ("0", ExecuteAndReadBack());
}

// msodbcsql reads the SQL_C_BIT buffer as one SCHAR and widens it like a tinyint
// (sqlccnvt.cpp:5057), so it never rejects a value outside 0/1 - anything
// non-zero reaches `bit` as 1. Asserted on both drivers rather than skipped,
// because this is the parity claim.
TEST_F(ScalarConversionLiveTest, ANonZeroBitByteIsOne) {
    for (SQLCHAR byte : {static_cast<SQLCHAR>(2), static_cast<SQLCHAR>(0xFF)}) {
        ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(4), ?)"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindFixed(SQL_C_BIT, SQL_BIT, byte), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("1", ExecuteAndReadBack()) << "byte " << static_cast<int>(byte);
        ResetParams();
    }
}

TEST_F(ScalarConversionLiveTest, FloatParamsRoundTrip) {
    EXPECT_EQ("1.5", [&] {
        EXPECT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(BindFixed(SQL_C_DOUBLE, SQL_DOUBLE, 1.5), SQL_HANDLE_STMT, stmt_);
        std::string v = ExecuteAndReadBack();
        ResetParams();
        return v;
    }());

    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_FLOAT, SQL_REAL, -2.25f), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("-2.25", ExecuteAndReadBack());
}

// msodbcsql's real range check is symmetric: sqlccnvt.cpp:5519 answers CVT_PREC
// (IDS_22_003) for a non-zero magnitude below FLT_MIN as well as one above
// FLT_MAX. Underflow is the half that is easy to miss.
//
// An infinity is rejected too, which is less obvious: Temp is a DOUBLE
// (sqlccnvt.cpp:5327) and FLT_MAX promotes to one, so `+INF > FLT_MAX` holds
// and the check fires before the narrowing cast. A NaN compares false against
// all four bounds and passes.
TEST_F(ScalarConversionLiveTest, ADoubleOutsideTheRealRangeIs22003) {
    const double inf = std::numeric_limits<double>::infinity();
    for (double v : {1e39, -1e39, 1e-40, -1e-40, inf, -inf}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindFixed(SQL_C_DOUBLE, SQL_REAL, v), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "value " << v;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
        ResetParams();
    }

    // The contrast that shows the check is a narrowing check and not a bound on
    // the value: float is 8 bytes, so the magnitude real rejects is fine here.
    // Infinity is not the control to use - SQL Server has no float encoding for
    // it, so both drivers fail that on the wire rather than at conversion.
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_DOUBLE, SQL_DOUBLE, 1e-40), SQL_HANDLE_STMT, stmt_);
    EXPECT_NE(SQL_ERROR, SQLExecute(stmt_));
}

TEST_F(ScalarConversionLiveTest, GuidParamKeepsItsFieldLayout) {
    SQLGUID guid = {0x01234567,
                    0x89AB,
                    0xCDEF,
                    {0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF}};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_GUID, SQL_GUID, guid), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("01234567-89AB-CDEF-0123-456789ABCDEF", ExecuteAndReadBack());
}

// The defaulted twin of the test above: a defaulted binding is checked against
// the conversion matrix like an explicit one. DefaultCTypeNullBindsAndExecutes
// covers SQL_GUID's NULL path, which has no buffer to read and so cannot show
// which C type was resolved.
//
// Skipped on the compare leg because msodbcsql's rgbTRANSTYPE380 resolves
// SQL_GUID to SQL_C_CHAR and reads this SQLGUID buffer as text, while this
// driver follows the ODBC 3.x table to SQL_C_GUID -- registered deviation 2
// (AB#47365), which this test is what makes observable.
TEST_F(ScalarConversionLiveTest, DefaultCTypeGuidRoundTrips) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SQLGUID guid = {0x01234567,
                    0x89AB,
                    0xCDEF,
                    {0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF}};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_DEFAULT, SQL_GUID, guid), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("01234567-89AB-CDEF-0123-456789ABCDEF", ExecuteAndReadBack());
}

// The declared scale reaches the server, and the value is rescaled to it.
TEST_F(ScalarConversionLiveTest, DecimalParamUsesTheDeclaredPrecisionAndScale) {
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_DECIMAL, "1.5", 10, 3), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1.500", ExecuteAndReadBack());
    ResetParams();

    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_NUMERIC, "-12.34", 10, 2), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("-12.34", ExecuteAndReadBack());
}

// Digits past the declared scale are dropped when zero and are 22001 when not -
// `if (c != '0') Error = CVT_FRACT_TRUNC` (sqlccnvt.cpp:7823), rewritten to
// IDS_22_001 inbound (sqlcfunc.cpp:3348).
TEST_F(ScalarConversionLiveTest, DecimalFractionPastTheScaleIsDroppedOnlyWhenZero) {
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_DECIMAL, "1.50", 5, 1), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1.5", ExecuteAndReadBack());
    ResetParams();

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_DECIMAL, "1.55", 5, 1), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
}

TEST_F(ScalarConversionLiveTest, DecimalPastItsDeclaredPrecisionIs22003) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_DECIMAL, "1000", 3, 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
}

TEST_F(ScalarConversionLiveTest, UnparseableDecimalLiteralIs22018) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_DECIMAL, "abc", 10, 2), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
}

// Every other decimal case binds SQL_C_CHAR, so this is the only end-to-end
// cover for the wide arm of decimal_from_text - the two C types must reach the
// same value and the same rescale.
TEST_F(ScalarConversionLiveTest, AWideLiteralReachesTheDecimalTarget) {
    std::vector<SQLWCHAR> wide;
    for (char c : std::string("-1.50")) {
        wide.push_back(static_cast<SQLWCHAR>(c));
    }
    SQLLEN ind = static_cast<SQLLEN>(wide.size() * sizeof(SQLWCHAR));
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_DECIMAL, 10, 1,
                                   wide.data(), ind, &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("-1.5", ExecuteAndReadBack());
}

// An exponent literal has no exact scaled form, so decimal_from_text routes it
// through the f64 approximation rather than the integer rescale. Untested until
// now, and the one decimal arm whose msodbcsql equivalent is unconfirmed - the
// CharToDouble citation was verified for an integer target, not a decimal one.
TEST_F(ScalarConversionLiveTest, AnExponentLiteralReachesTheDecimalTarget) {
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_DECIMAL, "1.5e2", 10, 2), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("150.00", ExecuteAndReadBack());
}

TEST_F(ScalarConversionLiveTest, DateParamRoundTrips) {
    SQL_DATE_STRUCT date = {2024, 2, 29};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 23)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_DATE, SQL_TYPE_DATE, date), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2024-02-29", ExecuteAndReadBack());
}

// ValidateDateStruct (sqlccnvt.cpp:8821) answers CVT_DT_ERROR = IDS_22_007_00.
// The month-length and leap-year arms are the ones plain day arithmetic would
// silently roll over into the next month.
TEST_F(ScalarConversionLiveTest, ImpossibleDateIs22007) {
    struct Case {
        SQLSMALLINT year;
        SQLUSMALLINT month;
        SQLUSMALLINT day;
    };
    for (const Case& c : {Case{0, 1, 1}, Case{10000, 1, 1}, Case{2024, 0, 1},
                          Case{2024, 13, 1}, Case{2024, 1, 0}, Case{2024, 4, 31},
                          Case{2023, 2, 29}, Case{1900, 2, 29}}) {
        SQL_DATE_STRUCT date = {c.year, c.month, c.day};
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_DATE, SQL_TYPE_DATE, date), SQL_HANDLE_STMT,
                      stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_))
            << c.year << "-" << c.month << "-" << c.day;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22007");
        ResetParams();
    }
}

// ValidateTimeStruct (sqlccnvt.cpp:8844) bounds each component separately and
// answers CVT_TM_ERROR = IDS_22_007_01. 60 seconds is rejected: no leap second.
TEST_F(ScalarConversionLiveTest, ImpossibleTimeIs22007) {
    struct Case {
        SQLUSMALLINT hour;
        SQLUSMALLINT minute;
        SQLUSMALLINT second;
        SQLUINTEGER fraction;
    };
    for (const Case& c : {Case{24, 0, 0, 0}, Case{0, 60, 0, 0}, Case{0, 0, 60, 0},
                          Case{0, 0, 0, 1000000000}}) {
        SQL_SS_TIME2_STRUCT t2 = {c.hour, c.minute, c.second, c.fraction};
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIME2, SQL_SS_TIME2, t2, 0, 7), SQL_HANDLE_STMT,
                      stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22007");
        ResetParams();
    }
}

// A temporal parameter is declared at the maximum fractional-seconds scale, so
// a value bound with DecimalDigits 3 still renders seven digits.
//
// Measured: TemporalParamsDeclareTheScaleFromDecimalDigits shows msodbcsql
// reports SQL_DESC_SCALE 7 under every ColumnSize and DecimalDigits. This test
// asserted three digits and was skipped on the compare leg until that ran.
TEST_F(ScalarConversionLiveTest, TimeParamIsDeclaredAtMaximumScale) {
    SQL_SS_TIME2_STRUCT t2 = {13, 45, 30, 123000000};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIME2, SQL_SS_TIME2, t2, 0, 3), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("13:45:30.1230000", ExecuteAndReadBack());
}

// The 2.x spelling carries a value, not just a typed NULL. SQL_TIME_STRUCT has
// no fraction field at all, so the maximum-scale declaration renders zeros -
// which is the part a unit test cannot see, since it is the declaration and not
// the struct that decides the rendering.
TEST_F(ScalarConversionLiveTest, PlainTimeStructRoundTripsThroughItsDeclaration) {
    SQL_TIME_STRUCT t = {13, 45, 30};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_TIME, SQL_TYPE_TIME, t), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("13:45:30.0000000", ExecuteAndReadBack());
}

// The edges themselves have to work, not just fail one past them - msodbcsql's
// TCTestBoundary is built on these, and ours asserted only the failures.
//
// The offset is +05:45 rather than a half hour because minute arithmetic is
// what :00 and :30 cannot exercise, and its UTC-normalised instant lands
// exactly on the minimum rather than one past it - where an off-by-one in the
// range check would show.
TEST_F(ScalarConversionLiveTest, TemporalBoundaryValuesRoundTrip) {
    SQL_SS_TIME2_STRUCT t2 = {23, 59, 59, 999999900};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIME2, SQL_SS_TIME2, t2, 0, 7), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("23:59:59.9999999", ExecuteAndReadBack());
    ResetParams();

    SQL_TIMESTAMP_STRUCT ceiling = {9999, 12, 31, 23, 59, 59, 999999900};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 121)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, ceiling, 0, 7),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("9999-12-31 23:59:59.9999999", ExecuteAndReadBack());
    ResetParams();

    SQL_TIMESTAMP_STRUCT floor_ts = {1, 1, 1, 0, 0, 0, 0};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 121)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, floor_ts, 0, 7),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("0001-01-01 00:00:00.0000000", ExecuteAndReadBack());
    ResetParams();

    SQL_SS_TIMESTAMPOFFSET_STRUCT dto = {1, 1, 1, 5, 45, 0, 0, 5, 45};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?, 121)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIMESTAMPOFFSET, SQL_SS_TIMESTAMPOFFSET, dto, 0, 7),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("0001-01-01 05:45:00.0000000 +05:45", ExecuteAndReadBack());
}

// A dropped fraction is 22008 for every temporal target.
//
// Measured on both legs; the derivation supports it but is not airtight, so the
// measurement is the authority. ParamToSQLType has two IDS_01_S07 rewrites. The
// one at sqlcfunc.cpp:3128 is unconditional and sits in a switch on fSqlType
// whose case list (:3072-3081) covers every temporal type - SQL_TIMESTAMP,
// SQL_TIME2_MAPPED and SQL_TIMESTAMPOFFSET_MAPPED included, which is what
// SQLBindParameter has already normalised these three targets to. It produces
// IDS_22_008_01, SQLSTATE 22008 (sqlncli_rc.h:1056), and the guard at :3131
// then jumps to ErrorRet, so the second rewrite at :3350 - gated on fCTypeOld,
// in a different switch keyed on the C type - is not reached this way.
//
// Not established: whether any binding reaches :3350 with a temporal fSqlType by
// another route. It would answer 22001 there.
TEST_F(ScalarConversionLiveTest, ADroppedFractionIsAlways22008) {
    SQL_TIMESTAMP_STRUCT ts = {2024, 6, 15, 1, 2, 3, 123400000};
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, ts, 0, 3),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22008");
    ResetParams();

    SQL_SS_TIMESTAMPOFFSET_STRUCT dto = {2024, 6, 15, 1, 2, 3, 123400000, 0, 0};
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIMESTAMPOFFSET, SQL_SS_TIMESTAMPOFFSET, dto, 0, 3),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22008");
    ResetParams();

    SQL_SS_TIME2_STRUCT t2 = {1, 2, 3, 123400000};
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIME2, SQL_SS_TIME2, t2, 0, 3), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22008");
}

// Dropping a whole time component onto a date is the same 22008.
//
// Skipped until AB#47790: SQL_C_TYPE_TIMESTAMP -> SQL_TYPE_DATE is an
// off-diagonal pairing with no conversion matrix row, so this driver rejects it
// at bind with HYC00 and never reaches the truncation. msodbcsql accepts the
// pairing and answers 22008, which is what the converter is already written to
// return - so this only needs the matrix row, not a behaviour change.
TEST_F(ScalarConversionLiveTest, ATimestampWithATimeCannotBecomeADate) {
    GTEST_SKIP() << "SQL_C_TYPE_TIMESTAMP -> SQL_TYPE_DATE is not bindable yet - AB#47790";

    SQL_TIMESTAMP_STRUCT ts = {2024, 6, 15, 12, 0, 0, 0};
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_DATE, ts), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22008");
    ResetParams();

    // Midnight drops nothing, so it converts.
    SQL_TIMESTAMP_STRUCT midnight = {2024, 6, 15, 0, 0, 0, 0};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 23)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_DATE, midnight), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ("2024-06-15", ExecuteAndReadBack());
}

// A temporal parameter is declared at the maximum fractional-seconds scale,
// whatever ColumnSize and DecimalDigits say.
//
// Measured on retail 18.6.2.1: SQL_DESC_SCALE is 7 for every combination below,
// including an explicit DecimalDigits of 0. FixupColumnSizeDecimalDigits
// (sqlcdesc.cpp:11904) derives ColumnSize *from* DecimalDigits and leaves the
// latter alone, so the source reads as though the app's scale survives - it does
// not reach the declaration. sqlccmd.cpp:2806 says the same in passing: "the
// time(n) portion is normalized to maximum precision".
//
// DecimalDigits still bounds the value - a fraction it cannot carry is 22008,
// which ADroppedFractionIsAlways22008 pins.
//
// The NULL rows are the half that is *not* yet measured: typed_null shares
// datetime_metadata, so it declares 7 here too, but no msodbcsql measurement
// covers a NULL temporal parameter. A failure on those rows is the answer.
TEST_F(ScalarConversionLiveTest, TemporalParamsAreDeclaredAtMaximumScale) {
    struct Case {
        SQLSMALLINT c_type;
        SQLSMALLINT sql_type;
        SQLULEN column_size;
        SQLSMALLINT decimal_digits;
        bool null_value;
        const char* what;
    };
    // ColumnSize is varied independently of DecimalDigits so the two inputs can
    // be told apart: 0 is "unstated", 12 is the character width of time(3), and
    // 16 is the width of time(7).
    const Case cases[] = {
        {SQL_C_SS_TIME2, SQL_SS_TIME2, 0, 3, false, "time2 unstated size, scale 3"},
        {SQL_C_SS_TIME2, SQL_SS_TIME2, 12, 3, false, "time2 size matching scale 3"},
        {SQL_C_SS_TIME2, SQL_SS_TIME2, 16, 3, false, "time2 size of scale 7, scale 3"},
        {SQL_C_SS_TIME2, SQL_SS_TIME2, 0, 0, false, "time2 scale 0"},
        {SQL_C_SS_TIME2, SQL_SS_TIME2, 0, 7, false, "time2 scale 7"},
        {SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, 0, 3, false, "datetime2 scale 3"},
        {SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, 0, 0, false, "datetime2 scale 0"},
        {SQL_C_SS_TIMESTAMPOFFSET, SQL_SS_TIMESTAMPOFFSET, 0, 3, false, "datetimeoffset scale 3"},
        {SQL_C_DEFAULT, SQL_SS_TIME2, 0, 3, true, "NULL time2 scale 3"},
        {SQL_C_DEFAULT, SQL_TYPE_TIMESTAMP, 0, 3, true, "NULL datetime2 scale 3"},
        {SQL_C_DEFAULT, SQL_SS_TIMESTAMPOFFSET, 0, 3, true, "NULL datetimeoffset scale 3"},
    };
    for (const Case& c : cases) {
        // Whole seconds, so no case can fail on the truncation rule instead.
        SQL_SS_TIME2_STRUCT t2 = {13, 45, 30, 0};
        SQL_TIMESTAMP_STRUCT ts = {2024, 6, 15, 13, 45, 30, 0};
        SQL_SS_TIMESTAMPOFFSET_STRUCT dto = {2024, 6, 15, 13, 45, 30, 0, 0, 0};

        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        SQLLEN null_ind = SQL_NULL_DATA;
        SQLRETURN rc;
        if (c.null_value) {
            rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT, c.sql_type,
                                  c.column_size, c.decimal_digits, nullptr, 0, &null_ind);
        } else if (c.c_type == SQL_C_SS_TIME2) {
            rc = BindFixed(c.c_type, c.sql_type, t2, c.column_size, c.decimal_digits);
        } else if (c.c_type == SQL_C_TYPE_TIMESTAMP) {
            rc = BindFixed(c.c_type, c.sql_type, ts, c.column_size, c.decimal_digits);
        } else {
            rc = BindFixed(c.c_type, c.sql_type, dto, c.column_size, c.decimal_digits);
        }
        ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLLEN scale = -1;
        ASSERT_SQL_OK(SQLColAttribute(stmt_, 1, SQL_DESC_SCALE, nullptr, 0, nullptr, &scale),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(static_cast<SQLLEN>(7), scale)
            << c.what << ": a temporal parameter is declared at the maximum scale";

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        ResetParams();
    }
}

TEST_F(ScalarConversionLiveTest, TimestampParamRoundTrips) {
    SQL_TIMESTAMP_STRUCT ts = {2024, 6, 15, 12, 30, 45, 0};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 120)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, ts, 0, 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2024-06-15 12:30:45", ExecuteAndReadBack());
}

// The struct is local wall clock and the wire is UTC, so 12:30 at +05:30 must
// reach the server as 07:00Z carrying a +05:30 offset.
TEST_F(ScalarConversionLiveTest, TimestampOffsetIsSentAsUtc) {
    SQL_SS_TIMESTAMPOFFSET_STRUCT dto = {2024, 6, 15, 12, 30, 0, 0, 5, 30};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), CAST(? AS DATETIMEOFFSET(0)), 121)"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIMESTAMPOFFSET, SQL_SS_TIMESTAMPOFFSET, dto, 0, 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2024-06-15 12:30:00 +05:30", ExecuteAndReadBack());
}

// IsValidTimezoneOffsetValue (dataconv.cpp:118) rejects components that disagree
// in sign even when the total is legal, and bounds the total at +/-14:00.
// Checking only the total would accept +5h -30m as +04:30.
TEST_F(ScalarConversionLiveTest, InvalidTimezoneOffsetIs22007) {
    struct Case {
        SQLSMALLINT tz_hour;
        SQLSMALLINT tz_minute;
    };
    for (const Case& c : {Case{5, -30}, Case{-5, 30}, Case{15, 0}, Case{14, 1},
                          Case{0, 60}}) {
        SQL_SS_TIMESTAMPOFFSET_STRUCT dto = {2024, 6, 15, 12, 0, 0, 0, c.tz_hour,
                                             c.tz_minute};
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindFixed(SQL_C_SS_TIMESTAMPOFFSET, SQL_SS_TIMESTAMPOFFSET, dto, 0, 0),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_))
            << "offset " << c.tz_hour << ":" << c.tz_minute;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22007");
        ResetParams();
    }
}

TEST_F(ScalarConversionLiveTest, XmlParamRoundTrips) {
    std::string xml = "<root><a>1</a></root>";
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(128), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_SS_XML, xml), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(xml, ExecuteAndReadBack());
}

// sql_variant wraps the inner declaration rather than declaring itself, so the
// server reports the inner base type.
//
// A narrow payload cannot be sent yet: mssql-tds hard-codes the variant's inner
// context to NVARCHAR and sizes it as UTF-16, so five UTF-8 bytes are rejected
// as "String length (5 characters) exceeds schema size (2 characters)".
// msodbcsql passes both assertions today. AB#47800.
TEST_F(ScalarConversionLiveTest, VariantParamWrapsItsInnerType) {
    GTEST_SKIP() << "sql_variant carries no varchar payload yet - AB#47800";

    ASSERT_SQL_OK(Prepare(kBaseTypeQuery), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_SS_VARIANT, "hello", 8), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("varchar", ExecuteAndReadBack());
    ResetParams();

    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_SS_VARIANT, "hello", 8), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("hello", ExecuteAndReadBack());
}

// sql_variant cannot hold a max type (server error 529), so ColumnSize 0 is read
// as "unstated" and declared at the non-max ceiling instead of meaning max the
// way it does for a plain varchar.
//
// Blocked on the same narrow-payload gap as VariantParamWrapsItsInnerType,
// AB#47800.
TEST_F(ScalarConversionLiveTest, VariantWithNoColumnSizeIsNotAMaxType) {
    GTEST_SKIP() << "sql_variant carries no varchar payload yet - AB#47800";

    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindNarrow(SQL_SS_VARIANT, "hello"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("hello", ExecuteAndReadBack());
}

// A defaulted binding of every scalar row must produce a typed NULL from
// ParameterType alone, which is the case that regressed into varchar(max)
// before AB#47500.
//
// The declaration is read off the result column rather than the value: a NULL
// carries no base type through SQL_VARIANT_PROPERTY, but `SELECT ? AS v` still
// takes the parameter's declared type for its output column, so SQLColAttribute
// reports what the server was told.
TEST_F(ScalarConversionLiveTest, DefaultCTypeNullBindsAndExecutes) {
    struct Case {
        SQLSMALLINT sql_type;
        SQLULEN column_size;
        SQLSMALLINT decimal_digits;
        const char* base_type;
    };
    const Case cases[] = {
        {SQL_BIT, 0, 0, "bit"},
        {SQL_REAL, 0, 0, "real"},
        {SQL_DOUBLE, 0, 0, "float"},
        {SQL_DECIMAL, 18, 2, "decimal"},
        {SQL_NUMERIC, 18, 2, "numeric"},
        {SQL_GUID, 0, 0, "uniqueidentifier"},
        {SQL_TYPE_DATE, 0, 0, "date"},
        {SQL_TYPE_TIME, 0, 3, "time"},
        {SQL_TYPE_TIMESTAMP, 0, 3, "datetime2"},
        {SQL_SS_TIMESTAMPOFFSET, 0, 3, "datetimeoffset"},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        SQLLEN null_ind = SQL_NULL_DATA;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT, c.sql_type,
                                       c.column_size, c.decimal_digits, nullptr, 0, &null_ind),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLTCHAR name[64] = {0};
        SQLSMALLINT name_len = 0;
        ASSERT_SQL_OK(SQLColAttribute(stmt_, 1, SQL_DESC_TYPE_NAME, name, sizeof(name),
                                      &name_len, nullptr),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.base_type, ODBCTestUtils::ToNarrow(SqlTString(name)))
            << "sql_type " << c.sql_type;

        // The value is still NULL, which is the other half of a typed NULL.
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("<null>", GetColumnChar()) << "sql_type " << c.sql_type;
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        ResetParams();
    }
}

// An application may pass no StrLen_or_IndPtr at all. A fixed-width C type has no
// length to state, but the null pointer still becomes SQL_NTS (-3) internally, so
// this pins that no scalar reader consults it. Values and expectations mirror the
// round-trip test for each type, so only the missing indicator differs.
// NullIndicatorPointerMeansNullTerminated covers the character half, where
// SQL_NTS does mean something.
TEST_F(ScalarConversionLiveTest, ScalarParamsBindWithoutAnIndicatorPointer) {
    auto bind = [&](SQLSMALLINT c_type, SQLSMALLINT sql_type, void* buf, SQLLEN buf_len,
                    SQLSMALLINT scale) {
        return SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, c_type, sql_type, 0, scale, buf,
                               buf_len, nullptr);
    };

    SQLCHAR bit = 1;
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(4), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(bind(SQL_C_BIT, SQL_BIT, &bit, sizeof(bit), 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", ExecuteAndReadBack());
    ResetParams();

    double dbl = 1.5;
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(bind(SQL_C_DOUBLE, SQL_DOUBLE, &dbl, sizeof(dbl), 0), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ("1.5", ExecuteAndReadBack());
    ResetParams();

    float real = -2.25f;
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(bind(SQL_C_FLOAT, SQL_REAL, &real, sizeof(real), 0), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ("-2.25", ExecuteAndReadBack());
    ResetParams();

    SQLGUID guid = {0x01234567,
                    0x89AB,
                    0xCDEF,
                    {0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF}};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), ?)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(bind(SQL_C_GUID, SQL_GUID, &guid, sizeof(guid), 0), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("01234567-89AB-CDEF-0123-456789ABCDEF", ExecuteAndReadBack());
    ResetParams();

    SQL_DATE_STRUCT date = {2024, 2, 29};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 23)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(bind(SQL_C_TYPE_DATE, SQL_TYPE_DATE, &date, sizeof(date), 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2024-02-29", ExecuteAndReadBack());
    ResetParams();

    SQL_SS_TIME2_STRUCT t2 = {13, 45, 30, 123000000};
    // Cast to a fixed scale server-side: a temporal parameter is declared at
    // the maximum scale (see TemporalParamsAreDeclaredAtMaximumScale), and that
    // is not what this test is about.
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), CAST(? AS TIME(0)), 108)"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(bind(SQL_C_SS_TIME2, SQL_SS_TIME2, &t2, sizeof(t2), 3), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ("13:45:30", ExecuteAndReadBack());
    ResetParams();

    SQL_TIMESTAMP_STRUCT ts = {2024, 6, 15, 12, 30, 45, 0};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(32), ?, 120)"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(bind(SQL_C_TYPE_TIMESTAMP, SQL_TYPE_TIMESTAMP, &ts, sizeof(ts), 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2024-06-15 12:30:45", ExecuteAndReadBack());
    ResetParams();

    SQL_SS_TIMESTAMPOFFSET_STRUCT dto = {2024, 6, 15, 12, 30, 0, 0, 5, 30};
    ASSERT_SQL_OK(Prepare("SELECT CONVERT(VARCHAR(64), CAST(? AS DATETIMEOFFSET(0)), 121)"),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(bind(SQL_C_SS_TIMESTAMPOFFSET, SQL_SS_TIMESTAMPOFFSET, &dto, sizeof(dto), 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2024-06-15 12:30:00 +05:30", ExecuteAndReadBack());
}

// A null value buffer is SQL NULL, whatever the indicator says. msodbcsql sets
// DBRPCVALUE_NULL on the bound-parameter path when lpbData is null
// (sqlcfunc.cpp:2549), before any length check, so the indicator's value never
// matters. Measured on both legs; this driver used to answer HY009 and now
// matches.
// A non-NULL parameter with a null ParameterValuePtr is HY009 here.
//
// Both drivers reject it, with different states: retail 18.6.2.1 answers HY090
// ("Invalid string or buffer length") at SQLExecute, measured on ADO build
// 172202 on both Build Linux and Build Windows. sqlcfunc.cpp:2549 reads as
// though a null buffer is simply taken as NULL, but that reading does not hold
// for this input - do not re-derive it from source. Registered deviation 7.
TEST_F(ScalarConversionLiveTest, NullDataPointerWithZeroLengthIsHy009) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLLEN zero = 0;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR, 10, 0,
                                    nullptr, 0, &zero);
    if (SQL_SUCCEEDED(rc)) {
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    }
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY009");
}
