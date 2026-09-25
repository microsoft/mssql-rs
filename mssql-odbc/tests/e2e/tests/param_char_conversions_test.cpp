// Copyright (c) Microsoft Corporation. All rights reserved.
// param_char_conversions_test.cpp  -  E2E tests for character parameter conversion:
// SQL_C_CHAR / SQL_C_WCHAR bound against char, varchar, text and their wide
// counterparts. Covers the declared wire type, ColumnSize semantics, truncation
// and its blank exemption, encoding, and the indicator/terminator rules.
//
// Statement lifecycle (prepare, re-execute, cached handles, data-at-execution)
// lives in execute_test.cpp.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <string>
#include <vector>

// From msodbcsql.h, which this suite does not include (see the note in
// transaction_test.cpp): `SQL_COPT_SS_BASE_EX + 3`, taking SQL_WARN_NO /
// SQL_WARN_YES. Opts in to the `01000` warning for a character the target
// collation's code page could not represent (AB#47598).
#ifndef SQL_COPT_SS_WARN_ON_CP_ERROR
#define SQL_COPT_SS_WARN_ON_CP_ERROR 1243
#endif
#ifndef SQL_WARN_NO
#define SQL_WARN_NO 0L
#endif
#ifndef SQL_WARN_YES
#define SQL_WARN_YES 1L
#endif

// SQL Server's sql_variant SQL type identifier, also from msodbcsql.h.
#ifndef SQL_SS_VARIANT
#define SQL_SS_VARIANT (-150)
#endif

// An ASCII value held in whichever width the bound C type reads, so one literal
// drives both SQL_C_CHAR and SQL_C_WCHAR cases of a table-driven test and the
// byte-count arithmetic lives in one place. Must outlive the SQLExecute that
// reads it.
class AsciiParam {
public:
    AsciiParam(SQLSMALLINT c_type, const std::string& text)
        : c_type_(c_type),
          narrow_(text.begin(), text.end()),
          indicator_(static_cast<SQLLEN>(c_type == SQL_C_WCHAR
                                             ? text.size() * sizeof(SQLWCHAR)
                                             : text.size())) {
        wide_.reserve(text.size());
        for (unsigned char ch : text) {
            // char is signed, so widening 0xC3 directly would give 0xFFC3, and a
            // UTF-8 sequence would widen byte by byte. Both are silent.
            EXPECT_LT(ch, 0x80) << "AsciiParam holds ASCII only";
            wide_.push_back(ch);
        }
    }

    SQLSMALLINT c_type() const { return c_type_; }
    SQLLEN length() const { return indicator_; }
    SQLLEN* indicator() { return &indicator_; }
    void* data() {
        return c_type_ == SQL_C_WCHAR ? static_cast<void*>(wide_.data())
                                      : static_cast<void*>(narrow_.data());
    }

private:
    SQLSMALLINT c_type_;
    std::vector<SQLCHAR> narrow_;
    std::vector<SQLWCHAR> wide_;
    SQLLEN indicator_;
};

class CharConversionLiveTest : public ODBCTest {
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

