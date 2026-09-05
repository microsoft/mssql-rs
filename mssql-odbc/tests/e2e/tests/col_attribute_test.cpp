// Copyright (c) Microsoft Corporation. All rights reserved.
// col_attribute_test.cpp  –  E2E tests for SQLColAttributeW.
//
// Per-type mapping tables (concise type, type name, radix, and the sql_variant
// underlying type) are meaningfully exercised here against live server metadata.
//
// Verifies:
//   1.  NullHandle                        - SQL_NULL_HSTMT → SQL_INVALID_HANDLE
//   2.  FreshStatementReturnsSequenceError- no active stmt → HY010
//   3.  InvalidColumnOrdinal              - column 0 and past-end → 07009
//   4.  UnknownFieldIdentifier            - unreported field id → HY091
//   5.  DescCountIgnoresColumnNumber      - SQL_DESC_COUNT describes the result set
//   6.  ConciseTypePerColumnType          - int/varchar/nvarchar/decimal concise types
//   7.  TypeNameAndRadix                  - SQL_DESC_TYPE_NAME, SQL_DESC_NUM_PREC_RADIX
//   8.  PrecisionScaleAndNullable         - DECIMAL(10,2), NOT NULL vs NULL
//   9.  UnsignedIsFalseOnlyForSignedNumerics - nonnumeric columns are "unsigned"
//   10. DisplaySizeIsRenderedWidth        - sign, hex expansion, characters not bytes
//   11. DisplaySizeForApproximateNumerics - real/float exponential form
//   12. OctetLengthIsTransferSize         - ODBC C struct size, not TDS wire width
//   13. VerboseTypeDiffersFromConciseForTimestamps - SQL_DATETIME + subtype
//   14. VerboseTypeMatchesConciseForNonTimestamps
//   15. SearchableIsDerivedFromTheType    - LIKE-only, unsearchable, full
//   16. IdentityColumnReportsAutoUniqueValue
//   17. AliasedColumnDoesNotReportTheAliasAsBaseColumnName
//   18. NameIsReportedInBytes             - SQL_DESC_NAME length is a byte count
//   19. NameTruncationReturnsInfo         - short buffer → SUCCESS_WITH_INFO + 01004
//   20. VariantTypeOnNonVariantColumn     - HY113
//   21. VariantUnderlyingTypeAfterProbe   - probe then SQL_CA_SS_VARIANT_TYPE
//   22. VariantTypeBeforeProbeIsSequenceError - attribute before the value is read
//   23. ClrUdtDescriptorFields             - CLR UDT type and size-bearing fields
//   24. Odbc2TemporalVariantTypes          - legacy codes and SS binary fallback
//   25. Odbc3TemporalVariantTypes          - legacy codes and SS binary fallback
//   26. Odbc38TemporalVariantTypes         - legacy codes and SS extended types
//   27. EmptyVariantProbeConsumesValueButKeepsBaseType - base type survives the probe
//   28. VariantExactNumericsReportNumeric - decimal/numeric/money/smallmoney → SQL_C_NUMERIC
//   29. VariantDecimalStillDeliversAsCharacter - the SQL_C_CHAR read after the attribute
//   30. VariantBaseTypesMatchMsodbcsql    - every measured-parity base type

#include "odbc_test_fixture.h"

#include <algorithm>
#include <string>

// SQL Server-specific identifiers not in standard <sqlext.h>.
#ifndef SQL_CA_SS_VARIANT_TYPE
#define SQL_CA_SS_VARIANT_TYPE (1215)
#endif
#ifndef SQL_SS_VARIANT
#define SQL_SS_VARIANT (-150)
#endif
#ifndef SQL_SS_UDT
#define SQL_SS_UDT (-151)
#endif
#ifndef SQL_C_SS_TIME2
#define SQL_C_SS_TIME2 0x4000
#endif
#ifndef SQL_C_SS_TIMESTAMPOFFSET
#define SQL_C_SS_TIMESTAMPOFFSET 0x4001
#endif

class ColAttributeLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

class ColAttributeOdbc3LiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        ASSERT_SQL_OK(SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                                    reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3), 0),
                      SQL_HANDLE_ENV, env_);
        Connect();
    }
};

class ColAttributeOdbc2LiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured - set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        ASSERT_SQL_OK(SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                                    reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC2), 0),
                      SQL_HANDLE_ENV, env_);
        Connect();
    }
};

// Reads a numeric attribute, asserting the call succeeded.
static SQLLEN NumericAttr(SQLHSTMT stmt, SQLUSMALLINT col, SQLUSMALLINT field) {
    SQLLEN value = -1;
    SQLRETURN rc = SQLColAttribute(stmt, col, field, nullptr, 0, nullptr, &value);
    EXPECT_TRUE(SQL_SUCCEEDED(rc)) << "field " << field;
    return value;
}

static void ExpectVariantType(SQLHSTMT stmt, SQLUSMALLINT col, SQLLEN expected) {
    SCOPED_TRACE("column " + std::to_string(col));
    SQLCHAR probe = 0;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt, col, SQL_C_BINARY, &probe, 0, &indicator),
                  SQL_HANDLE_STMT, stmt);
    EXPECT_EQ(expected, NumericAttr(stmt, col, SQL_CA_SS_VARIANT_TYPE));
}

