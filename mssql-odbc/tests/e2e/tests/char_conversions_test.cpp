// Copyright (c) Microsoft Corporation. All rights reserved.
// char_conversions_test.cpp  -  E2E tests for character parameter conversion:
// SQL_C_CHAR / SQL_C_WCHAR bound against char, varchar, text and their wide
// counterparts. Covers the declared wire type, ColumnSize semantics, truncation
// and its blank exemption, encoding, and the indicator/terminator rules.
//
// Statement lifecycle (prepare, re-execute, cached handles, data-at-execution)
// lives in execute_test.cpp.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <cassert>
#include <string>
#include <vector>

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
// escapes the check. Both exits from that arm then break past the shared trim at
// :2955, and in a retail build the over-long value is not rejected but silently
// widened on the wire by stMaxLen = max(*pstMaxLen, stLen) (sqlcmisc.cpp:7458).
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

// Blank overflow on the narrow -> wide path is trimmed here and not there. The
// walk at sqlcfunc.cpp:2926 never fires on a blank, and both of its exits skip
// the trim at :2955, so msodbcsql sends all six characters as nvarchar(6) after
// widening at sqlcmisc.cpp:7458. Same rule as
// OverflowingBlanksAreTrimmedSilently, which msodbcsql does honour because that
// binding is narrow -> narrow and reaches :2955.
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
// msodbcsql has no such gap here - it ships SQL_C_CHAR bytes verbatim under the
// client collation (sqlcmisc.cpp:7328), so the count it checks is the count that
// lands. Compare is skipped because that makes its result depend on the client
// code page: a non-UTF-8 client reads these bytes as two CP1252 characters and
// LEN returns 5.
//
// The inverse is the dangerous direction and needs a DBCS-collated database to
// pin: GB18030 emits four bytes where UTF-8 uses two, so an over-long value
// reaches serialize_char_varchar_direct and fails there with an opaque driver
// error rather than 22001.
//
// This asserts a known-wrong result, deferred rather than accepted (AB#47584).
TEST_F(CharConversionLiveTest, NarrowMultibyteIsMeasuredInUtf8Bytes) {
    SKIP_IF_COMPARING_MSODBCSQL();

    std::vector<SQLCHAR> value = {'c', 'a', 'f', 0xC3, 0xA9};
    SQLLEN ind = static_cast<SQLLEN>(value.size());

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 4, 0, value.data(), ind, &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22001");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(Prepare("SELECT LEN(?) AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, 5, 0, value.data(), ind, &ind),
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

// A character the target code page cannot hold is corrupted rather than
// rejected, and not with the '?' the ODBC spec's substitution wording suggests:
// serialize_string calls encoding_rs' encode, which emits a decimal numeric
// character reference, so U+65E5 lands in the column as the eight ASCII bytes
// "&#26085;". Only a tracing::warn! records it, so nothing reaches the
// application - no 22001, no 01004, no SQL_SUCCESS_WITH_INFO. The exact
// substitution is pinned in mssql-tds by
// unmappable_character_becomes_a_numeric_character_reference.
//
// This also makes the length check wrong in the dangerous direction: one UTF-16
// unit was measured against ColumnSize and eight bytes were sent, the same hole
// as the GB18030 case in NarrowMultibyteIsMeasuredInUtf8Bytes but reachable on a
// plain Latin1 database. At ColumnSize 1 it never reaches the server -
// serialize_char_varchar_direct rejects it with an opaque driver error rather
// than 22001.
//
// Skipped under comparison: msodbcsql converts with SystemLocale::FromUtf16
// (WideCharToMultiByte), which substitutes a single '?'.
//
// Known-wrong, deferred (AB#47598).
TEST_F(CharConversionLiveTest, UnmappableCharacterIsSilentlyCorrupted) {
    SKIP_IF_COMPARING_MSODBCSQL();

    // serialize_string picks the code page from the collation's LCID alone - it
    // ignores the UTF-8 flag and the sort ID - so only the LCID has to be Latin1
    // for U+65E5 to be unmappable. The parameter carries the *database*
    // collation, which need not match the instance's.
    ASSERT_SQL_OK(
        Prepare("SELECT CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation')"
                " AS VARCHAR(128))"),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    const std::string collation = GetColumnChar(1);
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    if (collation.find("Latin1_General") == std::string::npos) {
        GTEST_SKIP() << "needs a Latin1 collation, server has " << collation;
    }

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    SQLWCHAR wide[] = {0x65E5};
    SQLLEN ind = static_cast<SQLLEN>(sizeof(wide));
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                   SQL_VARCHAR, 8, 0, wide, ind, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("&#26085;", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}