    // Bind |value| in the width its own C type dictates.
    SQLRETURN BindAscii(SQLUSMALLINT param, AsciiParam& value,
                        SQLSMALLINT sql_type, SQLULEN column_size) {
        return SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, value.c_type(),
                                sql_type, column_size, 0, value.data(),
                                value.length(), value.indicator());
    }

    // Read column |col| of the current row as a narrow string.
    std::string GetColumnChar(SQLUSMALLINT col, SQLLEN* ind_out = nullptr) {
        SQLCHAR buf[512] = {0};
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, col, SQL_C_CHAR, buf, sizeof(buf), &ind);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        if (ind_out) {
            *ind_out = ind;
        }
        if (ind == SQL_NULL_DATA) {
            return std::string();
        }
        return std::string(reinterpret_cast<const char*>(buf));
    }

    // serialize_string picks the code page from the collation's LCID alone, so
    // only the LCID has to be Latin1 for U+65E5 to be unmappable. The parameter
    // carries the *database* collation, which need not match the instance's.
    bool DatabaseIsLatin1() {
        return DatabaseCollation().find("Latin1_General") != std::string::npos;
    }

    // The database collation name, or an empty string if it could not be read.
    std::string DatabaseCollation() {
        // Each step returns early: EXPECT_* is non-fatal, and falling through to
        // GetColumnChar on an unfetched row reports a collation mismatch instead
        // of the prepare or fetch that actually failed.
        EXPECT_SQL_OK(
            Prepare("SELECT CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation')"
                    " AS VARCHAR(128))"),
            SQL_HANDLE_STMT, stmt_);
        if (::testing::Test::HasFailure()) {
            return std::string();
        }
        EXPECT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        if (::testing::Test::HasFailure()) {
            return std::string();
        }
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        if (::testing::Test::HasFailure()) {
            return std::string();
        }
        const std::string collation = GetColumnChar(1);
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        return collation;
    }

    // The server-side session id, so a test can tell a surviving connection from
    // a transparently rebuilt one.
    std::string CurrentSpid() {
        EXPECT_SQL_OK(Prepare("SELECT CAST(@@SPID AS VARCHAR(16))"), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        const std::string spid = GetColumnChar(1);
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        return spid;
    }

    // A round trip on the same connection after a failed execute. Any cursor the
    // caller left open is closed first, since reusing the statement with one live
    // is 24000 and would mask what this checks.
    //
    // The SPID is the assertion that matters: session recovery is negotiated by
    // default, so a driver that killed the connection would reconnect here and
    // still answer 'alive'. Only an unchanged SPID says the request was retracted
    // rather than the session rebuilt.
    void ExpectSameSessionStillUsable(const std::string& expected_spid) {
        SQLCloseCursor(stmt_);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

        SQLUINTEGER dead = SQL_CD_TRUE;
        ASSERT_SQL_OK(
            SQLGetConnectAttr(dbc_, SQL_ATTR_CONNECTION_DEAD, &dead, SQL_IS_UINTEGER, nullptr),
            SQL_HANDLE_DBC, dbc_);
        EXPECT_EQ(SQL_CD_FALSE, dead) << "a retracted request must not cost the connection";

        ASSERT_SQL_OK(Prepare("SELECT 'alive'"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("alive", GetColumnChar(1));
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

        EXPECT_EQ(expected_spid, CurrentSpid())
            << "the session was rebuilt, so the request cost the connection";
    }
};

// The declared wire type follows ParameterType, not the C type that was bound,
// so a cross-family pairing transcodes instead of being rejected.
// SQL_VARIANT_PROPERTY reports the base type the server actually received, which
// the round-tripped text alone cannot show. `text`/`ntext` are excluded because
// they cannot be cast to sql_variant.
TEST_F(CharConversionLiveTest, CharParamDeclaresTheParameterType) {
    struct Case {
        SQLSMALLINT c_type;
        SQLSMALLINT sql_type;
        const char* base_type;
    };
    const Case cases[] = {
        {SQL_C_CHAR, SQL_VARCHAR, "varchar"},
        {SQL_C_CHAR, SQL_WVARCHAR, "nvarchar"},
        {SQL_C_CHAR, SQL_CHAR, "char"},
        {SQL_C_CHAR, SQL_WCHAR, "nchar"},
        {SQL_C_WCHAR, SQL_VARCHAR, "varchar"},
        {SQL_C_WCHAR, SQL_WVARCHAR, "nvarchar"},
        {SQL_C_WCHAR, SQL_CHAR, "char"},
        {SQL_C_WCHAR, SQL_WCHAR, "nchar"},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT CAST(SQL_VARIANT_PROPERTY(CAST(? AS SQL_VARIANT),"
                              " 'BaseType') AS VARCHAR(32))"),
                      SQL_HANDLE_STMT, stmt_);

        AsciiParam value(c.c_type, "abc");
        ASSERT_SQL_OK(BindAscii(1, value, c.sql_type, 8), SQL_HANDLE_STMT,
                      stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.base_type, GetColumnChar(1))
            << "c type " << c.c_type << " sql type " << c.sql_type;

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// Non-ASCII must survive a cross-family bind in both directions.
//
// Asserted server-side with UNICODE(): echoing the parameter back would decode
// symmetrically and pass even if the payload were mis-encoded.
//
// Driver-specific on both ends: this driver's SQL_C_CHAR is UTF-8, while
// msodbcsql reads and writes it in the client code page, so it takes the bound
// 0xC3 0xA9 as two CP1252 characters.
TEST_F(CharConversionLiveTest, CrossFamilyCharParamRoundTripsNonAscii) {
    SKIP_IF_COMPARING_MSODBCSQL();

    // U+00E9 is the fourth character; 233 is its code point.
    const char* kProbe =
        "SELECT CAST(UNICODE(SUBSTRING(?, 4, 1)) AS VARCHAR(16))";

    ASSERT_SQL_OK(Prepare(kProbe), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> utf8 = {'c', 'a', 'f', 0xC3, 0xA9};
    SQLLEN utf8_ind = static_cast<SQLLEN>(utf8.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_WVARCHAR, 8, 0, utf8.data(), utf8_ind,
                                   &utf8_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("233", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(Prepare(kProbe), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR wide[] = {'c', 'a', 'f', 0x00E9};
    SQLLEN wide_ind = static_cast<SQLLEN>(sizeof(wide));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 8, 0, wide, wide_ind, &wide_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("233", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A value longer than ColumnSize is 22001 at execute, for every target that
// carries a declared length; a value exactly at it is not.
TEST_F(CharConversionLiveTest, OverlongCharParamIs22001) {
    struct Case {
        SQLSMALLINT c_type;
        SQLSMALLINT sql_type;
        // msodbcsql lets one character of narrow -> wide overflow escape, so for
        // those two the 22001 is asserted by NarrowToWideOverlongParamIs22001,
        // which skips the comparison run. Fitting exactly diverges nowhere, so
        // that half runs for every pairing.
        bool msodbcsql_diverges;
    };
    const Case cases[] = {
        {SQL_C_CHAR, SQL_CHAR, false},     {SQL_C_CHAR, SQL_VARCHAR, false},
        {SQL_C_WCHAR, SQL_WCHAR, false},   {SQL_C_WCHAR, SQL_WVARCHAR, false},
        {SQL_C_WCHAR, SQL_CHAR, false},    {SQL_C_WCHAR, SQL_VARCHAR, false},
        {SQL_C_CHAR, SQL_WCHAR, true},     {SQL_C_CHAR, SQL_WVARCHAR, true},
    };
    for (const Case& c : cases) {
        AsciiParam value(c.c_type, "abcd");

        // Exactly at the declared length is not truncation.
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindAscii(1, value, c.sql_type, 4), SQL_HANDLE_STMT,
                      stmt_);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("abcd", GetColumnChar(1))
            << "c type " << c.c_type << " sql type " << c.sql_type;
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);

        if (c.msodbcsql_diverges) {
            continue;
        }

        // One character past it is.
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(BindAscii(1, value, c.sql_type, 3), SQL_HANDLE_STMT,
                      stmt_);

        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_))
            << "c type " << c.c_type << " sql type " << c.sql_type;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// The narrow -> wide direction, split out because msodbcsql gets it wrong: its
// per-character walk reads the running count before incrementing it
// (sqlcfunc.cpp:2926, cchDest++ at :2931), so exactly one character of overflow
// escapes the check. Measured against retail 18.05.0002: "abcd" into a
// wvarchar(3) returns SQL_SUCCESS with no diagnostic and the server receives
// nvarchar(3) holding "abc" - silently truncated. "abcde" is two characters
// over and is correctly rejected, and the narrow -> narrow control rejects at
// four, which is what pins the off-by-one to this arm.
//
// Not widened on the wire: an earlier revision of this comment claimed
// stMaxLen = max(*pstMaxLen, stLen) (sqlcmisc.cpp:7458) sends more characters
// than declared. SQL_VARIANT_PROPERTY 'MaxLength' reports 6 bytes, i.e.
// nvarchar(3), the declared size. Only the debug assert reproduces from source.
TEST_F(CharConversionLiveTest, NarrowToWideOverlongParamIs22001) {
    SKIP_IF_COMPARING_MSODBCSQL();

    for (SQLSMALLINT sql_type : {SQL_WCHAR, SQL_WVARCHAR, SQL_WLONGVARCHAR}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
        std::vector<SQLCHAR> value = {'a', 'b', 'c', 'd'};
        SQLLEN ind = static_cast<SQLLEN>(value.size());
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                       sql_type, 3, 0, value.data(), ind, &ind),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "sql type " << sql_type;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }

    // "cafe" with an acute accent is four UTF-16 units, so it fits an
    // nvarchar(4) even though the bound buffer holds five UTF-8 bytes.
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> multibyte = {'c', 'a', 'f', 0xC3, 0xA9};
    SQLLEN multibyte_ind = static_cast<SQLLEN>(multibyte.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_WVARCHAR, 4, 0, multibyte.data(),
                                   multibyte_ind, &multibyte_ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Blank overflow on the narrow -> wide path is trimmed, and retail 18.05.0002
// agrees: it also returns "abc" as nvarchar(3), so this is not a behavioral
// divergence on that build. The skip is retained only because the case has not
// been measured against the build CI actually compares against - retail
// 18.6.2.1, pinned by msodbcsqlVersion - and debug 18.06.0002 aborts on
// assert(*pstMaxLen >= stLen) (sqlcmisc.cpp:7458) rather than answering. Measure
// 18.6.2.1 and drop the skip if it matches. Note all six characters reach
// DescribeRPCParam untrimmed yet retail still declares nvarchar(3), so the
// fallthrough at :7459 does not describe what the retail binary does.
TEST_F(CharConversionLiveTest, NarrowToWideOverflowingBlanksAreTrimmed) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> value = {'a', 'b', 'c', ' ', ' ', ' '};
    SQLLEN ind = static_cast<SQLLEN>(value.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_WVARCHAR, 3, 0, value.data(), ind, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abc", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A narrow parameter is measured in UTF-8 bytes, but varchar(n) bounds the bytes
// of the database collation, which serialize_string only applies further down.
// Under a single-byte collation the two disagree for any non-ASCII value: "cafe"
// with an acute accent is five UTF-8 bytes and so fails varchar(4), even though
// the four bytes that would have reached the server fit. Under a UTF-8 collation
// the counts agree and the 22001 is correct. The second half pins the gap from
// the passing side: the count validated is 5, the value stored is 4 long.
//
// A narrow source is measured in UTF-16 units, the same unit a wide source
// uses, so the two C types agree on one value. Measuring SQL_C_CHAR in its own
// UTF-8 bytes rejected "cafe"-with-an-acute from a varchar(4) that SQL_C_WCHAR
// was allowed to fill, on data the server accepts - LEN returns 4 below.
//
// Compare is skipped because msodbcsql's answer depends on the client code
// page. TDS carries a collation with char data, so it normally ships SQL_C_CHAR
// bytes under a declared collation and lets the server convert - a CP1252 client
// counts 4 and agrees with us. A UTF-8 client trips DoCharToCharConversion
// (sqlcprot.h:4113), so msodbcsql transcodes, yet still measures the
// pre-transcode UTF-8 bytes: it counts 5 and rejects a value that the 4 bytes it
// actually sends would fit. That defect is deliberately not replicated.
//
// The count is still approximate, and now errs low rather than high: under a
// DBCS or _UTF8 collation the server bound is larger than the units we counted,
// so an over-long value reaches serialize_char_varchar_direct and fails there
// with an opaque driver error rather than 22001 (AB#47584).
TEST_F(CharConversionLiveTest, NarrowMultibyteIsMeasuredInUtf16Units) {
    SKIP_IF_COMPARING_MSODBCSQL();

    std::vector<SQLCHAR> value = {'c', 'a', 'f', 0xC3, 0xA9};
    SQLLEN ind = static_cast<SQLLEN>(value.size());

    // Four characters, five UTF-8 bytes: a varchar(4) holds it.
    ASSERT_SQL_OK(Prepare("SELECT LEN(?) AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 4, 0, value.data(), ind, &ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("4", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    // One character less and it is truncation, measured in the same units.
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 3, 0, value.data(), ind, &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    // The wide C type reaches the same verdict on the accepting side. The
    // rejecting side is not asserted here - a refused bind sends nothing, so the
    // server cannot observe it, and both_character_c_types_measure_a_value_alike
    // covers all four combinations.
    std::vector<SQLWCHAR> wide = {'c', 'a', 'f', 0x00E9};
    SQLLEN wind = static_cast<SQLLEN>(wide.size() * sizeof(SQLWCHAR));
    ASSERT_SQL_OK(Prepare("SELECT LEN(?) AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 4, 0, wide.data(), wind, &wind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("4", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The wide counterpart SQL_WLONGVARCHAR is covered by
// NarrowToWideOverlongParamIs22001 instead: it is a narrow -> wide binding, and
// msodbcsql mishandles those.
TEST_F(CharConversionLiveTest, TextParamIsBoundedByColumnSize) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> value = {'a', 'b', 'c', 'd'};
    SQLLEN ind = static_cast<SQLLEN>(value.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_LONGVARCHAR, 3, 0, value.data(), ind,
                                   &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    // The bound is enforced with the same blank exemption as the sized types.
    // This is the first case here that executes a non-NULL long character
    // parameter, so a server-side failure would point at the declaration rather
    // than at the trimming.
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> padded = {'a', 'b', 'c', ' ', ' '};
    SQLLEN padded_ind = static_cast<SQLLEN>(padded.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_LONGVARCHAR, 3, 0, padded.data(),
                                   padded_ind, &padded_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abc", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Overflow made up only of blanks is trimmed without a diagnostic - msodbcsql
// checks the overflow with CheckTrailingChars / CheckTrailingWChars before
// raising 22001, picking between them on the C type (sqlcfunc.cpp:2957). Both
// arms of that ternary are covered here.
TEST_F(CharConversionLiveTest, OverflowingBlanksAreTrimmedSilently) {
    struct Case {
        SQLSMALLINT c_type;
        SQLSMALLINT sql_type;
    };
    const Case cases[] = {
        {SQL_C_CHAR, SQL_VARCHAR},
        {SQL_C_CHAR, SQL_CHAR},
        {SQL_C_WCHAR, SQL_WVARCHAR},
        {SQL_C_WCHAR, SQL_VARCHAR},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        AsciiParam value(c.c_type, "abc   ");
        ASSERT_SQL_OK(BindAscii(1, value, c.sql_type, 3), SQL_HANDLE_STMT,
                      stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("abc", GetColumnChar(1))
            << "c type " << c.c_type << " sql type " << c.sql_type;
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// Blanks are only dropped to make a value fit. Nothing above trims a value that
// already fits, so a driver that trimmed unconditionally would pass every one of
// those tests; DATALENGTH 4 on a varchar(8) is what rules that out.
TEST_F(CharConversionLiveTest, TrailingBlanksThatFitAreKept) {
    for (SQLSMALLINT sql_type : {SQL_VARCHAR, SQL_WVARCHAR}) {
        ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"),
                      SQL_HANDLE_STMT, stmt_);

        std::vector<SQLCHAR> value = {'a', 'b', 'c', ' '};
        SQLLEN ind = static_cast<SQLLEN>(value.size());
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                       sql_type, 8, 0, value.data(), ind, &ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(sql_type == SQL_WVARCHAR ? "8" : "4", GetColumnChar(1))
            << "sql type " << sql_type;
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// DATALENGTH is measured on the server, so it shows the length the parameter
// was actually declared with: the fixed-length types pad out to ColumnSize and
// the variable-length ones do not. A driver that ignored ParameterType and sent
// everything as varchar(max) would report the payload length for all six rows.
TEST_F(CharConversionLiveTest, DeclaredLengthReachesTheServer) {
    struct Case {
        SQLSMALLINT c_type;
        SQLSMALLINT sql_type;
        const char* data_length;
    };
    const Case cases[] = {
        {SQL_C_CHAR, SQL_CHAR, "8"},      {SQL_C_CHAR, SQL_VARCHAR, "3"},
        {SQL_C_CHAR, SQL_WCHAR, "16"},    {SQL_C_WCHAR, SQL_WCHAR, "16"},
        {SQL_C_WCHAR, SQL_WVARCHAR, "6"}, {SQL_C_WCHAR, SQL_VARCHAR, "3"},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"),
                      SQL_HANDLE_STMT, stmt_);

        AsciiParam value(c.c_type, "abc");
        ASSERT_SQL_OK(BindAscii(1, value, c.sql_type, 8), SQL_HANDLE_STMT,
                      stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.data_length, GetColumnChar(1))
            << "c type " << c.c_type << " sql type " << c.sql_type;

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// ColumnSize 0 is the unbounded sentinel. These payloads are past the non-max
// ceilings (8000 bytes, 4000 units), so they only survive if the parameter was
// declared varchar(max)/nvarchar(max) rather than a bounded length.
TEST_F(CharConversionLiveTest, UnboundedColumnSizeCarriesOversizedValues) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> narrow(9000, 'x');
    SQLLEN narrow_ind = static_cast<SQLLEN>(narrow.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 0, 0, narrow.data(), narrow_ind,
                                   &narrow_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("9000", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);
    std::vector<SQLWCHAR> wide(5000, 'x');
    SQLLEN wide_ind = static_cast<SQLLEN>(wide.size() * sizeof(SQLWCHAR));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_WVARCHAR, 0, 0, wide.data(), wide_ind,
                                   &wide_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("10000", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// ColumnSize 0 is `max` even for a value that would fit a bounded length, so the
// parameter cannot be cast to sql_variant - sql_variant has no `max` member. The
// application has to pass a real ColumnSize, which
// CharParamDeclaresTheParameterType covers.
//
// Asserted rather than fixed because msodbcsql 18.6 declares `max` here too and
// fails identically, so deriving the length from the data instead would be the
// divergence (AB#47533).
//
// Benefits-from-mock-tds: a mock TDS server could assert the RPC parameter was
// declared varchar(max)/nvarchar(max) directly.
TEST_F(CharConversionLiveTest, UnboundedColumnSizeIsMaxEvenForASmallValue) {
    const std::pair<SQLSMALLINT, SQLSMALLINT> cases[] = {
        {SQL_C_CHAR, SQL_VARCHAR},
        {SQL_C_WCHAR, SQL_WVARCHAR},
    };
    for (const auto& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT CAST(? AS SQL_VARIANT)"), SQL_HANDLE_STMT,
                      stmt_);

        AsciiParam value(c.first, "abc");
        ASSERT_SQL_OK(BindAscii(1, value, c.second, 0), SQL_HANDLE_STMT, stmt_);

        EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_)) << "sql type " << c.second;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22018");
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// A zero-length value is an empty string, not a NULL - only SQL_NULL_DATA in the
// indicator means NULL. The fixed-length targets still pad out to ColumnSize,
// which is the one case where an empty value has a non-zero DATALENGTH.
TEST_F(CharConversionLiveTest, EmptyCharParamIsNotNull) {
    struct Case {
        SQLSMALLINT sql_type;
        const char* data_length;
    };
    const Case cases[] = {
        {SQL_VARCHAR, "0"},
        {SQL_WVARCHAR, "0"},
        {SQL_CHAR, "8"},
        {SQL_WCHAR, "16"},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(
            Prepare("SELECT COALESCE(CAST(DATALENGTH(?) AS VARCHAR(16)), 'null')"),
            SQL_HANDLE_STMT, stmt_);

        SQLCHAR buf[1] = {0};
        SQLLEN ind = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                       c.sql_type, 8, 0, buf, sizeof(buf), &ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.data_length, GetColumnChar(1))
            << "sql type " << c.sql_type;

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// The fixed-length types pad with spaces, not NULs. DeclaredLengthReachesTheServer
// only pins the padded length, which a NUL-padding bug would satisfy, and an
// equality test would not help either - SQL Server ignores trailing spaces when
// comparing. Reading the pad character directly is what discriminates: 32 is a
// space, 0 would be a NUL.
//
// Also the one place the mechanism differs: we send the actual length and let the
// server pad, msodbcsql pads client-side under SQL_COPT_SS_ANSI_NPW
// (sqlcmisc.cpp:7466).
TEST_F(CharConversionLiveTest, FixedLengthCharParamIsSpacePadded) {
    for (SQLSMALLINT sql_type : {SQL_CHAR, SQL_WCHAR}) {
        ASSERT_SQL_OK(
            Prepare("SELECT CAST(UNICODE(SUBSTRING(?, 5, 1)) AS VARCHAR(16))"),
            SQL_HANDLE_STMT, stmt_);

        std::vector<SQLCHAR> value = {'a', 'b', 'c'};
        SQLLEN ind = static_cast<SQLLEN>(value.size());
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                       sql_type, 8, 0, value.data(), ind, &ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("32", GetColumnChar(1)) << "sql type " << sql_type;
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// SQL_NTS: the terminator sets the length before any transcode, so the NUL must
// not reach the server. The narrow case crosses families, where a DATALENGTH of
// 6 is three UTF-16 units and 8 would mean the NUL was included; the wide case
// scans for a UTF-16 terminator instead, a separate path.
TEST_F(CharConversionLiveTest, NullTerminatedBuffersDropTheNul) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> narrow = {'a', 'b', 'c', '\0'};
    SQLLEN narrow_ind = SQL_NTS;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_WVARCHAR, 8, 0, narrow.data(),
                                   static_cast<SQLLEN>(narrow.size()),
                                   &narrow_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("6", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);

    SQLWCHAR wide[] = {'a', 'b', 'c', 0};
    SQLLEN wide_ind = SQL_NTS;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_WVARCHAR, 8, 0, wide,
                                   static_cast<SQLLEN>(sizeof(wide)),
                                   &wide_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("6", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// An explicit indicator means the buffer is not terminated, so a NUL inside the
// value is data and must reach the server. A driver that stopped at the first
// NUL would report DATALENGTH 1 here.
TEST_F(CharConversionLiveTest, EmbeddedNulSurvivesAnExplicitLength) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> narrow = {'a', '\0', 'b'};
    SQLLEN narrow_ind = static_cast<SQLLEN>(narrow.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 8, 0, narrow.data(), narrow_ind,
                                   &narrow_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("3", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    // The NUL is the value's second character, not a terminator that shortened it.
    ASSERT_SQL_OK(
        Prepare("SELECT CAST(UNICODE(SUBSTRING(?, 2, 1)) AS VARCHAR(16))"),
        SQL_HANDLE_STMT, stmt_);
    SQLWCHAR wide[] = {'a', 0, 'b'};
    SQLLEN wide_ind = static_cast<SQLLEN>(sizeof(wide));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_WVARCHAR, 8, 0, wide, wide_ind,
                                   &wide_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("0", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A byte count that is not a whole number of UTF-16 units is floored, not
// rejected - msodbcsql does the same with cbData &= ~1 (sqlcfunc.cpp:2862).
// Seven bytes over a four-unit buffer must yield three characters.
TEST_F(CharConversionLiveTest, OddByteCountOnWideBufferIsFloored) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR wide[] = {'a', 'b', 'c', 'd'};
    SQLLEN ind = 7;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_WVARCHAR, 8, 0, wide,
                                   static_cast<SQLLEN>(sizeof(wide)), &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abc", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Truncation cuts on a UTF-16 unit boundary, which could split a surrogate pair
// in half. It cannot: the trailing units must all be blanks for the trim to
// happen at all, and a low surrogate is not a blank, so the pair raises 22001
// instead. One unit further and it goes through whole - DATALENGTH 8 is four
// units, so neither half was dropped.
TEST_F(CharConversionLiveTest, SurrogatePairIsNotSplitAtTheBoundary) {
    SQLWCHAR wide[] = {0x0061, 0x0062, 0xD83D, 0xDE00};
    SQLLEN len = static_cast<SQLLEN>(sizeof(wide));

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    SQLLEN ind = len;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_WVARCHAR, 3, 0, wide, len, &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);
    ind = len;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_WVARCHAR, 4, 0, wide, len, &ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("8", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The declaration is rebuilt from the binding on every execute, so a longer
// value against the same ColumnSize must fail even though the first execute
// succeeded - and the statement must still be usable afterwards.
TEST_F(CharConversionLiveTest, RebindingChangesTheTruncationVerdict) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> fits = {'a', 'b', 'c'};
    SQLLEN fits_ind = static_cast<SQLLEN>(fits.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 3, 0, fits.data(), fits_ind,
                                   &fits_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abc", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> overlong = {'a', 'b', 'c', 'd'};
    SQLLEN overlong_ind = static_cast<SQLLEN>(overlong.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 3, 0, overlong.data(),
                                   overlong_ind, &overlong_ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");

    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 3, 0, fits.data(), fits_ind,
                                   &fits_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abc", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Each parameter carries its own declaration, so two cross-family bindings in
// opposite directions must not borrow each other's type. 6 is three UTF-16
// units, 3 is three single-byte ones.
TEST_F(CharConversionLiveTest, MixedFamilyCharParamsInOneStatement) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16)) + '/'"
                          " + CAST(DATALENGTH(?) AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> narrow = {'a', 'b', 'c'};
    SQLLEN narrow_ind = static_cast<SQLLEN>(narrow.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_WVARCHAR, 8, 0, narrow.data(),
                                   narrow_ind, &narrow_ind),
                  SQL_HANDLE_STMT, stmt_);

    SQLWCHAR wide[] = {'a', 'b', 'c'};
    SQLLEN wide_ind = static_cast<SQLLEN>(sizeof(wide));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 8, 0, wide, wide_ind,
                                   &wide_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("6/3", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A character the target code page cannot hold is replaced with the '?' the
// ODBC spec's substitution wording suggests, and that is all msodbcsql does
// too. It used to be neither: serialize_string left the encode to encoding_rs,
// which implements WHATWG form-submission semantics and emits a decimal numeric
// character reference, so U+65E5 landed in the column as the eight ASCII bytes
// "&#26085;" - markup rather than text, and eight bytes counted against the
// column where one character was measured. The substitution is pinned in
// mssql-tds by unmappable_character_is_substituted_rather_than_escaped.
//
// Nothing is reported by default, matching msodbcsql, which discards its own
// loss flag on the parameter path (Xlat(..., TOSERVER, NULL, ...) at
// sqlcmisc.cpp:7364 and sqlccnvt.cpp:995). An application that wants to know
// asks with SQL_COPT_SS_WARN_ON_CP_ERROR; that leg is
// UnmappableCharacterWarnsWhenAsked below.
//
// Runs on both legs: '?' is what msodbcsql sends as well, and the exact
// SQL_SUCCESS is asserted rather than SQL_SUCCEEDED so "silent by default"
// is actually pinned.
TEST_F(CharConversionLiveTest, UnmappableCharacterIsSubstituted) {
    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    // Asserted server-side with ASCII() and DATALENGTH(): echoing the parameter
    // back would decode symmetrically and hide a mis-encoded payload, and the
    // length is the half of this that regressed - one character was measured
    // against ColumnSize and eight bytes were sent.
    const char* kProbe =
        "SELECT CAST(ASCII(?) AS VARCHAR(16)) + '/' + CAST(DATALENGTH(?) AS VARCHAR(16))";

    ASSERT_SQL_OK(Prepare(kProbe), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR wide[] = {0x65E5};
    SQLLEN ind = static_cast<SQLLEN>(sizeof(wide));
    for (SQLUSMALLINT param = 1; param <= 2; ++param) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                       SQL_VARCHAR, 8, 0, wide, ind, &ind),
                      SQL_HANDLE_STMT, stmt_);
    }

    // Exactly SQL_SUCCESS: the substitution is not reportable unless asked for,
    // so anything that warned unconditionally would still satisfy
    // SQL_SUCCEEDED and must not pass here.
    EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("63/1", GetColumnChar(1))
        << "one '?' (0x3F) of one byte, not eight bytes of '&#26085;'";
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    // Only the unmappable character is replaced; its neighbours survive, and
    // the value does not change length. 'é' is a CP1252 character, so a
    // substitution that over-reached would show up here as 0x3F or as a
    // different DATALENGTH.
    ASSERT_SQL_OK(
        Prepare("SELECT CAST(ASCII(SUBSTRING(?, 2, 1)) AS VARCHAR(16)) + '/'"
                " + CAST(ASCII(SUBSTRING(?, 3, 1)) AS VARCHAR(16)) + '/'"
                " + CAST(DATALENGTH(?) AS VARCHAR(16))"),
        SQL_HANDLE_STMT, stmt_);
    SQLWCHAR mixed[] = {0x00E9, 0x65E5, 0x00E9};  // é 日 é
    SQLLEN mixed_ind = static_cast<SQLLEN>(sizeof(mixed));
    for (SQLUSMALLINT param = 1; param <= 3; ++param) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                       SQL_VARCHAR, 8, 0, mixed, mixed_ind, &mixed_ind),
                      SQL_HANDLE_STMT, stmt_);
    }
    EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("63/233/3", GetColumnChar(1)) << "'?' in the middle, 'e-acute' intact, 3 bytes";
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The narrow C type is the *pre-existing* route to the same encoder -
// SQL_C_CHAR + SQL_VARCHAR was a supported pairing long before the wide leg was
// enabled - and AppValue::NarrowText reaches serialize_string's VARCHAR arm as
// EncodingType::Utf8, whose bytes are not wire bytes, so it is re-encoded under
// the collation exactly as the wide leg is.
//
// Its own test rather than a section of UnmappableCharacterIsSubstituted: a
// mid-test SKIP_IF_COMPARING_MSODBCSQL() reports the whole case skipped, which
// would take the both-legs assertions above out of the parity run with it.
//
// Driver-specific: this driver's SQL_C_CHAR is UTF-8 (parity-deviations entry
// 3), while msodbcsql reads it in the client code page, so the same bound bytes
// are not the same value on the two drivers and no assertion can hold for both.
// Measured with the skip removed: msodbcsql answers 230/3 here, CP1252 reading
// the three UTF-8 bytes as three separate characters.
TEST_F(CharConversionLiveTest, UnmappableCharacterIsSubstitutedForANarrowCType) {
    SKIP_IF_COMPARING_MSODBCSQL();

    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    ASSERT_SQL_OK(
        Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16)) + '/'"
                " + CAST(DATALENGTH(?) AS VARCHAR(16))"),
        SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> narrow = {0xE6, 0x97, 0xA5};  // U+65E5 in UTF-8
    SQLLEN narrow_ind = static_cast<SQLLEN>(narrow.size());
    for (SQLUSMALLINT param = 1; param <= 2; ++param) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_CHAR,
                                       SQL_VARCHAR, 8, 0, narrow.data(), narrow_ind,
                                       &narrow_ind),
                      SQL_HANDLE_STMT, stmt_);
    }

    EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("63/1", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// An astral character is two UTF-16 code units and substitutes as two bytes.
// WideCharToMultiByte counts that way (measured: U+1F600 under CP1252 gives
// 3F 3F) and so does the engine - DATALENGTH(CAST(N'<emoji>' AS varchar(4)))
// is 2. Substituting once per Unicode scalar instead would make the value a
// byte shorter here than through msodbcsql, so a varchar(n) would accept a
// string the reference driver rejects.
//
// Runs on both legs: this is parity, not a deviation.
TEST_F(CharConversionLiveTest, AstralUnmappableCharacterSubstitutesPerUtf16Unit) {
    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    ASSERT_SQL_OK(Prepare("SELECT CAST(DATALENGTH(?) AS VARCHAR(16))"), SQL_HANDLE_STMT, stmt_);
    // U+1F600 as a surrogate pair.
    SQLWCHAR astral[] = {0xD83D, 0xDE00};
    SQLLEN ind = static_cast<SQLLEN>(sizeof(astral));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 8, 0, astral, ind, &ind),
                  SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetColumnChar(1)) << "one substitute byte per UTF-16 unit";
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Pins parity-deviations.md entry 18. msodbcsql converts with
// WideCharToMultiByte(cp, 0, ...), where dwFlags=0 leaves *best-fit* mapping
// on, so a character with no code-page representation but a plausible
// transliteration is rewritten rather than substituted: U+0141 LATIN CAPITAL
// LETTER L WITH STROKE becomes 'L' (76), not '?' (63). SQL Server's own
// CAST(N'...' AS varchar) agrees, because it uses the same Windows NLS tables.
//
// encoding_rs is a strict WHATWG encoder with no best-fit tables, so this
// driver substitutes. Not replicated because msodbcsql has no single answer to
// replicate - its iconv legs take the transliteration from the C library and
// drop it entirely under musl - so matching the Windows column would mean
// shipping per-code-page tables to chase a behaviour the reference driver does
// not hold stable across its own platforms.
//
// Skipped under comparison: this is the disagreement, by design.
TEST_F(CharConversionLiveTest, BestFitMappableCharacterDeviates) {
    SKIP_IF_COMPARING_MSODBCSQL();

    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    // Every one of these has a Windows best-fit mapping and no CP1252
    // representation, so msodbcsql and the engine transliterate where we
    // substitute. Kept as a table so a future best-fit implementation has the
    // expected values to switch to (the parenthesised byte).
    struct Case {
        SQLWCHAR ch;
        const char* name;
        const char* msodbcsql_would_send;  // for the reader, not asserted
    };
    const Case cases[] = {
        {0x0100, "U+0100 A-macron", "65 'A'"},
        {0x0106, "U+0106 C-acute", "67 'C'"},
        {0x0141, "U+0141 L-stroke", "76 'L'"},
        {0x2010, "U+2010 hyphen", "45 '-'"},
    };

    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16))"), SQL_HANDLE_STMT, stmt_);
        SQLWCHAR buf[] = {c.ch};
        SQLLEN ind = static_cast<SQLLEN>(sizeof(buf));
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                       SQL_VARCHAR, 8, 0, buf, ind, &ind),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_)) << c.name;
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("63", GetColumnChar(1))
            << c.name << ": this driver substitutes '?'; msodbcsql best-fit maps it to "
            << c.msodbcsql_would_send;
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    }

    // A character CP1252 *can* hold is untouched, so the deviation is confined
    // to the best-fit band rather than being "any non-ASCII character".
    // U+20AC EURO SIGN is byte 0x80 (128) in CP1252.
    ASSERT_SQL_OK(Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16))"), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR euro[] = {0x20AC};
    SQLLEN euro_ind = static_cast<SQLLEN>(sizeof(euro));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 8, 0, euro, euro_ind, &euro_ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("128", GetColumnChar(1)) << "CP1252 holds the euro sign; nothing to substitute";
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The substitution is a property of the encoder, so it must reach every narrow
// target the serializer routes differently: char pads and is fixed-width, and
// varchar(max) is PLP-framed rather than length-prefixed. A bug confined to one
// arm would otherwise pass on plain varchar alone.
//
// Runs on both legs. SQL_LONGVARCHAR is deliberately absent: this driver sends
// it as varchar(max) while msodbcsql declares `text` (AB#47592), and a `text`
// parameter cannot be used in the ASCII()/DATALENGTH() probe below - measured,
// msodbcsql answers SQL_ERROR then HY010 - so it is covered by its own
// driver-specific case rather than made to look like a parity assertion.
TEST_F(CharConversionLiveTest, UnmappableCharacterIsSubstitutedForEveryNarrowTarget) {
    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    struct Case {
        SQLSMALLINT sql_type;
        SQLULEN column_size;
        const char* name;
        const char* expected;  // ASCII of first char / DATALENGTH
    };
    const Case cases[] = {
        // char(4) pads to its declared width, so one '?' plus three blanks.
        {SQL_CHAR, 4, "SQL_CHAR", "63/4"},
        {SQL_VARCHAR, 8, "SQL_VARCHAR", "63/1"},
        // ColumnSize 0 is the `max` spelling: PLP framing, no declared bound.
        {SQL_VARCHAR, 0, "SQL_VARCHAR(max)", "63/1"},
    };

    for (const Case& c : cases) {
        ASSERT_SQL_OK(
            Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16)) + '/'"
                    " + CAST(DATALENGTH(?) AS VARCHAR(16))"),
            SQL_HANDLE_STMT, stmt_);
        SQLWCHAR wide[] = {0x65E5};
        SQLLEN ind = static_cast<SQLLEN>(sizeof(wide));
        for (SQLUSMALLINT param = 1; param <= 2; ++param) {
            ASSERT_SQL_OK(SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                           c.sql_type, c.column_size, 0, wide, ind, &ind),
                          SQL_HANDLE_STMT, stmt_);
        }
        EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_)) << c.name;
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.expected, GetColumnChar(1)) << c.name;
        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);
    }
}

// SQL_LONGVARCHAR reaches the same encoder, but msodbcsql cannot run this
// scenario at all rather than running it differently: it declares the parameter
// `text` (AB#47592), and `text` is not a legal sp_executesql parameter type, so
// the probe fails before any conversion happens. Measured with the skip
// removed: SQL_ERROR from SQLExecute, then HY010 from the fetch. Skipped
// because there is no msodbcsql behaviour to compare, not because we expect to
// differ - revisit if AB#47592 changes our declaration.
TEST_F(CharConversionLiveTest, UnmappableCharacterIsSubstitutedForLongVarchar) {
    SKIP_IF_COMPARING_MSODBCSQL();

    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    ASSERT_SQL_OK(
        Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16)) + '/'"
                " + CAST(DATALENGTH(?) AS VARCHAR(16))"),
        SQL_HANDLE_STMT, stmt_);
    SQLWCHAR wide[] = {0x65E5};
    SQLLEN ind = static_cast<SQLLEN>(sizeof(wide));
    for (SQLUSMALLINT param = 1; param <= 2; ++param) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                       SQL_LONGVARCHAR, 8, 0, wide, ind, &ind),
                      SQL_HANDLE_STMT, stmt_);
    }
    EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("63/1", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A narrow value wrapped in a sql_variant takes a different serializer path -
// resolve_narrow_wire_bytes, which resolves the wire bytes once so the declared
// VARIANT_PROPERTIES length and the payload cannot disagree (AB#47800). It
// calls the same encoder, so it must substitute on the same terms, and the
// declared length must follow the *substituted* bytes rather than the source.
//
// Driver-specific for the same reason as UnmappableCharacterIsSubstitutedForA
// NarrowCType: reaching the *narrow* inner type needs SQL_C_CHAR, which is
// UTF-8 here and the client code page on msodbcsql. Measured with the skip
// removed: msodbcsql agrees on the inner type and the surviving 'c'
// (varchar/99/...) but reports 6 bytes to our 4, because it sent the six raw
// bytes as six CP1252 characters rather than transcoding "caf" + U+65E5 to
// "caf?". That is parity-deviations entry 3, not a substitution difference.
TEST_F(CharConversionLiveTest, UnmappableCharacterIsSubstitutedInsideASqlVariant) {
    SKIP_IF_COMPARING_MSODBCSQL();

    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    // The inner type must still be varchar, not nvarchar: a substitution that
    // re-tagged the value would hide the loss behind a widened declaration.
    ASSERT_SQL_OK(Prepare("SELECT CAST(SQL_VARIANT_PROPERTY(?, 'BaseType') AS VARCHAR(32))"
                          " + '/' + CAST(ASCII(CONVERT(VARCHAR(8), ?)) AS VARCHAR(16))"
                          " + '/' + CAST(DATALENGTH(?) AS VARCHAR(16))"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> narrow = {'c', 'a', 'f', 0xE6, 0x97, 0xA5};  // "caf" + U+65E5
    SQLLEN ind = static_cast<SQLLEN>(narrow.size());
    for (SQLUSMALLINT param = 1; param <= 3; ++param) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_CHAR,
                                       SQL_SS_VARIANT, 8, 0, narrow.data(), ind, &ind),
                      SQL_HANDLE_STMT, stmt_);
    }

    EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    // "caf?" - four bytes, so the declared length followed the substitution
    // rather than the six source bytes.
    EXPECT_EQ("varchar/99/4", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQL_COPT_SS_WARN_ON_CP_ERROR (1243) turns the substitution into a reportable
// event: SQLSTATE 01000 and SQL_SUCCESS_WITH_INFO. msodbcsql owns the attribute
// but only consults it on the fetch direction; this driver applies it to
// parameters, which is where its own loss occurs - SQL_C_CHAR is UTF-8 here, so
// a fetch can always represent whatever the server sent (AB#47598).
//
// Skipped under comparison for exactly that reason: msodbcsql returns plain
// SQL_SUCCESS for a parameter however the attribute is set.
TEST_F(CharConversionLiveTest, UnmappableCharacterWarnsWhenAsked) {
    SKIP_IF_COMPARING_MSODBCSQL();

    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_COPT_SS_WARN_ON_CP_ERROR,
                                    reinterpret_cast<SQLPOINTER>(SQL_WARN_YES), 0),
                  SQL_HANDLE_DBC, dbc_);

    ASSERT_SQL_OK(Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16))"), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR wide[] = {0x65E5};
    SQLLEN ind = static_cast<SQLLEN>(sizeof(wide));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 8, 0, wide, ind, &ind),
                  SQL_HANDLE_STMT, stmt_);

    // Still a success: the value was sent, so the application keeps its rows.
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01000");
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("63", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    // A value the code page can hold must not warn, or the attribute would make
    // every statement return SQL_SUCCESS_WITH_INFO.
    ASSERT_SQL_OK(Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16))"), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR mappable[] = {0x00E9};
    SQLLEN mappable_ind = static_cast<SQLLEN>(sizeof(mappable));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 8, 0, mappable, mappable_ind,
                                   &mappable_ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SUCCESS, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("233", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_COPT_SS_WARN_ON_CP_ERROR,
                                    reinterpret_cast<SQLPOINTER>(SQL_WARN_NO), 0),
                  SQL_HANDLE_DBC, dbc_);
}