static constexpr const char* TEMPORAL_VARIANTS_QUERY =
    "SELECT CAST(CAST('2026-09-03' AS DATE) AS SQL_VARIANT),"
    " CAST(CAST('2026-09-03T12:34:00' AS SMALLDATETIME) AS SQL_VARIANT),"
    " CAST(CAST('2026-09-03T12:34:56.123' AS DATETIME) AS SQL_VARIANT),"
    " CAST(CAST('2026-09-03T12:34:56.1234567' AS DATETIME2(7)) AS SQL_VARIANT),"
    " CAST(CAST('12:34:56.1234567' AS TIME(7)) AS SQL_VARIANT),"
    " CAST(CAST('2026-09-03T12:34:56.1234567+05:30' AS DATETIMEOFFSET(7))"
    " AS SQL_VARIANT)";

TEST(ColAttributeTest, NullHandle) {
    SQLLEN value = 0;
    SQLRETURN rc = SQLColAttribute(
        SQL_NULL_HSTMT, 1, SQL_DESC_CONCISE_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

TEST_F(ColAttributeLiveTest, FreshStatementReturnsSequenceError) {
    SQLLEN value = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_DESC_CONCISE_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
}

TEST_F(ColAttributeLiveTest, InvalidColumnOrdinal) {
    ExecDirect("SELECT CAST(1 AS INT) AS c1");
    for (SQLUSMALLINT col : {static_cast<SQLUSMALLINT>(0), static_cast<SQLUSMALLINT>(2)}) {
        SQLLEN value = 0;
        SQLRETURN rc =
            SQLColAttribute(stmt_, col, SQL_DESC_CONCISE_TYPE, nullptr, 0, nullptr, &value);
        EXPECT_EQ(SQL_ERROR, rc) << "column " << col;
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");
    }
    SQLCloseCursor(stmt_);
}

// An identifier this driver does not report is rejected rather than answered
// with a silent zero. msodbcsql reports a wider set, so only compare our leg.
TEST_F(ColAttributeLiveTest, UnknownFieldIdentifier) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("SELECT CAST(1 AS INT) AS c1");
    SQLLEN value = 0;
    SQLRETURN rc = SQLColAttribute(stmt_, 1, 9999, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY091");
    SQLCloseCursor(stmt_);
}

// SQL_DESC_COUNT describes the result set, so it answers for a column number
// that would otherwise be out of range.
TEST_F(ColAttributeLiveTest, DescCountIgnoresColumnNumber) {
    ExecDirect("SELECT 1 AS a, 2 AS b, 3 AS c");
    EXPECT_EQ(3, NumericAttr(stmt_, 1, SQL_DESC_COUNT));
    EXPECT_EQ(3, NumericAttr(stmt_, 99, SQL_DESC_COUNT));
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, ConciseTypePerColumnType) {
    ExecDirect(
        "SELECT CAST(1 AS INT) AS i, CAST('a' AS VARCHAR(10)) AS v,"
        " CAST(N'b' AS NVARCHAR(10)) AS n, CAST(1.5 AS DECIMAL(10,2)) AS d");
    EXPECT_EQ(SQL_INTEGER, NumericAttr(stmt_, 1, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(SQL_VARCHAR, NumericAttr(stmt_, 2, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(SQL_WVARCHAR, NumericAttr(stmt_, 3, SQL_DESC_CONCISE_TYPE));
    EXPECT_EQ(SQL_DECIMAL, NumericAttr(stmt_, 4, SQL_DESC_CONCISE_TYPE));
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, ClrUdtDescriptorFields) {
    ExecDirect("SELECT CAST(NULL AS geography) AS geography_col, "
               "CAST(NULL AS geometry) AS geometry_col, "
               "CAST(NULL AS hierarchyid) AS hierarchyid_col");

    struct UdtColumn {
        SQLUSMALLINT ordinal;
        SQLLEN size;
    };
    const UdtColumn columns[] = {
        {1, 0},
        {2, 0},
        {3, 892},
    };

    for (const auto& column : columns) {
        EXPECT_EQ(SQL_SS_UDT, NumericAttr(stmt_, column.ordinal, SQL_DESC_TYPE))
            << "column " << column.ordinal;
        EXPECT_EQ(SQL_SS_UDT, NumericAttr(stmt_, column.ordinal, SQL_DESC_CONCISE_TYPE))
            << "column " << column.ordinal;
        EXPECT_EQ(column.size, NumericAttr(stmt_, column.ordinal, SQL_DESC_LENGTH))
            << "column " << column.ordinal;
        EXPECT_EQ(column.size, NumericAttr(stmt_, column.ordinal, SQL_DESC_PRECISION))
            << "column " << column.ordinal;
        EXPECT_EQ(column.size, NumericAttr(stmt_, column.ordinal, SQL_DESC_OCTET_LENGTH))
            << "column " << column.ordinal;
        EXPECT_EQ(0, NumericAttr(stmt_, column.ordinal, SQL_DESC_DISPLAY_SIZE))
            << "column " << column.ordinal;
    }
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, TypeNameAndRadix) {
    ExecDirect("SELECT CAST(1 AS INT) AS i, CAST(1.5 AS FLOAT) AS f,"
               " CAST('a' AS VARCHAR(10)) AS v");

    SQLTCHAR name[64] = {};
    SQLSMALLINT nameLen = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_DESC_TYPE_NAME, name, sizeof(name), &nameLen, nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("int", ODBCTestUtils::ToNarrow(SqlTString(name)));

    // Exact numerics are base 10, approximate are base 2, non-numerics have none.
    EXPECT_EQ(10, NumericAttr(stmt_, 1, SQL_DESC_NUM_PREC_RADIX));
    EXPECT_EQ(2, NumericAttr(stmt_, 2, SQL_DESC_NUM_PREC_RADIX));
    EXPECT_EQ(0, NumericAttr(stmt_, 3, SQL_DESC_NUM_PREC_RADIX));
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, PrecisionScaleAndNullable) {
    ExecDirect("SELECT CAST(1.5 AS DECIMAL(10,2)) AS d,"
               " CAST(NULL AS INT) AS n");
    EXPECT_EQ(10, NumericAttr(stmt_, 1, SQL_DESC_PRECISION));
    EXPECT_EQ(2, NumericAttr(stmt_, 1, SQL_DESC_SCALE));
    EXPECT_EQ(SQL_NULLABLE, NumericAttr(stmt_, 2, SQL_DESC_NULLABLE));
    SQLCloseCursor(stmt_);
}

// `tinyint` is the only unsigned integer SQL Server exposes.
// SQL_DESC_UNSIGNED is SQL_FALSE only for the signed numeric types. Every
// nonnumeric column is "unsigned" by the ODBC definition, which is the opposite
// of the intuitive reading and the easiest field to get backwards.
TEST_F(ColAttributeLiveTest, UnsignedIsFalseOnlyForSignedNumerics) {
    ExecDirect(
        "SELECT CAST(1 AS TINYINT) AS c1, CAST(1 AS SMALLINT) AS c2,"
        "       CAST(1 AS INT) AS c3, CAST(1 AS BIGINT) AS c4,"
        "       CAST(1 AS REAL) AS c5, CAST(1 AS FLOAT) AS c6,"
        "       CAST(1 AS DECIMAL(10,2)) AS c7, CAST(1 AS BIT) AS c8,"
        "       CAST('a' AS VARCHAR(10)) AS c9, CAST(N'a' AS NVARCHAR(10)) AS c10,"
        "       CAST(0x01 AS VARBINARY(8)) AS c11, CAST('2020-01-01' AS DATE) AS c12,"
        "       CAST(SYSDATETIME() AS DATETIME2(3)) AS c13, NEWID() AS c14,"
        "       CAST(1 AS MONEY) AS c15");
    // tinyint is the one unsigned integer.
    EXPECT_EQ(SQL_TRUE, NumericAttr(stmt_, 1, SQL_DESC_UNSIGNED)) << "tinyint";
    // money is reported as SQL_DECIMAL, so it is a signed numeric too.
    for (SQLUSMALLINT col : {2, 3, 4, 5, 6, 7, 15}) {
        EXPECT_EQ(SQL_FALSE, NumericAttr(stmt_, col, SQL_DESC_UNSIGNED))
            << "signed numeric column " << col;
    }
    for (SQLUSMALLINT col : {8, 9, 10, 11, 12, 13, 14}) {
        EXPECT_EQ(SQL_TRUE, NumericAttr(stmt_, col, SQL_DESC_UNSIGNED))
            << "nonnumeric column " << col;
    }
    SQLCloseCursor(stmt_);
}

// Display size is the rendered width, not the column size: an int needs a
// character for the sign, a GUID renders as 36, and binary renders as two hex
// characters per byte.
TEST_F(ColAttributeLiveTest, DisplaySizeIsRenderedWidth) {
    ExecDirect(
        "SELECT CAST(1 AS INT) AS c1, CAST(1 AS TINYINT) AS c2,"
        "       CAST(1 AS SMALLINT) AS c3, CAST(1 AS BIGINT) AS c4,"
        "       NEWID() AS c5, CAST(0x01 AS BINARY(8)) AS c6,"
        "       CAST(1 AS DECIMAL(10,2)) AS c7, CAST(1 AS BIT) AS c8,"
        "       CAST('a' AS VARCHAR(10)) AS c9, CAST(N'a' AS NVARCHAR(10)) AS c10");
    EXPECT_EQ(11, NumericAttr(stmt_, 1, SQL_DESC_DISPLAY_SIZE)) << "int";
    EXPECT_EQ(3, NumericAttr(stmt_, 2, SQL_DESC_DISPLAY_SIZE)) << "tinyint";
    EXPECT_EQ(6, NumericAttr(stmt_, 3, SQL_DESC_DISPLAY_SIZE)) << "smallint";
    EXPECT_EQ(20, NumericAttr(stmt_, 4, SQL_DESC_DISPLAY_SIZE)) << "bigint";
    EXPECT_EQ(36, NumericAttr(stmt_, 5, SQL_DESC_DISPLAY_SIZE)) << "uniqueidentifier";
    EXPECT_EQ(16, NumericAttr(stmt_, 6, SQL_DESC_DISPLAY_SIZE)) << "binary(8)";
    // Precision plus the sign and the decimal point.
    EXPECT_EQ(12, NumericAttr(stmt_, 7, SQL_DESC_DISPLAY_SIZE)) << "decimal(10,2)";
    EXPECT_EQ(1, NumericAttr(stmt_, 8, SQL_DESC_DISPLAY_SIZE)) << "bit";
    EXPECT_EQ(10, NumericAttr(stmt_, 9, SQL_DESC_DISPLAY_SIZE)) << "varchar(10)";
    // Characters, not bytes.
    EXPECT_EQ(10, NumericAttr(stmt_, 10, SQL_DESC_DISPLAY_SIZE)) << "nvarchar(10)";
    SQLCloseCursor(stmt_);
}

// The approximate numerics report the width of their rendered exponential form,
// which is unrelated to both the column size and the wire width.
TEST_F(ColAttributeLiveTest, DisplaySizeForApproximateNumerics) {
    ExecDirect("SELECT CAST(1 AS REAL) AS c1, CAST(1 AS FLOAT) AS c2");
    EXPECT_EQ(14, NumericAttr(stmt_, 1, SQL_DESC_DISPLAY_SIZE)) << "real";
    EXPECT_EQ(24, NumericAttr(stmt_, 2, SQL_DESC_DISPLAY_SIZE)) << "float";
    SQLCloseCursor(stmt_);
}

// SQL_DESC_OCTET_LENGTH is the size of the ODBC transfer representation, so the
// temporal types report their C struct size, not the TDS payload width. A date
// is 3 bytes on the wire but transfers as a 6-byte SQL_DATE_STRUCT; reporting
// the wire width would have callers allocate short.
TEST_F(ColAttributeLiveTest, OctetLengthIsTransferSize) {
    ExecDirect(
        "SELECT CAST('2020-01-01' AS DATE) AS c1,"
        "       CAST(SYSDATETIME() AS TIME(3)) AS c2,"
        "       CAST(SYSDATETIME() AS DATETIME) AS c3,"
        "       CAST(SYSDATETIME() AS SMALLDATETIME) AS c4,"
        "       CAST(SYSDATETIME() AS DATETIME2(7)) AS c5,"
        "       CAST(SYSDATETIMEOFFSET() AS DATETIMEOFFSET(7)) AS c6,"
        "       CAST(1 AS INT) AS c7, NEWID() AS c8,"
        "       CAST('a' AS VARCHAR(10)) AS c9, CAST(N'a' AS NVARCHAR(10)) AS c10");
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQL_DATE_STRUCT)),
              NumericAttr(stmt_, 1, SQL_DESC_OCTET_LENGTH))
        << "date";
    EXPECT_EQ(12, NumericAttr(stmt_, 2, SQL_DESC_OCTET_LENGTH)) << "time(3)";
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQL_TIMESTAMP_STRUCT)),
              NumericAttr(stmt_, 3, SQL_DESC_OCTET_LENGTH))
        << "datetime";
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQL_TIMESTAMP_STRUCT)),
              NumericAttr(stmt_, 4, SQL_DESC_OCTET_LENGTH))
        << "smalldatetime";
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(SQL_TIMESTAMP_STRUCT)),
              NumericAttr(stmt_, 5, SQL_DESC_OCTET_LENGTH))
        << "datetime2(7)";
    EXPECT_EQ(20, NumericAttr(stmt_, 6, SQL_DESC_OCTET_LENGTH)) << "datetimeoffset(7)";
    // The non-temporal types transfer at their wire width.
    EXPECT_EQ(4, NumericAttr(stmt_, 7, SQL_DESC_OCTET_LENGTH)) << "int";
    EXPECT_EQ(16, NumericAttr(stmt_, 8, SQL_DESC_OCTET_LENGTH)) << "uniqueidentifier";
    EXPECT_EQ(10, NumericAttr(stmt_, 9, SQL_DESC_OCTET_LENGTH)) << "varchar(10)";
    // Bytes, not characters.
    EXPECT_EQ(20, NumericAttr(stmt_, 10, SQL_DESC_OCTET_LENGTH)) << "nvarchar(10)";
    SQLCloseCursor(stmt_);
}

