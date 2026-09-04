// Copyright (c) Microsoft Corporation. All rights reserved.
// param_cross_conversions_test.cpp  -  E2E tests for cross-family parameter
// conversion: an integer C type bound against a character ParameterType, and a
// character C type bound against an integer ParameterType.
//
// Same-family character conversions live in param_char_conversions_test.cpp, and
// statement lifecycle in execute_test.cpp.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

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

// A character C buffer against an integer wire type is supplied at execution
// like any other pairing: it cannot be PLP-framed, so the chunks are collected
// and the complete value is converted by the same code the materialized path
// uses. Text parses to an integer here exactly as it does when bound directly
// (AB#47590).
//
// msodbcsql returns SQL_NEED_DATA for this pairing at SQLExecute but does not
// actually stream it: SQLPutData then rejects with HY019 ("Processing of fixed
// length targets cannot be spread over multiple calls to SQLPutData"), so the
// parity run stays skipped.
TEST_F(CrossConversionLiveTest, CrossFamilyDataAtExecutionConvertsToInteger) {
    SKIP_IF_COMPARING_MSODBCSQL();

    for (SQLSMALLINT c_type : {SQL_C_CHAR, SQL_C_WCHAR}) {
        ASSERT_SQL_OK(Prepare("SELECT ? + 1"), SQL_HANDLE_STMT, stmt_);

        SQLLEN ind = SQL_LEN_DATA_AT_EXEC(0);
        SQLCHAR token = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, c_type, SQL_INTEGER,
                                       0, 0, &token, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_)) << "c type " << c_type;
        SQLPOINTER returned = nullptr;
        ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &returned));

        if (c_type == SQL_C_WCHAR) {
            const SQLCHAR wide[] = {'4', 0, '1', 0};
            ASSERT_SQL_OK(SQLPutData(stmt_, const_cast<SQLCHAR*>(wide), sizeof(wide)),
                          SQL_HANDLE_STMT, stmt_);
        } else {
            const SQLCHAR narrow[] = {'4', '1'};
            ASSERT_SQL_OK(SQLPutData(stmt_, const_cast<SQLCHAR*>(narrow), sizeof(narrow)),
                          SQL_HANDLE_STMT, stmt_);
        }

        ASSERT_SQL_OK(SQLParamData(stmt_, &returned), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("42", GetColumnChar(1)) << "c type " << c_type;
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
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