// The streamed route converts each chunk on the way out and writes it straight
// to the wire, so it applies the code page in DaeTranscode rather than in
// serialize_string. It must substitute on the same terms.
//
// Runs on both legs: bound wide so the source bytes mean the same thing to
// both drivers, and with no attribute set so only the substitution is asserted.
// The warning half is a deviation and lives in
// DataAtExecutionUnmappableCharacterWarnsWhenAsked.
TEST_F(CharConversionLiveTest, DataAtExecutionUnmappableCharacterIsSubstituted) {
    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    ASSERT_SQL_OK(
        Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16)) + '/'"
                " + CAST(DATALENGTH(?) AS VARCHAR(16))"),
        SQL_HANDLE_STMT, stmt_);
    SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    for (SQLUSMALLINT param = 1; param <= 2; ++param) {
        ASSERT_SQL_OK(SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                       SQL_VARCHAR, 0, 0, &token, 0, &streamed_ind),
                      SQL_HANDLE_STMT, stmt_);
    }

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLWCHAR chunk[] = {0x65E5};
    SQLPOINTER value_ptr = nullptr;
    // Two data-at-execution parameters, each supplied in turn.
    for (int i = 0; i < 2; ++i) {
        ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));
        ASSERT_SQL_OK(SQLPutData(stmt_, chunk, sizeof(chunk)), SQL_HANDLE_STMT, stmt_);
    }
    EXPECT_EQ(SQL_SUCCESS, SQLParamData(stmt_, &value_ptr));

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("63/1", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The streamed loss must reach the *statement* rather than the SQLPutData that
// produced it: a diagnostic posted there would be cleared by the next ODBC call
// on the handle, so the application would never read it. SQLPutData therefore
// succeeds plainly and SQLParamData carries the warning (AB#47598).
//
// Skipped under comparison for the same reason as
// UnmappableCharacterWarnsWhenAsked: msodbcsql does not consult the attribute
// on the parameter path at all.
TEST_F(CharConversionLiveTest, DataAtExecutionUnmappableCharacterWarnsWhenAsked) {
    SKIP_IF_COMPARING_MSODBCSQL();

    if (!DatabaseIsLatin1()) {
        GTEST_SKIP() << "needs a Latin1 database collation";
    }

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_COPT_SS_WARN_ON_CP_ERROR,
                                    reinterpret_cast<SQLPOINTER>(SQL_WARN_YES), 0),
                  SQL_HANDLE_DBC, dbc_);

    ASSERT_SQL_OK(Prepare("SELECT CAST(ASCII(?) AS VARCHAR(16))"), SQL_HANDLE_STMT, stmt_);
    SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 0, 0, &token, 0, &streamed_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    SQLWCHAR chunk[] = {0x65E5};
    // The chunk itself succeeds plainly: the warning belongs to the statement.
    EXPECT_EQ(SQL_SUCCESS, SQLPutData(stmt_, chunk, sizeof(chunk)));
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SQLParamData(stmt_, &value_ptr));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01000");

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("63", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_COPT_SS_WARN_ON_CP_ERROR,
                                    reinterpret_cast<SQLPOINTER>(SQL_WARN_NO), 0),
                  SQL_HANDLE_DBC, dbc_);
}