// SQL_DESC_TYPE is the verbose field: the timestamp family collapses to
// SQL_DATETIME with the member in SQL_DESC_DATETIME_INTERVAL_CODE, while
// SQL_DESC_CONCISE_TYPE keeps SQL_TYPE_TIMESTAMP.
TEST_F(ColAttributeLiveTest, VerboseTypeDiffersFromConciseForTimestamps) {
    ExecDirect(
        "SELECT CAST(SYSDATETIME() AS DATETIME) AS c1,"
        "       CAST(SYSDATETIME() AS SMALLDATETIME) AS c2,"
        "       CAST(SYSDATETIME() AS DATETIME2(3)) AS c3");
    for (SQLUSMALLINT col : {1, 2, 3}) {
        EXPECT_EQ(SQL_DATETIME, NumericAttr(stmt_, col, SQL_DESC_TYPE))
            << "verbose type, column " << col;
        EXPECT_EQ(SQL_TYPE_TIMESTAMP, NumericAttr(stmt_, col, SQL_DESC_CONCISE_TYPE))
            << "concise type, column " << col;
    }
    SQLCloseCursor(stmt_);
}

// Having collapsed the verbose type to SQL_DATETIME, the driver still answers
// which member it was. msodbcsql rejects this field through SQLColAttribute, so
// only our leg is compared.
TEST_F(ColAttributeLiveTest, DatetimeSubtypeAccompaniesTheVerboseType) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect(
        "SELECT CAST(SYSDATETIME() AS DATETIME) AS c1,"
        "       CAST(SYSDATETIME() AS DATETIME2(3)) AS c2,"
        "       CAST(1 AS INT) AS c3");
    EXPECT_EQ(SQL_CODE_TIMESTAMP, NumericAttr(stmt_, 1, SQL_DESC_DATETIME_INTERVAL_CODE));
    EXPECT_EQ(SQL_CODE_TIMESTAMP, NumericAttr(stmt_, 2, SQL_DESC_DATETIME_INTERVAL_CODE));
    // Not a datetime, so there is no subtype.
    EXPECT_EQ(0, NumericAttr(stmt_, 3, SQL_DESC_DATETIME_INTERVAL_CODE));
    SQLCloseCursor(stmt_);
}

