// Copyright (c) Microsoft Corporation. All rights reserved.
// execute_test.cpp  –  E2E tests for SQLPrepare + SQLBindParameter + SQLExecute.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <cstring>
#include <vector>

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

TEST(ExecuteTest, ExecuteNullHandle) {
    SQLRETURN rc = SQLExecute(SQL_NULL_HSTMT);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

TEST(ExecuteTest, BindParameterNullHandle) {
    SQLCHAR value[] = "x";
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(SQL_NULL_HSTMT, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                    SQL_VARCHAR, 1, 0, value, sizeof(value), &ind);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class PrepareExecuteLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    // Prepare helper.
    SQLRETURN Prepare(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // Bind a narrow (SQL_C_CHAR / varchar) input parameter held in |store|.
    // |store| must outlive the SQLExecute call (bound by reference).
    SQLRETURN BindChar(SQLUSMALLINT param, std::vector<SQLCHAR>& store,
                       SQLLEN& ind) {
        return SQLBindParameter(stmt_, param, SQL_PARAM_INPUT, SQL_C_CHAR,
                                SQL_VARCHAR, store.size(), 0, store.data(),
                                static_cast<SQLLEN>(store.size()), &ind);
    }

    // Read column 1 of the current row as a narrow string.
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

// SQLExecute on a statement that was never prepared is a sequence error.
TEST_F(PrepareExecuteLiveTest, ExecuteWithoutPrepareReturnsHy010) {
    SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

// Prepare + execute with no parameters.
TEST_F(PrepareExecuteLiveTest, PrepareExecuteNoParams) {
    ASSERT_SQL_OK(Prepare("SELECT 1"), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLExecute(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", GetColumnChar(1));

    rc = SQLFetch(stmt_);
    EXPECT_EQ(SQL_NO_DATA, rc);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

// A single character parameter flows through sp_prepare/sp_execute and is
// returned verbatim.
TEST_F(PrepareExecuteLiveTest, SingleCharParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value = {'h', 'e', 'l', 'l', 'o', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("hello", GetColumnChar(1));

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A wide-character parameter binds as nvarchar and round-trips.
TEST_F(PrepareExecuteLiveTest, WideCharParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    // UTF-16 "wide" + NUL terminator.
    SQLWCHAR value[] = {'w', 'i', 'd', 'e', 0};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                    SQL_WVARCHAR, 4, 0, value, sizeof(value), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("wide", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A wide-char parameter bound with an explicit byte-length indicator (not
// SQL_NTS) sends exactly that many bytes. The indicator is a byte count per the
// ODBC spec, so 8 bytes == 4 UTF-16 units.
TEST_F(PrepareExecuteLiveTest, ExplicitLengthWideCharParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    // "wider" (no NUL) with an indicated length of 8 bytes → only "wide".
    SQLWCHAR value[] = {'w', 'i', 'd', 'e', 'r', 0};
    SQLLEN ind = 4 * sizeof(SQLWCHAR);  // 8 bytes = 4 code units
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_WCHAR,
                                    SQL_WVARCHAR, 5, 0, value, sizeof(value), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("wide", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQL_C_DEFAULT reaches the driver unresolved (the Driver Manager does not
// substitute it), and the driver resolves it from ParameterType. SQL_VARCHAR
// yields SQL_C_CHAR on both drivers.
//
// Benefits-from-mock-tds: a mock TDS server could assert the RPC parameter was
// declared varchar with a single-byte payload; the round-tripped text alone
// cannot show which C type the driver resolved.
TEST_F(PrepareExecuteLiveTest, DefaultCTypeNarrowCharParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value = {'p', 'l', 'a', 'i', 'n', '\0'};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT,
                                    SQL_VARCHAR, value.size(), 0, value.data(),
                                    static_cast<SQLLEN>(value.size()), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("plain", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Accepted parity deviation (AB#47365): SQL_C_DEFAULT resolves the wide
// character SQL types to SQL_C_WCHAR, per the ODBC 3.x default-C-type table.
// msodbcsql's rgbTRANSTYPE380 resolves them to SQL_C_CHAR and would read this
// UTF-16 buffer as narrow text, so the round trip differs there.
//
// Benefits-from-mock-tds: a mock TDS server could assert the RPC parameter was
// declared nvarchar carrying the full UTF-16 payload, which is the deviation
// itself; the round-tripped ASCII text alone cannot show it.
TEST_F(PrepareExecuteLiveTest, DefaultCTypeWideCharParam) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLWCHAR value[] = {'w', 'i', 'd', 'e', 0};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT,
                                    SQL_WVARCHAR, 4, 0, value, sizeof(value), &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("wide", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// An integer parameter bound with SQL_C_DEFAULT survives the round trip: the
// driver picks a C type for it, sends the value, and the server returns it
// unchanged. Which C type it picks is unit-tested, not observable here.
//
// Benefits-from-mock-tds: a mock TDS server could assert @P1 is declared an
// integer rather than nvarchar(max); SQL Server implicit-converts either way.
TEST_F(PrepareExecuteLiveTest, DefaultCTypeIntegerParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 42;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT,
                                   SQL_INTEGER, 0, 0, &value, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("42", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// One SQL_C_SLONG buffer binds against every integer ParameterType, and each
// one executes and returns the value intact. Which type reaches the wire is
// unit-tested, not observable here.
//
// Benefits-from-mock-tds: a mock TDS server could assert the declared type
// actually changes across the four binds; the round-tripped text is "7" for all
// of them, and for the pre-P3 nvarchar(max) behaviour too.
TEST_F(PrepareExecuteLiveTest, IntegerParamAcceptsEveryIntegerTargetType) {
    const SQLSMALLINT sql_types[] = {SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT};
    for (SQLSMALLINT sql_type : sql_types) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        SQLINTEGER value = 7;
        SQLLEN ind = 0;
        SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                        sql_type, 0, 0, &value, 0, &ind);
        ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("7", GetColumnChar(1)) << "ParameterType " << sql_type;

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

// A value the target type cannot hold is a conversion error at execute, not a
// silently wrapped wire value: 300 does not fit tinyint.
TEST_F(PrepareExecuteLiveTest, IntegerParamOutOfRangeForTargetIs22003) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLINTEGER value = 300;
    SQLLEN ind = 0;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                    SQL_TINYINT, 0, 0, &value, 0, &ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
}

// Integer -> character is quadrant C, still unimplemented, so the bind is
// rejected up front with HYC00 rather than failing at execute. msodbcsql
// supports the pairing, hence the skip; it goes away with P5 (AB#47500).
TEST_F(PrepareExecuteLiveTest, IntegerToCharacterConversionIsRejected) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SQLINTEGER value = 42;
    SQLLEN ind = 0;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                    SQL_VARCHAR, 0, 0, &value, 0, &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// SQL Server has no interval type, so a real-but-unsupported ParameterType is
// HYC00 ("optional feature not implemented") rather than a conversion failure.
// msodbcsql's IsValidSqlType returns IDS_S1_C00 for the whole interval range
// before IsValidSQLConversion is reached, so both drivers agree here.
TEST_F(PrepareExecuteLiveTest, IntervalSqlTypeIsRejectedWithHyc00) {
    SQL_INTERVAL_STRUCT value = {};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT,
                                    SQL_INTERVAL_YEAR, 0, 0, &value,
                                    sizeof(value), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// A NULL takes its type from ParameterType, so an explicitly bound integer C
// type must produce a typed NULL rather than being rejected. The value path and
// the NULL path of the same binding have to agree.
//
// Benefits-from-mock-tds: a mock TDS server could assert the typed NULL carries
// the ParameterType's TYPE_INFO; a fetched NULL looks the same whatever it was
// declared as.
TEST_F(PrepareExecuteLiveTest, ExplicitIntegerParamAcceptsNull) {
    const SQLSMALLINT sql_types[] = {SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT};
    for (SQLSMALLINT sql_type : sql_types) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        SQLINTEGER value = 0;
        SQLLEN ind = SQL_NULL_DATA;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                       sql_type, 0, 0, &value, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLLEN ind_out = 0;
        GetColumnChar(1, &ind_out);
        EXPECT_EQ(SQL_NULL_DATA, ind_out) << "ParameterType " << sql_type;

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

// The same NULL against a defaulted binding. An application must get the same
// answer whether it named the C type or let the driver pick it, so this and
// ExplicitIntegerParamAcceptsNull have to stay in step.
//
// Benefits-from-mock-tds: as above - only a mock server can show the two
// spellings produce the same declaration.
TEST_F(PrepareExecuteLiveTest, DefaultCTypeIntegerParamAcceptsNull) {
    const SQLSMALLINT sql_types[] = {SQL_TINYINT, SQL_SMALLINT, SQL_INTEGER, SQL_BIGINT};
    for (SQLSMALLINT sql_type : sql_types) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        SQLINTEGER value = 0;
        SQLLEN ind = SQL_NULL_DATA;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT,
                                       sql_type, 0, 0, &value, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLLEN ind_out = 0;
        GetColumnChar(1, &ind_out);
        EXPECT_EQ(SQL_NULL_DATA, ind_out) << "ParameterType " << sql_type;

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

// ODBC never promises an aligned application buffer, so the driver reads
// parameter values unaligned. Binding from an odd offset is the only test that
// would fault if that ever regressed to an aligned load.
TEST_F(PrepareExecuteLiveTest, UnalignedIntegerParamBufferIsRead) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    alignas(8) unsigned char backing[16] = {0};
    unsigned char* misaligned = backing + 1;
    const SQLINTEGER value = 123456789;
    std::memcpy(misaligned, &value, sizeof(value));

    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                   SQL_INTEGER, 0, 0, misaligned, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("123456789", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQL_C_TINYINT is sign-unknown, and reads signed against any SQL type other
// than tinyint - here SQL_SMALLINT - so 0xFF is -1. Only SQL_C_UTINYINT reads it
// as 255. The tinyint-to-tinyint exemption is pinned separately by
// TinyintCTypeIsUnsignedAgainstTinyintParameter; this test is the case that does
// *not* get it.
//
// Benefits-from-mock-tds: a mock TDS server could assert the smallint payload
// on the wire, rather than inferring it from the rendered text.
TEST_F(PrepareExecuteLiveTest, TinyintCTypeIsSignedButUtinyintIsNot) {
    struct Case {
        SQLSMALLINT c_type;
        const char* expected;
    };
    const Case cases[] = {{SQL_C_TINYINT, "-1"},
                          {SQL_C_STINYINT, "-1"},
                          {SQL_C_UTINYINT, "255"}};
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        unsigned char raw = 0xFF;
        SQLLEN ind = 0;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, c.c_type,
                                       SQL_SMALLINT, 0, 0, &raw, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(c.expected, GetColumnChar(1)) << "C type " << c.c_type;

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

// msodbcsql treats SQL_C_TINYINT as sign-unknown against a same-width tinyint:
// ParamToSQLType rewrites the C type to unsigned when ParameterType is
// SQL_TINYINT, so a raw 0xFF is 255 rather than -1. Any wider SQL type keeps the
// signed reading, which TinyintCTypeIsSignedButUtinyintIsNot pins against
// SQL_SMALLINT.
//
// Benefits-from-mock-tds: a mock TDS server could assert the declared tinyint
// payload on the wire, rather than inferring it from the rendered text.
TEST_F(PrepareExecuteLiveTest, TinyintCTypeIsUnsignedAgainstTinyintParameter) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    unsigned char raw = 0xFF;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_TINYINT,
                                   SQL_TINYINT, 0, 0, &raw, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("255", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// The fetch half of the same rule: a tinyint column above 127 is bit-copied into
// SQL_C_TINYINT rather than range-checked as signed. msodbcsql never even
// reaches its converter here - sqlcdata.cpp maps the tinyint column to
// SQL_C_UTINYINT and clears fConvNeeded - so the byte arrives intact.
TEST_F(PrepareExecuteLiveTest, TinyintColumnAbove127FetchesIntoTinyintCType) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(200 AS tinyint) AS v"), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    signed char out = 0;
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_TINYINT, &out, sizeof(out), &ind),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(200, static_cast<unsigned char>(out));
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(out)), ind);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// msodbcsql validates ColumnSize in SQLBindParameter (CheckSqlPrecScale, run
// after the type and conversion checks). Its CheckSqlPrec rejects 0 because
// SQL_PREC_UNLIMITED is 0, so a fixed-length declaration cannot use it - but the
// variable-length types read 0 as the `max` spelling and accept it. The error is
// HY104 at bind, not at execute, and it applies whether or not the value is NULL.
TEST_F(PrepareExecuteLiveTest, ZeroColumnSizeIsRejectedForFixedLengthCharTypes) {
    struct Case {
        SQLSMALLINT c_type;
        SQLSMALLINT sql_type;
        bool ok;
    };
    const Case cases[] = {
        {SQL_C_CHAR, SQL_CHAR, false},      {SQL_C_WCHAR, SQL_WCHAR, false},
        {SQL_C_CHAR, SQL_VARCHAR, true},    {SQL_C_WCHAR, SQL_WVARCHAR, true},
        {SQL_C_CHAR, SQL_LONGVARCHAR, false},
    };
    for (const Case& c : cases) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        SQLCHAR buf[8] = {0};
        SQLLEN ind = SQL_NULL_DATA;
        SQLRETURN rc =
            SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, c.c_type, c.sql_type,
                             /*ColumnSize*/ 0, 0, buf, sizeof(buf), &ind);
        if (c.ok) {
            EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        } else {
            EXPECT_EQ(SQL_ERROR, rc) << "sql type " << c.sql_type;
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY104");
        }
        EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT,
                      stmt_);
    }
}

// SQL_C_UBIGINT is the one C type whose range exceeds every SQL Server integer
// target, so a value above i64::MAX is 22003 rather than a wrapped negative.
TEST_F(PrepareExecuteLiveTest, UbigintAboveBigintMaxIs22003) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLUBIGINT value = 9223372036854775808ULL;  // i64::MAX + 1
    SQLLEN ind = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_UBIGINT,
                                   SQL_BIGINT, 0, 0, &value, 0, &ind),
                  SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "22003");
}

// For a fixed-width C type StrLen_or_Ind is not a length - the size comes from
// the C type - so a stray value is ignored rather than validated. msodbcsql
// consults the indicator only for non-fixed C types (`!IsFixedCType(...) &&
// cbValue > 0`, sqlccmd.cpp:4128 and :4539) and likewise overrides BufferLength
// for them at bind (sqlcdesc.cpp:2507).
TEST_F(PrepareExecuteLiveTest, StrayIndicatorIsIgnoredForFixedWidthCTypes) {
    // -7 is HY090 on a character binding and ignored here; 999 would overread a
    // 4-byte buffer if it were ever honoured as a length.
    for (SQLLEN stray : {static_cast<SQLLEN>(-7), static_cast<SQLLEN>(0),
                         static_cast<SQLLEN>(999)}) {
        ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

        SQLINTEGER value = 42;
        SQLLEN ind = stray;
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG,
                                       SQL_INTEGER, 0, 0, &value, 0, &ind),
                      SQL_HANDLE_STMT, stmt_);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ("42", GetColumnChar(1)) << "indicator " << stray;

        EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

// SQLBindParameter: "If StrLen_or_IndPtr is a null pointer, the driver assumes
// that all input parameter values are non-NULL and that character and binary
// data is null-terminated." So this is SQL_NTS, not a NULL parameter.
TEST_F(PrepareExecuteLiveTest, NullIndicatorPointerMeansNullTerminated) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value = {'a', 'b', 'c', '\0'};
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                   SQL_VARCHAR, value.size(), 0, value.data(),
                                   static_cast<SQLLEN>(value.size()), nullptr),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abc", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A defaulted binding is checked against the conversion matrix like an explicit
// one, so an application gets the same answer either way. SQL_GUID has no
// conversion row yet, so the bind is rejected with HYC00 - unbuilt, not illegal.
// msodbcsql accepts it (it resolves to SQL_C_CHAR via rgbTRANSTYPE380 and can
// convert), hence the skip.
//
// Re-enable as a round-trip test when SQL_C_GUID -> SQL_GUID lands: AB#47500.
TEST_F(PrepareExecuteLiveTest, DefaultCTypeGuidIsRejectedAtBind) {
    SKIP_IF_COMPARING_MSODBCSQL();

    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLGUID value = {};
    SQLLEN ind = 0;
    EXPECT_EQ(SQL_ERROR,
              SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_DEFAULT,
                               SQL_GUID, 0, 0, &value, sizeof(value), &ind));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// A NULL-indicator parameter produces a SQL NULL result.
TEST_F(PrepareExecuteLiveTest, NullParam) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value = {'i', 'g', 'n', 'o', 'r', 'e', 'd', '\0'};
    SQLLEN ind = SQL_NULL_DATA;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLLEN ind_out = 0;
    GetColumnChar(1, &ind_out);
    EXPECT_EQ(SQL_NULL_DATA, ind_out);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Multiple parameters are bound in order and substituted positionally,
// including a NULL-indicator parameter mixed in with non-NULL ones.
TEST_F(PrepareExecuteLiveTest, MultipleParams) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(? AS INT) + CAST(? AS INT) AS s, ? AS n"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> a = {'3', '\0'};
    std::vector<SQLCHAR> b = {'4', '\0'};
    std::vector<SQLCHAR> c = {'i', 'g', 'n', 'o', 'r', 'e', 'd', '\0'};
    SQLLEN ind_a = SQL_NTS, ind_b = SQL_NTS, ind_c = SQL_NULL_DATA;
    ASSERT_SQL_OK(BindChar(1, a, ind_a), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindChar(2, b, ind_b), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindChar(3, c, ind_c), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("7", GetColumnChar(1));

    SQLLEN ind_out = 0;
    GetColumnChar(2, &ind_out);
    EXPECT_EQ(SQL_NULL_DATA, ind_out);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A parameter used in a WHERE clause filters rows correctly.
TEST_F(PrepareExecuteLiveTest, ParamInWhereClause) {
    ExecDirect("CREATE TABLE #people (id INT, name VARCHAR(50))");
    ExecDirect("INSERT INTO #people VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')");

    ASSERT_SQL_OK(Prepare("SELECT id FROM #people WHERE name = ?"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> name = {'b', 'o', 'b', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, name, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetColumnChar(1));

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Re-executing a prepared statement with a changed parameter buffer reuses the
// cached server handle (sp_execute) and reflects the new value. The buffer is
// read by reference at each SQLExecute.
TEST_F(PrepareExecuteLiveTest, ReExecuteWithNewParamValue) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    // Fixed-capacity buffer so its address is stable across re-execute — the
    // driver reads the bound buffer by reference at each SQLExecute, so it must
    // not be reallocated between calls.
    std::vector<SQLCHAR> value(32, 0);
    std::memcpy(value.data(), "first", 6);
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("first", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Overwrite the SAME buffer in place (no reallocation) and re-execute.
    std::memset(value.data(), 0, value.size());
    std::memcpy(value.data(), "second", 7);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("second", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Prepare once, execute many times with different parameter values. The plan is
// prepared on the first execute (sp_prepexec) and reused via sp_execute on every
// subsequent call; the bound buffer is re-read by reference each time.
TEST_F(PrepareExecuteLiveTest, PrepareOnceExecuteMany) {
    ASSERT_SQL_OK(Prepare("SELECT CAST(? AS INT) * 2 AS v"), SQL_HANDLE_STMT, stmt_);

    // Fixed-capacity buffer bound once; its address must stay stable across the
    // re-executes since the driver reads it by reference at each SQLExecute.
    std::vector<SQLCHAR> value(16, 0);
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    for (int i = 1; i <= 5; ++i) {
        std::string in = std::to_string(i);
        std::memset(value.data(), 0, value.size());
        std::memcpy(value.data(), in.c_str(), in.size() + 1);

        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(std::to_string(i * 2), GetColumnChar(1));
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
}

// Rebinding a parameter after an execute must not corrupt the connection: the
// second execute returning the new value proves the statement remains usable
// after the rebind (which internally orphans the cached handle for release).
// This is a behavioral check only — that sp_unprepare + sp_prepexec actually
// fire is asserted by the unit test rebind_invalidates_cached_prepared_handle;
// both the reused and re-prepared paths return the same value, so this test
// alone cannot distinguish them.
//
// Benefits-from-mock-tds: a mock TDS server could assert sp_unprepare +
// sp_prepexec actually fired, which the returned value alone cannot.
TEST_F(PrepareExecuteLiveTest, RebindReleasesPriorHandleAndReprepares) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> first = {'f', 'i', 'r', 's', 't', '\0'};
    SQLLEN ind1 = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, first, ind1), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("first", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Rebind parameter 1 to a different buffer — invalidates the prepared plan.
    std::vector<SQLCHAR> second = {'s', 'e', 'c', 'o', 'n', 'd', '\0'};
    SQLLEN ind2 = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, second, ind2), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("second", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Re-preparing a statement with new text must not corrupt the connection: the
// execute of the new plan returning its value proves the statement remains
// usable after the re-prepare (which internally orphans the prior handle for
// release). This is a behavioral check only — that the prior handle is actually
// released is asserted by the unit test reprepare_orphans_prior_handle_for_unprepare.
//
// Benefits-from-mock-tds: a mock TDS server could assert the prior handle's
// sp_unprepare / piggybacked @handle drop actually fired.
TEST_F(PrepareExecuteLiveTest, ReprepareReleasesPriorHandle) {
    ASSERT_SQL_OK(Prepare("SELECT 1 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Re-prepare with different text — orphans the first handle.
    ASSERT_SQL_OK(Prepare("SELECT 2 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Freeing a prepared statement while its result-set cursor is still open must
// leave the connection healthy: internally the driver drains the cursor
// (capturing the prepared handle) and issues sp_unprepare, but this test only
// verifies the observable outcome — a fresh statement on the same connection
// executes normally afterward. Uses a private statement so the fixture's stmt_
// teardown is unaffected.
//
// Benefits-from-mock-tds: a mock TDS server could assert the drain + sp_unprepare
// RPCs fired, not just the healthy-connection outcome.
TEST_F(PrepareExecuteLiveTest, FreeWithOpenCursorReleasesHandleAndKeepsConnection) {
    SQLHSTMT s = SQL_NULL_HSTMT;
    ASSERT_EQ(SQL_SUCCESS, SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &s));

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 7 AS v");
    ASSERT_EQ(SQL_SUCCESS,
              SQLPrepare(s, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS));
    ASSERT_EQ(SQL_SUCCESS, SQLExecute(s));
    // Fetch a row but deliberately leave the cursor open.
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(s));

    // Free with the cursor still open — the driver must drain + unprepare.
    ASSERT_EQ(SQL_SUCCESS, SQLFreeHandle(SQL_HANDLE_STMT, s));

    // The connection is still healthy: a fresh statement executes normally.
    ASSERT_SQL_OK(Prepare("SELECT 8 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("8", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A '?' inside a string literal is not a parameter marker.
TEST_F(PrepareExecuteLiveTest, LiteralQuestionMarkIsNotAParam) {
    ASSERT_SQL_OK(Prepare("SELECT '?' AS v"), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("?", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// Re-executing while the cursor is still open is a cursor-state error, but the
// same statement re-executes cleanly once the cursor is closed.
TEST_F(PrepareExecuteLiveTest, ReExecuteWhileCursorOpenReturns24000) {
    ASSERT_SQL_OK(Prepare("SELECT 1"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    // Fetch a row so the cursor is firmly open in the DM's state machine.
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // After closing the cursor, the prepared statement is reusable.
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", GetColumnChar(1));

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQLExecDirect with a bound parameter executes directly (sp_executesql) and
// substitutes the value, with no persistent prepared handle.
TEST_F(PrepareExecuteLiveTest, ExecDirectWithParam) {
    std::vector<SQLCHAR> value = {'d', 'i', 'r', 'e', 'c', 't', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS v");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("direct", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQLExecDirect substitutes multiple bound parameters positionally, including a
// NULL-indicator parameter.
TEST_F(PrepareExecuteLiveTest, ExecDirectWithMultipleParams) {
    std::vector<SQLCHAR> a = {'5', '\0'};
    std::vector<SQLCHAR> b = {'6', '\0'};
    std::vector<SQLCHAR> c = {'i', 'g', 'n', 'o', 'r', 'e', 'd', '\0'};
    SQLLEN ind_a = SQL_NTS, ind_b = SQL_NTS, ind_c = SQL_NULL_DATA;
    ASSERT_SQL_OK(BindChar(1, a, ind_a), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindChar(2, b, ind_b), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(BindChar(3, c, ind_c), SQL_HANDLE_STMT, stmt_);

    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SELECT CAST(? AS INT) + CAST(? AS INT) AS s, ? AS n");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("11", GetColumnChar(1));

    SQLLEN ind_out = 0;
    GetColumnChar(2, &ind_out);
    EXPECT_EQ(SQL_NULL_DATA, ind_out);

    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQLExecDirect with a marker but no bound parameter fails with 07002.
TEST_F(PrepareExecuteLiveTest, ExecDirectUnboundMarkerReturns07002) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS v");
    SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07002");
}

// A prepared statement with an unbound marker fails at execute time.
TEST_F(PrepareExecuteLiveTest, UnboundMarkerReturns07002) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07002");
}

// SQL_RESET_PARAMS drops bindings; a subsequent execute sees an unbound marker.
TEST_F(PrepareExecuteLiveTest, ResetParamsClearsBindings) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value = {'x', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc = SQLExecute(stmt_);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07002");
}

// ParameterNumber 0 is invalid.
TEST_F(PrepareExecuteLiveTest, BindParameterNumberZeroReturns07009) {
    std::vector<SQLCHAR> value = {'x', '\0'};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 0, SQL_PARAM_INPUT, SQL_C_CHAR,
                                    SQL_VARCHAR, 1, 0, value.data(),
                                    static_cast<SQLLEN>(value.size()), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");
}

// Output parameters are not implemented in Phase 1: mssql-odbc rejects the bind
// with HYC00. The reference msodbcsql driver supports output params, so this is
// mssql-odbc-specific behavior — skip it on the msodbcsql comparison leg.
TEST_F(PrepareExecuteLiveTest, OutputParameterReturnsHyc00) {
    SKIP_IF_COMPARING_MSODBCSQL();
    std::vector<SQLCHAR> value = {'x', '\0'};
    SQLLEN ind = SQL_NTS;
    SQLRETURN rc = SQLBindParameter(stmt_, 1, SQL_PARAM_OUTPUT, SQL_C_CHAR,
                                    SQL_VARCHAR, 1, 0, value.data(),
                                    static_cast<SQLLEN>(value.size()), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HYC00");
}

// A re-prepare whose new plan fails at sp_prepexec (syntax error) must leave the
// statement reusable. The failing sp_prepexec carries the prior handle as its
// piggybacked `@handle` drop, and the server releases it while processing the
// RPC — so the driver must forget it, not re-arm it. Mirrors msodbcsql, which
// clears `hPrepDropDeferred` before dispatch and never restores it on failure
// (PrepOrPrepExecQuery, sqlccmd.cpp). Had the driver re-armed the handle, the
// next sp_prepexec would re-drop it and fail with HY000/8179 (handle not found).
TEST_F(PrepareExecuteLiveTest, FailedReprepareKeepsStatementUsable) {
    // Prepare + execute a valid plan so a server handle is cached.
    ASSERT_SQL_OK(Prepare("SELECT 1 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("1", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Re-prepare with failing text: sp_prepexec drops the cached handle but then
    // fails on the syntax error. The driver must forget the released handle.
    ASSERT_SQL_OK(Prepare("SELECT FROM WHERE"), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_ERROR, SQLExecute(stmt_));

    // A fresh plan still prepares and executes cleanly — a re-armed handle would
    // make this sp_prepexec fail with HY000/8179 (handle not found).
    ASSERT_SQL_OK(Prepare("SELECT 2 AS v"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("2", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A prepared parameterized DML statement runs via sp_prepexec on the first
// execute and reuses the cached handle via sp_execute on the next. Every other
// parameterized prepared test is a SELECT; this is the only one that drives the
// no-result-set finish path (drain + prepared-handle capture at DDL/DML finish)
// with parameters.
TEST_F(PrepareExecuteLiveTest, PreparedParamDmlReusesHandle) {
    ExecDirect("CREATE TABLE #nums (v INT)");

    ASSERT_SQL_OK(Prepare("INSERT INTO #nums (v) VALUES (CAST(? AS INT))"),
                  SQL_HANDLE_STMT, stmt_);

    // Fixed-capacity buffer bound once; its address stays stable across executes.
    std::vector<SQLCHAR> value(16, 0);
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    // First execute prepares (sp_prepexec); the second reuses the plan
    // (sp_execute). A DML statement opens no cursor, so no close between executes.
    for (const char* n : {"10", "20"}) {
        std::memset(value.data(), 0, value.size());
        std::memcpy(value.data(), n, std::strlen(n) + 1);
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    }

    // Both rows landed.
    ASSERT_SQL_OK(Prepare("SELECT SUM(v) FROM #nums"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("30", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// SQLExecDirect on a statement that still holds an orphaned prepared handle must
// release it via sp_unprepare before running the batch. Every other ExecDirect
// test runs on a fresh statement where the pending drop is empty, so this is the
// only path that exercises that release actually firing.
TEST_F(PrepareExecuteLiveTest, ExecDirectReleasesOrphanedPreparedHandle) {
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> first = {'f', 'i', 'r', 's', 't', '\0'};
    SQLLEN ind1 = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, first, ind1), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("first", GetColumnChar(1));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Rebind orphans the cached prepared handle, queuing it for release.
    std::vector<SQLCHAR> second = {'s', 'e', 'c', 'o', 'n', 'd', '\0'};
    SQLLEN ind2 = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, second, ind2), SQL_HANDLE_STMT, stmt_);

    // SQLExecDirect supersedes the prepared plan: it must sp_unprepare the
    // orphaned handle and stay healthy (the bound param is ignored — no marker).
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 42 AS v");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("42", GetColumnChar(1));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}

// A prepared parameterized SELECT that matches multiple rows exercises the fetch
// loop under a cached plan; a re-execute with a narrower bound value reuses the
// handle (sp_execute) and returns fewer rows.
TEST_F(PrepareExecuteLiveTest, PreparedParamSelectMultipleRows) {
    ExecDirect("CREATE TABLE #ids (id INT)");
    ExecDirect("INSERT INTO #ids VALUES (1), (2), (3)");

    ASSERT_SQL_OK(Prepare("SELECT id FROM #ids WHERE id <= CAST(? AS INT) ORDER BY id"),
                  SQL_HANDLE_STMT, stmt_);

    std::vector<SQLCHAR> value(16, 0);
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(BindChar(1, value, ind), SQL_HANDLE_STMT, stmt_);

    std::memcpy(value.data(), "3", 2);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    for (const char* expected : {"1", "2", "3"}) {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, GetColumnChar(1));
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // Re-execute reuses the cached plan via sp_execute with a narrower bound.
    std::memset(value.data(), 0, value.size());
    std::memcpy(value.data(), "2", 2);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    for (const char* expected : {"1", "2"}) {
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(expected, GetColumnChar(1));
    }
    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    EXPECT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
}