// ---------------------------------------------------------------------------
// A value rejected after a packet has flushed leaves the server holding an
// incomplete message. Unless it is withdrawn (EOM | IGNORE, then its DONE
// consumed) the next command is read as a continuation and answered 4002 - on a
// *later* statement than the one that failed. AB#47687.
//
// The drivers are stranded by different events: msodbcsql converts each
// parameter as the RPC is written, this driver converts all of them first. So a
// conversion error strands msodbcsql, and only a serialization-time rejection
// strands us. One test each below; for the mechanism exercised on both at once,
// see SQLCancelAfterAFlushRetractsTheRequest in execute_test.cpp.
// ---------------------------------------------------------------------------

// Strands msodbcsql: parameter 2 fails `ParamToSQLType` with parameter 1 already
// on the wire. This driver rejects it before serializing, so it takes the
// local-discard branch. Both must end with a usable connection.
TEST_F(CharConversionLiveTest, ConversionFailureAfterAFlushLeavesTheConnectionUsable) {
    const std::string spid = CurrentSpid();
    ASSERT_SQL_OK(Prepare("SELECT ?, ?"), SQL_HANDLE_STMT, stmt_);

    // ColumnSize 0 is the `max` spelling, so no length bound applies to this one.
    std::vector<SQLCHAR> big(20000, 'a');
    SQLLEN big_ind = static_cast<SQLLEN>(big.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR, 0, 0,
                                   big.data(), big_ind, &big_ind),
                  SQL_HANDLE_STMT, stmt_);

    // Ten characters into varchar(4), with a non-blank overflow, so neither
    // driver may trim it silently.
    AsciiParam overlong(SQL_C_CHAR, "abcdefghij");
    ASSERT_SQL_OK(BindAscii(2, overlong, SQL_VARCHAR, 4), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));

    ExpectSameSessionStillUsable(spid);
}