// date, time and datetimeoffset sit outside the ODBC datetime range, so the
// verbose and concise fields agree.
TEST_F(ColAttributeLiveTest, VerboseTypeMatchesConciseForNonTimestamps) {
    ExecDirect(
        "SELECT CAST('2020-01-01' AS DATE) AS c1,"
        "       CAST(SYSDATETIME() AS TIME(3)) AS c2,"
        "       CAST(SYSDATETIMEOFFSET() AS DATETIMEOFFSET(3)) AS c3,"
        "       CAST(1 AS INT) AS c4");
    for (SQLUSMALLINT col : {1, 2, 3, 4}) {
        EXPECT_EQ(NumericAttr(stmt_, col, SQL_DESC_CONCISE_TYPE),
                  NumericAttr(stmt_, col, SQL_DESC_TYPE))
            << "column " << col;
    }
    SQLCloseCursor(stmt_);
}

// Searchability is derived from the type: the LOB text types take only LIKE and
// xml/image take neither, so a blanket SQL_PRED_SEARCHABLE overstates them.
TEST_F(ColAttributeLiveTest, SearchableIsDerivedFromTheType) {
    ExecDirect(
        "SELECT CAST('a' AS VARCHAR(10)) AS c1, CAST(N'a' AS NVARCHAR(10)) AS c2,"
        "       CAST('2020-01-01' AS DATE) AS c3, CAST(1 AS INT) AS c4,"
        "       CAST(0x01 AS VARBINARY(8)) AS c5, NEWID() AS c6,"
        "       CAST('a' AS TEXT) AS c7, CAST(N'a' AS NTEXT) AS c8,"
        "       CAST(0x01 AS IMAGE) AS c9, CAST('<x/>' AS XML) AS c10");
    EXPECT_EQ(SQL_PRED_SEARCHABLE, NumericAttr(stmt_, 1, SQL_DESC_SEARCHABLE)) << "varchar";
    EXPECT_EQ(SQL_PRED_SEARCHABLE, NumericAttr(stmt_, 2, SQL_DESC_SEARCHABLE)) << "nvarchar";
    EXPECT_EQ(SQL_PRED_SEARCHABLE, NumericAttr(stmt_, 3, SQL_DESC_SEARCHABLE)) << "date";
    EXPECT_EQ(SQL_PRED_BASIC, NumericAttr(stmt_, 4, SQL_DESC_SEARCHABLE)) << "int";
    EXPECT_EQ(SQL_PRED_BASIC, NumericAttr(stmt_, 5, SQL_DESC_SEARCHABLE)) << "varbinary";
    EXPECT_EQ(SQL_PRED_BASIC, NumericAttr(stmt_, 6, SQL_DESC_SEARCHABLE)) << "uniqueidentifier";
    EXPECT_EQ(SQL_PRED_CHAR, NumericAttr(stmt_, 7, SQL_DESC_SEARCHABLE)) << "text";
    EXPECT_EQ(SQL_PRED_CHAR, NumericAttr(stmt_, 8, SQL_DESC_SEARCHABLE)) << "ntext";
    EXPECT_EQ(SQL_PRED_NONE, NumericAttr(stmt_, 9, SQL_DESC_SEARCHABLE)) << "image";
    EXPECT_EQ(SQL_PRED_NONE, NumericAttr(stmt_, 10, SQL_DESC_SEARCHABLE)) << "xml";
    SQLCloseCursor(stmt_);
}

// SQL_DESC_AUTO_UNIQUE_VALUE reflects IDENTITY, which needs a real table since
// a projected expression is never an identity column.
TEST_F(ColAttributeLiveTest, IdentityColumnReportsAutoUniqueValue) {
    ExecDirect("CREATE TABLE #colattr_identity (id INT IDENTITY(1,1), val INT)");
    ExecDirect("SELECT id, val FROM #colattr_identity");
    EXPECT_EQ(SQL_TRUE, NumericAttr(stmt_, 1, SQL_DESC_AUTO_UNIQUE_VALUE)) << "identity";
    EXPECT_EQ(SQL_FALSE, NumericAttr(stmt_, 2, SQL_DESC_AUTO_UNIQUE_VALUE)) << "plain int";
    SQLCloseCursor(stmt_);
    ExecDirect("DROP TABLE #colattr_identity");
}

// SQL_DESC_NAME and SQL_DESC_LABEL report the alias; SQL_DESC_BASE_COLUMN_NAME
// must not. Without FOR BROWSE the server sends no base-column name, so ODBC
// asks for an empty string rather than the alias.
TEST_F(ColAttributeLiveTest, AliasedColumnDoesNotReportTheAliasAsBaseColumnName) {
    ExecDirect("CREATE TABLE #colattr_alias (source_col INT)");
    ExecDirect("SELECT source_col AS alias_col FROM #colattr_alias");

    SQLTCHAR buf[64] = {};
    SQLSMALLINT len = 0;
    ASSERT_SQL_OK(
        SQLColAttribute(stmt_, 1, SQL_DESC_NAME, buf, sizeof(buf), &len, nullptr),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("alias_col", ODBCTestUtils::ToNarrow(SqlTString(buf)));

    len = 0;
    std::fill(std::begin(buf), std::end(buf), 0);
    ASSERT_SQL_OK(
        SQLColAttribute(stmt_, 1, SQL_DESC_BASE_COLUMN_NAME, buf, sizeof(buf), &len, nullptr),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_NE("alias_col", ODBCTestUtils::ToNarrow(SqlTString(buf)))
        << "the alias must not be reported as the base column name";

    SQLCloseCursor(stmt_);
    ExecDirect("DROP TABLE #colattr_alias");
}

// The wide entry point reports string lengths in bytes, not characters.
TEST_F(ColAttributeLiveTest, NameIsReportedInBytes) {
    ExecDirect("SELECT 1 AS abcd");
    SQLTCHAR name[32] = {};
    SQLSMALLINT nameLen = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_DESC_NAME, name, sizeof(name), &nameLen, nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("abcd", ODBCTestUtils::ToNarrow(SqlTString(name)));
    EXPECT_EQ(static_cast<SQLSMALLINT>(4 * sizeof(SQLTCHAR)), nameLen);
    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, NameTruncationReturnsInfo) {
    ExecDirect("SELECT 1 AS averylongcolumnname");
    SQLTCHAR name[3] = {};
    SQLSMALLINT nameLen = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_DESC_NAME, name, sizeof(name), &nameLen, nullptr);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    SQLCloseCursor(stmt_);
}

// The variant attribute is rejected outright on a column that is not a
// sql_variant, rather than reporting a type the caller would then trust.
//
// msodbcsql-specific: msodbcsql returns SUCCESS here. Its `SQL_CA_SS_VARIANT_TYPE`
// case sets `wError = IDS_S1_113` and then plain `break`s, where the neighbouring
// `SQL_CA_SS_VARIANT_SERVER_TYPE` case does `SETRC_SERR_GOTO(retcode, ErrorRet)`
// with the same error — so the diagnostic it prepares is never actually returned.
// Recorded in the divergence table in docs/typed-columnar-fetch-plan.md.
TEST_F(ColAttributeLiveTest, VariantTypeOnNonVariantColumn) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("SELECT CAST(1 AS INT) AS c1");
    SQLLEN value = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_CA_SS_VARIANT_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY113");
    SQLCloseCursor(stmt_);
}