// Strands this driver: the value passes our UTF-16 unit count at one unit, then
// `serialize_string` rejects it against varchar(1) once the collation's code
// page turns it into two bytes - after parameter 1 has flushed. msodbcsql
// measures the converted bytes first and rejects it before sending anything, so
// it never reaches the partial-send state here and the case is skipped on that
// leg.
//
// Needs a DBCS collation. This used to run on Latin1, where U+65E5 was
// unmappable and `encoding_rs` expanded it to the eight bytes of "&#26085;";
// that expansion was itself the bug, and an unmappable character is now one '?'
// byte (AB#47598), so nothing a Latin1 code page produces can outgrow the unit
// count that let it through. A multi-byte *mappable* character is what is left.
// The retraction mechanism itself is covered without a server by mssql-tds'
// `a_send_site_retracts_when_serialization_fails_after_a_flush`, which binds
// past the declared length directly and so does not need this gap.
TEST_F(CharConversionLiveTest, SerializationFailureAfterAFlushLeavesTheConnectionUsable) {
    SKIP_IF_COMPARING_MSODBCSQL();
    const std::string collation = DatabaseCollation();
    // Japanese / Chinese / Korean collations select a DBCS code page, where a
    // single BMP character is two bytes.
    const bool dbcs = collation.find("Japanese") != std::string::npos ||
                      collation.find("Chinese") != std::string::npos ||
                      collation.find("Korean") != std::string::npos;
    if (!dbcs) {
        GTEST_SKIP() << "needs a DBCS collation for one unit to outgrow one byte, server has "
                     << collation;
    }

    const std::string spid = CurrentSpid();
    ASSERT_SQL_OK(Prepare("SELECT ?, ?"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> big(20000, 'a');
    SQLLEN big_ind = static_cast<SQLLEN>(big.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR, 0, 0,
                                   big.data(), big_ind, &big_ind),
                  SQL_HANDLE_STMT, stmt_);

    // U+3042 HIRAGANA A: one UTF-16 unit, two bytes in every DBCS code page.
    SQLWCHAR wide[] = {0x3042};
    SQLLEN ind = static_cast<SQLLEN>(sizeof(wide));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_VARCHAR, 1, 0, wide,
                                   ind, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_ERROR, SQLExecute(stmt_))
        << "the over-long value must be rejected during serialization";

    ExpectSameSessionStillUsable(spid);
}