// The full sequence an application uses to read a sql_variant: describe the
// column, probe it with a zero-length SQL_C_BINARY read, then ask for the
// underlying C type. The underlying type belongs to the value, so it tracks the
// row rather than the column.
TEST_F(ColAttributeLiveTest, VariantUnderlyingTypeAfterProbe) {
    ExecDirect(
        "SELECT CAST(42 AS SQL_VARIANT) AS v"
        " UNION ALL SELECT CAST(CAST('abc' AS VARCHAR(10)) AS SQL_VARIANT)");

    SQLSMALLINT dataType = 0;
    SQLRETURN rc = SQLDescribeCol(
        stmt_, 1, nullptr, 0, nullptr, &dataType, nullptr, nullptr, nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_SS_VARIANT, dataType);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    // The probe is keyed on a zero buffer length, not on a null pointer:
    // mssql-python passes NULL here, but it dlopen's the driver directly, while
    // these tests go through the Driver Manager, which rejects a null
    // TargetValuePtr with HY009 before the driver ever sees the call.
    SQLCHAR probe = 0;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_NE(SQL_NULL_DATA, indicator);
    EXPECT_EQ(SQL_C_SLONG, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

    // Second row holds a different base type in the same column.
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_C_CHAR, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

    SQLCloseCursor(stmt_);
}

// ODBC 3.8 introduced the SQL Server temporal C types. Applications declaring
// ODBC 2 or 3 receive the binary fallback for time and datetimeoffset.
TEST_F(ColAttributeOdbc2LiveTest, Odbc2TemporalVariantTypes) {
    ExecDirect(TEMPORAL_VARIANTS_QUERY);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 1, SQL_C_DATE));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 2, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 3, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 4, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 5, SQL_C_BINARY));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 6, SQL_C_BINARY));

    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeOdbc3LiveTest, Odbc3TemporalVariantTypes) {
    ExecDirect(TEMPORAL_VARIANTS_QUERY);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 1, SQL_C_DATE));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 2, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 3, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 4, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 5, SQL_C_BINARY));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 6, SQL_C_BINARY));

    SQLCloseCursor(stmt_);
}

TEST_F(ColAttributeLiveTest, Odbc38TemporalVariantTypes) {
    ExecDirect(TEMPORAL_VARIANTS_QUERY);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 1, SQL_C_DATE));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 2, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 3, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 4, SQL_C_TIMESTAMP));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 5, SQL_C_SS_TIME2));
    ASSERT_NO_FATAL_FAILURE(ExpectVariantType(stmt_, 6, SQL_C_SS_TIMESTAMPOFFSET));

    SQLCloseCursor(stmt_);
}

// AB#47537 follow-up: the empty-value case of the zero-length SQL_C_BINARY
// probe now *consumes* the column (there was nothing left to deliver), so this
// pins the interaction with variant base-type tracking -- the two live in
// different state (`last_captured` vs `last_variant_base`), and nothing else
// would catch a future change that cleared both together.
//
// This is the exact sequence mssql-python runs on a sql_variant column
// (`ddbc_bindings.cpp`): zero-length SQL_C_BINARY probe, then
// SQLColAttribute(SQL_CA_SS_VARIANT_TYPE), then a read using the reported C
// type. The base type must still be answerable after the probe consumed the
// value, and the follow-up read must report SQL_NO_DATA rather than handing
// back a stale value.
//
// Measured on both legs: base type SQL_C_BINARY and a SQL_NO_DATA re-read agree
// exactly. The probe's own return code is the one divergence -- msodbcsql
// 18.6.2.1 answers SQL_SUCCESS_WITH_INFO/01004 for a variant wrapping an empty
// value where this driver answers SQL_SUCCESS (a bare empty `varbinary(8)`,
// with no variant wrapper, is plain SQL_SUCCESS on both). ASSERT_SQL_OK accepts
// either, so this test asserts the parts that matter without pinning that
// difference; it is invisible to mssql-python, whose probe is gated on
// SQL_SUCCEEDED.
TEST_F(ColAttributeLiveTest, EmptyVariantProbeConsumesValueButKeepsBaseType) {
    ExecDirect("SELECT CAST(CAST('' AS VARBINARY(8)) AS SQL_VARIANT) AS v");
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR probe = 0;
    SQLLEN indicator = -999;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(0, indicator) << "an empty value reports zero bytes, not SQL_NULL_DATA";

    // The probe consumed the value; the base type must survive it.
    EXPECT_EQ(SQL_C_BINARY, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

    // Nothing remained, so the column is done rather than re-readable.
    EXPECT_EQ(SQL_NO_DATA, SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator));

    SQLCloseCursor(stmt_);
}

// A NULL sql_variant is just a zero length on the wire, with no base type or
// property byte following it. Reading those anyway would consume the next
// column's bytes, so the column after the variant is what actually proves it.
TEST_F(ColAttributeLiveTest, NullVariantDoesNotDisturbTheFollowingColumn) {
    ExecDirect("SELECT CAST(NULL AS SQL_VARIANT) AS v, CAST(12345 AS INT) AS following");

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR probe = 0;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NULL_DATA, indicator);

    SQLINTEGER following = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 2, SQL_C_SLONG, &following, sizeof(following), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(12345, following);

    SQLCloseCursor(stmt_);
}

// The base type belongs to the value that was probed, so probing one variant
// column must not answer for another.
TEST_F(ColAttributeLiveTest, VariantTypeIsPerColumn) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("SELECT CAST(42 AS SQL_VARIANT) AS a,"
               " CAST(CAST('x' AS VARCHAR(5)) AS SQL_VARIANT) AS b");

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLCHAR probe = 0;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_C_SLONG, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

    // Column 2 has not been probed, so it has no type to report yet -- it must
    // not inherit column 1's.
    SQLLEN value = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 2, SQL_CA_SS_VARIANT_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");

    SQLCloseCursor(stmt_);
}

// Without the probe there is no value to report a type for; msodbcsql relies on
// the same ordering, so this only pins our diagnostic.
TEST_F(ColAttributeLiveTest, VariantTypeBeforeProbeIsSequenceError) {
    SKIP_IF_COMPARING_MSODBCSQL();
    ExecDirect("SELECT CAST(42 AS SQL_VARIANT) AS v");
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLLEN value = 0;
    SQLRETURN rc =
        SQLColAttribute(stmt_, 1, SQL_CA_SS_VARIANT_TYPE, nullptr, 0, nullptr, &value);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY010");
    SQLCloseCursor(stmt_);
}