// ColumnSize bounds a streamed character value exactly as it bounds a
// materialized one, and the bound is against the accumulated total rather than
// each chunk (AB#47590). The parameter is declared varchar(2) to match: the
// value body is still PLP framing opened before the length is known, but the
// variable it is assigned to carries the declared length.
//
// Runs on both legs: msodbcsql applies the same cchMaxPrec bound and
// CheckTrailingChars rule to each call, accumulating cbDataSentToServer across
// them (sqlccmd.cpp:11085-11108).
TEST_F(CharConversionLiveTest, DataAtExecutionOverflowingColumnSizeIsRejected) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR, 2, 0,
                                   &token, 0, &streamed_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    SQLCHAR chunk[] = {'a', 'b', 'c', 'd'};
    EXPECT_EQ(SQL_ERROR, SQLPutData(stmt_, chunk, sizeof(chunk)));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
}

// A wideness-mismatched pairing is bounded on the same terms as a matched one.
// ColumnSize names the declaration's unit and the count is of the source, so
// nvarchar(2) bounds two UTF-16 units of the bound buffer whichever width that
// buffer has. Both directions are covered: without the bound the client would
// declare a length and then stream past it, since a streamed value never
// reaches the close-time conversion that bounds a materialized one.
TEST_F(CharConversionLiveTest, DataAtExecutionBoundsAWidenessMismatch) {
    // Narrow buffer against a wide declaration.
    {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
        SQLCHAR token = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_WVARCHAR, 2, 0,
                                       &token, 0, &streamed_ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
        SQLPOINTER value_ptr = nullptr;
        ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

        SQLCHAR chunk[] = {'a', 'b', 'c', 'd'};
        EXPECT_EQ(SQL_ERROR, SQLPutData(stmt_, chunk, sizeof(chunk)));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    }

    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);

    // Wide buffer against a narrow declaration.
    {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
        SQLCHAR token = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_VARCHAR, 2, 0,
                                       &token, 0, &streamed_ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
        SQLPOINTER value_ptr = nullptr;
        ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

        // Four UTF-16 units against nvarchar-equivalent room for two.
        SQLWCHAR wide[] = {'a', 'b', 'c', 'd'};
        EXPECT_EQ(SQL_ERROR, SQLPutData(stmt_, wide, sizeof(wide)));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    }
}