// The exact numerics report SQL_C_NUMERIC, which is what tells a caller the
// value is a decimal rather than a string. mssql-python routes on this answer
// alone -- SQL_C_CHAR made it hand back `str` instead of `decimal.Decimal`
// (AB#47702). `money` and `smallmoney` are included because msodbcsql answers
// SQL_C_NUMERIC for them too, matching the SQL_DECIMAL it reports for a money
// column, and because they arrive as distinct TDS base types (MONEYN width 8
// and 4) that the driver maps separately.
//
// This compares against msodbcsql: the value is the whole point of the test.
TEST_F(ColAttributeLiveTest, VariantExactNumericsReportNumeric) {
    struct Case {
        const char* label;
        const char* expr;
    };
    const Case cases[] = {
        {"decimal", "CAST(999.99 AS DECIMAL(18, 4))"},
        {"numeric", "CAST(888.88 AS NUMERIC(10, 2))"},
        {"money", "CAST(12.34 AS MONEY)"},
        {"smallmoney", "CAST(12.34 AS SMALLMONEY)"},
        // No CAST: SQL Server stores a bare decimal literal as `numeric`.
        {"implicit numeric", "45.67"},
    };

    for (const Case& c : cases) {
        SCOPED_TRACE(c.label);
        ExecDirect(std::string("SELECT CAST(") + c.expr + " AS SQL_VARIANT) AS v");
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLCHAR probe = 0;
        SQLLEN indicator = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_NE(SQL_NULL_DATA, indicator);
        EXPECT_EQ(SQL_C_NUMERIC, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

        SQLCloseCursor(stmt_);
    }
}
// Reporting SQL_C_NUMERIC describes the value, not the delivery path: the
// character fetch that mssql-python actually performs after reading the
// attribute has to keep working, digits intact. Compared against msodbcsql,
// which renders the same padded form.
TEST_F(ColAttributeLiveTest, VariantDecimalStillDeliversAsCharacter) {
    ExecDirect("SELECT CAST(CAST(999.99 AS DECIMAL(18, 4)) AS SQL_VARIANT) AS v");
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR probe = 0;
    SQLLEN indicator = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_C_NUMERIC, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

    SQLCHAR text[64] = {0};
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, text, sizeof(text), &indicator),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_STREQ("999.9900", reinterpret_cast<const char*>(text));

    SQLCloseCursor(stmt_);
}

// Every base type whose answer is measured to agree with msodbcsql, in one
// place. Spot-checking a couple of types is what let the exact-numeric answer
// (AB#47702) and the `tinyint` signedness answer both survive unnoticed.
//
// This list is hand-written, so it does not by itself stop a new base type from
// going unchecked. What does is `variant_c_type`'s match, which enumerates every
// `TdsDataType` instead of ending in a `_` arm: adding a type fails to compile
// until someone answers for it, and this table is where the answer gets pinned
// against msodbcsql.
//
// Temporal types are covered separately above because their expected values
// depend on the declared ODBC version.
TEST_F(ColAttributeLiveTest, VariantBaseTypesMatchMsodbcsql) {
    struct Case {
        const char* label;
        const char* expr;
        SQLSMALLINT expected;
    };
    const Case cases[] = {
        {"bit", "CAST(1 AS BIT)", SQL_C_BIT},
        // Unsigned: tinyint is 0-255 on the server, so a signed answer would
        // make a caller read 200 as -56.
        {"tinyint", "CAST(200 AS TINYINT)", SQL_C_UTINYINT},
        {"smallint", "CAST(300 AS SMALLINT)", SQL_C_SSHORT},
        {"int", "CAST(42 AS INT)", SQL_C_SLONG},
        {"bigint", "CAST(42 AS BIGINT)", SQL_C_SBIGINT},
        {"real", "CAST(1.5 AS REAL)", SQL_C_FLOAT},
        {"float", "CAST(1.5 AS FLOAT)", SQL_C_DOUBLE},
        {"decimal", "CAST(1.5 AS DECIMAL(18, 4))", SQL_C_NUMERIC},
        {"numeric", "CAST(1.5 AS NUMERIC(10, 2))", SQL_C_NUMERIC},
        {"money", "CAST(1.5 AS MONEY)", SQL_C_NUMERIC},
        {"smallmoney", "CAST(1.5 AS SMALLMONEY)", SQL_C_NUMERIC},
        {"char", "CAST('ab' AS CHAR(2))", SQL_C_CHAR},
        {"varchar", "CAST('ab' AS VARCHAR(10))", SQL_C_CHAR},
        {"nchar", "CAST(N'ab' AS NCHAR(2))", SQL_C_WCHAR},
        {"nvarchar", "CAST(N'ab' AS NVARCHAR(10))", SQL_C_WCHAR},
        {"binary", "CAST(0x01 AS BINARY(1))", SQL_C_BINARY},
        {"varbinary", "CAST(0x01 AS VARBINARY(10))", SQL_C_BINARY},
        {"uniqueidentifier", "CAST(NEWID() AS UNIQUEIDENTIFIER)", SQL_C_GUID},
    };

    for (const Case& c : cases) {
        SCOPED_TRACE(c.label);
        ExecDirect(std::string("SELECT CAST(") + c.expr + " AS SQL_VARIANT) AS v");
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLCHAR probe = 0;
        SQLLEN indicator = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_BINARY, &probe, 0, &indicator),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_NE(SQL_NULL_DATA, indicator);
        EXPECT_EQ(c.expected, NumericAttr(stmt_, 1, SQL_CA_SS_VARIANT_TYPE));

        SQLCloseCursor(stmt_);
    }
}