// The other half of the rule for the character family: an overflow of blanks is
// dropped rather than reported, so the value lands trimmed to ColumnSize. The
// pad byte differs from the binary path -- a blank, not a zero.
TEST_F(CharConversionLiveTest, DataAtExecutionTrimsBlankOverflowToColumnSize) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR, 3, 0,
                                   &token, 0, &streamed_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    SQLCHAR chunk[] = {'a', 'b', 'c', ' ', ' '};
    ASSERT_SQL_OK(SQLPutData(stmt_, chunk, sizeof(chunk)), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLParamData(stmt_, &value_ptr), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abc", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A wide streamed parameter is measured in characters, so its byte budget is
// twice ColumnSize. Two chunks of one character each fit nvarchar(2); a third
// does not, and the overflow is not a blank.
TEST_F(CharConversionLiveTest, DataAtExecutionWideColumnSizeCountsCharacters) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR, SQL_WVARCHAR, 2, 0,
                                   &token, 0, &streamed_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    SQLWCHAR first[] = {'a'};
    ASSERT_SQL_OK(SQLPutData(stmt_, first, sizeof(first)), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR second[] = {'b'};
    ASSERT_SQL_OK(SQLPutData(stmt_, second, sizeof(second)), SQL_HANDLE_STMT, stmt_);
    // Two characters already sent against nvarchar(2): a third overflows.
    SQLWCHAR third[] = {'c'};
    EXPECT_EQ(SQL_ERROR, SQLPutData(stmt_, third, sizeof(third)));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
}

// A max declaration has no length to enforce, so a streamed value of any size
// still goes out whole. ColumnSize 0 is how SQLDescribeParam reports one.
TEST_F(CharConversionLiveTest, DataAtExecutionMaxDeclarationIsUnbounded) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR, 0, 0,
                                   &token, 0, &streamed_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    SQLCHAR chunk[] = {'a', 'b', 'c', 'd'};
    ASSERT_SQL_OK(SQLPutData(stmt_, chunk, sizeof(chunk)), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLParamData(stmt_, &value_ptr), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abcd", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// `char(n)` is not one of the PLP-framable types, so its value cannot be
// streamed as chunks: it is collected across the SQLPutData calls and converted
// whole when SQLParamData closes the parameter, which is the branch msodbcsql
// serves with WriteToExtBuffer (sqlccmd.cpp:4913). The declaration and the
// blank padding are what prove it went out as `char(8)` rather than a `max`.
TEST_F(CharConversionLiveTest, DataAtExecutionFixedWidthIsCollectedAndDeclaredChar) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(SQL_VARIANT_PROPERTY(CAST(? AS SQL_VARIANT),"
                          " 'BaseType') AS VARCHAR(32)) + '/' + CAST(LEN(?) AS VARCHAR(8))"),
                  SQL_HANDLE_STMT, stmt_);

    SQLLEN streamed_ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    std::vector<SQLCHAR> echo = {'a', 'b', 'c'};
    SQLLEN echo_ind = static_cast<SQLLEN>(echo.size());
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_CHAR, 8, 0,
                                   &token, 0, &streamed_ind),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_CHAR, 8,
                                   0, echo.data(), echo_ind, &echo_ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    // Two chunks: the value only exists once both have been collected.
    SQLCHAR first[] = {'a'};
    SQLCHAR second[] = {'b', 'c'};
    ASSERT_SQL_OK(SQLPutData(stmt_, first, sizeof(first)), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLPutData(stmt_, second, sizeof(second)), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLParamData(stmt_, &value_ptr), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    // `char(8)` blank-pads, so LEN() of the materialized sibling is 3 while the
    // streamed one is declared the same fixed-width type.
    EXPECT_EQ("char/3", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

TEST_F(CharConversionLiveTest, DataAtExecutionFixedWidthOverflowIs22001OnSecondChunk) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLLEN ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_CHAR, 4, 0, &token, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    SQLCHAR first[] = {'a', 'b'};
    SQLCHAR second[] = {'c', 'd', 'e'};
    ASSERT_SQL_OK(SQLPutData(stmt_, first, sizeof(first)), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(SQL_ERROR, SQLPutData(stmt_, second, sizeof(second)));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
}

TEST_F(CharConversionLiveTest, DataAtExecutionBoundedFixedWidthReassemblesSplitUtf8) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLLEN ind = SQL_DATA_AT_EXEC;
    SQLCHAR token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_WCHAR, 4, 0, &token, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value_ptr = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value_ptr));

    SQLCHAR first[] = {'c', 'a', 'f', 0xC3};
    SQLCHAR second[] = {0xA9};
    ASSERT_SQL_OK(SQLPutData(stmt_, first, sizeof(first)), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLPutData(stmt_, second, sizeof(second)), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLParamData(stmt_, &value_ptr), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR value[8] = {0};
    SQLLEN value_len = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_WCHAR, value, sizeof(value),
                             &value_len),
                  SQL_HANDLE_STMT, stmt_);
    const SQLWCHAR expected[] = {'c', 'a', 'f', 0x00E9};
    ASSERT_EQ(sizeof(expected), static_cast<size_t>(value_len));
    for (size_t i = 0; i < sizeof(expected) / sizeof(SQLWCHAR); ++i) {
        EXPECT_EQ(expected[i], value[i]) << "code unit " << i;
    }
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}
