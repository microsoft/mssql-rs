// Copyright (c) Microsoft Corporation. All rights reserved.
// get_info_test.cpp  –  E2E tests for SQLGetInfoW.
//
// Values are pinned against retail msodbcsql18 18.6.2.1, so parity assertions
// run unchanged on both legs of `run_e2e.sh --compare-with-msodbcsql`.
// Capability assertions that describe only this driver are skipped on the
// comparison leg and recorded in docs/sql-get-info-plan.md.
//
// This suite builds wide on Windows and narrow on Unix (see ODBC_E2E_FORCE_UNICODE
// in CMakeLists.txt), so lengths are asserted in SQLTCHAR units rather than
// hard-coded to UTF-16. On the narrow build the driver manager re-encodes the
// driver's UTF-16 output and determines what a null-pointer size probe reports.
// The truncation test calls SQLGetInfoW explicitly to check its byte-length and
// buffer-boundary contract without the driver manager's ANSI conversion.

#include "odbc_test_fixture.h"

#include <string>

namespace {

// Reads a wide-string info type and returns the decoded value. |rc| and the
// reported byte length are reported back so callers can assert on them.
std::string GetInfoString(SQLHDBC dbc, SQLUSMALLINT infoType, SQLRETURN* rc,
                          SQLSMALLINT* byteLen) {
    SQLTCHAR buf[4096] = {};
    SQLSMALLINT len = -1;
    *rc = SQLGetInfo(dbc, infoType, buf, static_cast<SQLSMALLINT>(sizeof(buf)), &len);
    if (byteLen) *byteLen = len;
    if (!SQL_SUCCEEDED(*rc)) return {};
    return ODBCTestUtils::ToNarrow(SqlTString(buf));
}

SQLUSMALLINT GetInfoU16(SQLHDBC dbc, SQLUSMALLINT infoType, SQLRETURN* rc,
                        SQLSMALLINT* byteLen) {
    SQLUSMALLINT value = 0xAAAA;
    SQLSMALLINT len = -1;
    *rc = SQLGetInfo(dbc, infoType, &value, sizeof(value), &len);
    if (byteLen) *byteLen = len;
    return value;
}

SQLUINTEGER GetInfoU32(SQLHDBC dbc, SQLUSMALLINT infoType, SQLRETURN* rc,
                       SQLSMALLINT* byteLen) {
    SQLUINTEGER value = 0xAAAAAAAA;
    SQLSMALLINT len = -1;
    *rc = SQLGetInfo(dbc, infoType, &value, sizeof(value), &len);
    if (byteLen) *byteLen = len;
    return value;
}

// A connection string with `extra` appended. Reuses the shared builder so the
// credential handling stays in one place. `BuildConnectionString()` returns
// `ODBC_TEST_CONNSTR` verbatim when that env var is set, which is not
// guaranteed to end in `;`, so normalize the separator here rather than
// assuming one.
SqlTString ConnStrWith(const std::string& extra) {
    std::string base = ODBCTestUtils::ToNarrow(ODBCTestUtils::BuildConnectionString());
    if (!base.empty() && base.back() != ';') base += ';';
    return ODBCTestUtils::ToSqlTStr(base + extra);
}

}  // namespace

class GetInfoLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            FAIL() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

// ===================================================================
// Identity strings
// ===================================================================

// SQL_SERVER_NAME is the name the server reports for itself, taken from the
// ServerName field of the INFO tokens sent at login.
//
// Deliberately NOT compared against @@SERVERNAME. The two differ on a host that
// was renamed after SQL Server was installed, because @@SERVERNAME keeps the
// name recorded at setup until sp_dropserver/sp_addserver is run while the
// token carries the running instance's actual name. The Windows CI agent is
// exactly that case -- SQL_SERVER_NAME reported "4fdcda98c000000" against
// @@SERVERNAME "hvyuptkn" -- and retail msodbcsql18 reported the same pair, so
// equality is not a property of either driver. Cross-driver agreement on the
// value is covered by the --compare-with-msodbcsql parity run.
TEST_F(GetInfoLiveTest, ServerNameIsReportedAndStable) {
    SQLRETURN rc = SQL_ERROR;
    SQLSMALLINT len = -1;
    std::string serverName = GetInfoString(dbc_, SQL_SERVER_NAME, &rc, &len);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_FALSE(serverName.empty()) << "SQL_SERVER_NAME must not be empty";
    EXPECT_EQ(static_cast<SQLSMALLINT>(serverName.size() * sizeof(SQLTCHAR)), len);

    // Cached connection state, so repeated reads must not drift.
    SQLSMALLINT len2 = -1;
    std::string again = GetInfoString(dbc_, SQL_SERVER_NAME, &rc, &len2);
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(serverName, again);
    EXPECT_EQ(len, len2);
}

// SQL_USER_NAME must be answerable; the value differs by design (see below).
TEST_F(GetInfoLiveTest, UserNameIsReported) {
    SQLRETURN rc = SQL_ERROR;
    SQLSMALLINT len = -1;
    std::string userName = GetInfoString(dbc_, SQL_USER_NAME, &rc, &len);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(static_cast<SQLSMALLINT>(userName.size() * sizeof(SQLTCHAR)), len);

    // DIVERGENCE: msodbcsql18 reports USER_NAME() (the database user, e.g.
    // "dbo"). It fetches that lazily on the first SQLGetInfo(SQL_USER_NAME)
    // after login, riding along on the batch that refreshes its alias-type
    // cache (`sqlcstr.cpp` g_szSqlUdtQuery, driven by CONN_ST_REFRESH_UDT).
    // mssql-odbc has no such cache and no facility for issuing an internal
    // query mid-session, so it reports the login instead. Both are non-empty
    // for SQL authentication; integrated and token authentication legitimately
    // yield an empty string.
    //
    // ODBC_TEST_CONNSTR ignores ODBC_TEST_UID, so a stale UID in the environment
    // says nothing about how the connection actually authenticated.
    const ODBCTestConfig& cfg = ODBCTestConfig::Instance();
    if (!cfg.HasConnStr() && cfg.HasCredentials()) {
        EXPECT_FALSE(userName.empty());
    }
}

// SQL_DATA_SOURCE_NAME is empty for a DSN-less connection and the DSN otherwise.
TEST_F(GetInfoLiveTest, DataSourceNameMatchesConnectionMethod) {
    SQLRETURN rc = SQL_ERROR;
    std::string dsn = GetInfoString(dbc_, SQL_DATA_SOURCE_NAME, &rc, nullptr);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));

    const ODBCTestConfig& cfg = ODBCTestConfig::Instance();
    // A full override ignores ODBC_TEST_DSN and may name a DSN of its own, so
    // neither branch below describes the connection that was actually made.
    if (cfg.HasConnStr()) {
        GTEST_SKIP() << "ODBC_TEST_CONNSTR overrides ODBC_TEST_DSN";
    }
    if (cfg.HasDSN()) {
        EXPECT_EQ(cfg.DSN(), dsn);
    } else {
        EXPECT_TRUE(dsn.empty()) << "DSN-less connection reported '" << dsn << "'";
    }
}

// ===================================================================
// Fixed values shared with msodbcsql18
// ===================================================================

TEST_F(GetInfoLiveTest, CatalogAndSchemaTerms) {
    SQLRETURN rc = SQL_ERROR;
    EXPECT_EQ("database", GetInfoString(dbc_, SQL_CATALOG_TERM, &rc, nullptr));
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(".", GetInfoString(dbc_, SQL_CATALOG_NAME_SEPARATOR, &rc, nullptr));
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ("owner", GetInfoString(dbc_, SQL_SCHEMA_TERM, &rc, nullptr));
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
}

TEST_F(GetInfoLiveTest, YesNoCapabilities) {
    struct Case { SQLUSMALLINT infoType; const char* expected; const char* name; };
    const Case cases[] = {
        {SQL_ACCESSIBLE_TABLES,      "Y", "SQL_ACCESSIBLE_TABLES"},
        {SQL_ACCESSIBLE_PROCEDURES,  "Y", "SQL_ACCESSIBLE_PROCEDURES"},
        {SQL_PROCEDURES,             "Y", "SQL_PROCEDURES"},
        {SQL_EXPRESSIONS_IN_ORDERBY, "Y", "SQL_EXPRESSIONS_IN_ORDERBY"},
        {SQL_DATA_SOURCE_READ_ONLY,  "N", "SQL_DATA_SOURCE_READ_ONLY"},
    };
    for (const Case& c : cases) {
        SQLRETURN rc = SQL_ERROR;
        SQLSMALLINT len = -1;
        std::string value = GetInfoString(dbc_, c.infoType, &rc, &len);
        EXPECT_TRUE(SQL_SUCCEEDED(rc)) << c.name;
        EXPECT_EQ(c.expected, value) << c.name;
        EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLTCHAR)), len) << c.name;
    }
}

// Non-row arrays return an aggregate count. Retail msodbcsql 18.6.2.1
// advertises SQL_PARC_BATCH instead, so this assertion remains driver-specific.
TEST_F(GetInfoLiveTest, ParamArrayCapabilities) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SQLRETURN rc = SQL_ERROR;
    SQLSMALLINT len = -1;

    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_PARC_NO_BATCH),
              GetInfoU32(dbc_, SQL_PARAM_ARRAY_ROW_COUNTS, &rc, &len));
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUINTEGER)), len);
}

TEST_F(GetInfoLiveTest, ParamArraySelectsAreNavigable) {
    SQLRETURN rc = SQL_ERROR;
    SQLSMALLINT len = -1;
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_PAS_BATCH),
              GetInfoU32(dbc_, SQL_PARAM_ARRAY_SELECTS, &rc, &len));
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUINTEGER)), len);
}

TEST_F(GetInfoLiveTest, IdentifierLimitsAreSysnameWidth) {
    for (SQLUSMALLINT infoType : {SQL_MAX_COLUMN_NAME_LEN, SQL_MAX_SCHEMA_NAME_LEN,
                                  SQL_MAX_TABLE_NAME_LEN}) {
        SQLRETURN rc = SQL_ERROR;
        SQLSMALLINT len = -1;
        SQLUSMALLINT value = GetInfoU16(dbc_, infoType, &rc, &len);
        EXPECT_TRUE(SQL_SUCCEEDED(rc)) << "info_type " << infoType;
        EXPECT_EQ(128, value) << "info_type " << infoType;
        EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUSMALLINT)), len)
            << "info_type " << infoType;
    }
}

TEST_F(GetInfoLiveTest, MaxStatementLenAndConformance) {
    SQLRETURN rc = SQL_ERROR;
    SQLSMALLINT len = -1;
    SQLUINTEGER packetSize = 0;

    ASSERT_SQL_OK(SQLGetConnectAttr(dbc_, SQL_ATTR_PACKET_SIZE, &packetSize,
                                    SQL_IS_UINTEGER, nullptr),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(128u * packetSize, GetInfoU32(dbc_, SQL_MAX_STATEMENT_LEN, &rc, &len));
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUINTEGER)), len);

    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_SC_SQL92_ENTRY),
              GetInfoU32(dbc_, SQL_SQL_CONFORMANCE, &rc, &len));
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUINTEGER)), len);
}

// The other parity checks in this file re-derive their expectation from
// SQLGetConnectAttr(SQL_ATTR_PACKET_SIZE), which reads the exact same stored
// value SQLGetInfo does here — so on this driver `128*x == 128*x` passes
// regardless of what x actually is, and the shared default connection string
// never sets `PacketSize=`, so none of them exercise that keyword. Open a
// second connection with an explicit, non-default `PacketSize=` and assert
// the literal expected number, so a regression in either the connection's
// resolved packet size or SQLGetInfo's derivation would be caught.
//
// mssql-odbc-specific: skipped on the msodbcsql comparison leg. Measured
// against retail msodbcsql18 18.6.2.1 over an encrypted connection, it
// reports a *smaller* value than requested here (16192, not 16384) —
// contradicting the "never the negotiated value" claim in
// docs/sql-get-info-plan.md, which was derived from static source reading
// and evidently misses a TLS-driven reduction path. mssql-odbc's own design
// (always the requested/configured size, proven independent of negotiation
// by the `Encrypt=no` mock-server unit test in driver_connect.rs) has no
// such reduction, so the literal expectation only holds for this driver.
// See the divergence table entry for `SQL_MAX_STATEMENT_LEN` et al.
TEST_F(GetInfoLiveTest, MaxLengthsUseTheConnectionStringPacketSize) {
    SKIP_IF_COMPARING_MSODBCSQL();
    constexpr SQLUINTEGER kRequestedPacketSize = 16384;

    SQLHDBC dbc = SQL_NULL_HDBC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc), SQL_HANDLE_ENV, env_);
    SqlTString connstr = ConnStrWith("PacketSize=" + std::to_string(kRequestedPacketSize));
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;
    ASSERT_SQL_OK(SQLDriverConnect(dbc, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                                   static_cast<SQLSMALLINT>(connstr.size()), outStr,
                                   static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLTCHAR)),
                                   &outLen, SQL_DRIVER_NOPROMPT),
                  SQL_HANDLE_DBC, dbc);

    SQLUINTEGER packetSize = 0;
    ASSERT_SQL_OK(SQLGetConnectAttr(dbc, SQL_ATTR_PACKET_SIZE, &packetSize, SQL_IS_UINTEGER,
                                    nullptr),
                  SQL_HANDLE_DBC, dbc);
    EXPECT_EQ(kRequestedPacketSize, packetSize)
        << "SQLGetConnectAttr must report the connection string's PacketSize=, not a "
           "negotiated or default value";

    SQLRETURN rc = SQL_ERROR;
    SQLSMALLINT len = -1;
    for (SQLUSMALLINT infoType :
         {SQL_MAX_STATEMENT_LEN, SQL_MAX_CHAR_LITERAL_LEN, SQL_MAX_BINARY_LITERAL_LEN}) {
        EXPECT_EQ(128u * kRequestedPacketSize, GetInfoU32(dbc, infoType, &rc, &len))
            << "info_type " << infoType;
        EXPECT_TRUE(SQL_SUCCEEDED(rc)) << "info_type " << infoType;
        EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUINTEGER)), len) << "info_type " << infoType;
    }

    SQLDisconnect(dbc);
    SQLFreeHandle(SQL_HANDLE_DBC, dbc);
}

TEST_F(GetInfoLiveTest, KeywordsAndSpecialCharacters) {
    SQLRETURN rc = SQL_ERROR;
    SQLSMALLINT len = -1;

    std::string keywords = GetInfoString(dbc_, SQL_KEYWORDS, &rc, &len);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_EQ(static_cast<SQLSMALLINT>(keywords.size() * sizeof(SQLTCHAR)), len);
    // A bare comma-separated list; applications split on ',' verbatim.
    EXPECT_EQ(std::string::npos, keywords.find(", "));
    EXPECT_NE(std::string::npos, keywords.find("NONCLUSTERED"));
    EXPECT_NE(std::string::npos, keywords.find("IDENTITY_INSERT"));

    std::string special = GetInfoString(dbc_, SQL_SPECIAL_CHARACTERS, &rc, &len);
    ASSERT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_NE(std::string::npos, special.find('#'));
    EXPECT_NE(std::string::npos, special.find('$'));
    // '@' is only legal as the first character of a variable name.
    EXPECT_EQ(std::string::npos, special.find('@'));
}

TEST_F(GetInfoLiveTest, WorkItem48149StringValuesMatchMsodbcsql) {
    struct Case { SQLUSMALLINT infoType; const char* expected; const char* name; };
    const Case cases[] = {
        {SQL_SEARCH_PATTERN_ESCAPE, "\\", "SQL_SEARCH_PATTERN_ESCAPE"},
        {SQL_DESCRIBE_PARAMETER, "Y", "SQL_DESCRIBE_PARAMETER"},
        {SQL_MULT_RESULT_SETS, "Y", "SQL_MULT_RESULT_SETS"},
        {SQL_PROCEDURE_TERM, "stored procedure", "SQL_PROCEDURE_TERM"},
        {SQL_TABLE_TERM, "table", "SQL_TABLE_TERM"},
        {SQL_CATALOG_NAME, "Y", "SQL_CATALOG_NAME"},
        {SQL_COLUMN_ALIAS, "Y", "SQL_COLUMN_ALIAS"},
        {SQL_LIKE_ESCAPE_CLAUSE, "Y", "SQL_LIKE_ESCAPE_CLAUSE"},
        {SQL_ORDER_BY_COLUMNS_IN_SELECT, "N", "SQL_ORDER_BY_COLUMNS_IN_SELECT"},
        {SQL_OUTER_JOINS, "F", "SQL_OUTER_JOINS"},
        {SQL_XOPEN_CLI_YEAR, "1995", "SQL_XOPEN_CLI_YEAR"},
    };

    for (const Case& c : cases) {
        SQLRETURN rc = SQL_ERROR;
        SQLSMALLINT len = -1;
        EXPECT_EQ(c.expected, GetInfoString(dbc_, c.infoType, &rc, &len)) << c.name;
        EXPECT_EQ(SQL_SUCCESS, rc) << c.name;
        EXPECT_EQ(static_cast<SQLSMALLINT>(strlen(c.expected) * sizeof(SQLTCHAR)), len)
            << c.name;
    }
}

TEST_F(GetInfoLiveTest, WorkItem48149U16ValuesMatchMsodbcsql) {
    struct Case { SQLUSMALLINT infoType; SQLUSMALLINT expected; const char* name; };
    const Case cases[] = {
        {SQL_CONCAT_NULL_BEHAVIOR, SQL_CB_NULL, "SQL_CONCAT_NULL_BEHAVIOR"},
        {SQL_NULL_COLLATION, SQL_NC_LOW, "SQL_NULL_COLLATION"},
        {SQL_CORRELATION_NAME, SQL_CN_ANY, "SQL_CORRELATION_NAME"},
        {SQL_GROUP_BY, SQL_GB_GROUP_BY_CONTAINS_SELECT, "SQL_GROUP_BY"},
        {SQL_IDENTIFIER_CASE, SQL_IC_MIXED, "SQL_IDENTIFIER_CASE"},
        {SQL_QUOTED_IDENTIFIER_CASE, SQL_IC_MIXED, "SQL_QUOTED_IDENTIFIER_CASE"},
        {SQL_MAX_CATALOG_NAME_LEN, 128, "SQL_MAX_CATALOG_NAME_LEN"},
        {SQL_MAX_COLUMNS_IN_GROUP_BY, 0, "SQL_MAX_COLUMNS_IN_GROUP_BY"},
        {SQL_MAX_COLUMNS_IN_INDEX, 16, "SQL_MAX_COLUMNS_IN_INDEX"},
        {SQL_MAX_COLUMNS_IN_ORDER_BY, 0, "SQL_MAX_COLUMNS_IN_ORDER_BY"},
        {SQL_MAX_COLUMNS_IN_SELECT, 4096, "SQL_MAX_COLUMNS_IN_SELECT"},
        {SQL_MAX_COLUMNS_IN_TABLE, 1024, "SQL_MAX_COLUMNS_IN_TABLE"},
        {SQL_MAX_IDENTIFIER_LEN, 128, "SQL_MAX_IDENTIFIER_LEN"},
        {SQL_MAX_TABLES_IN_SELECT, 32, "SQL_MAX_TABLES_IN_SELECT"},
        {SQL_MAX_USER_NAME_LEN, 128, "SQL_MAX_USER_NAME_LEN"},
    };

    for (const Case& c : cases) {
        SQLRETURN rc = SQL_ERROR;
        SQLSMALLINT len = -1;
        EXPECT_EQ(c.expected, GetInfoU16(dbc_, c.infoType, &rc, &len)) << c.name;
        EXPECT_EQ(SQL_SUCCESS, rc) << c.name;
        EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUSMALLINT)), len) << c.name;
    }
}

TEST_F(GetInfoLiveTest, WorkItem48149SqlCapabilitiesMatchMsodbcsql) {
    SQLUINTEGER packetSize = 0;
    ASSERT_SQL_OK(SQLGetConnectAttr(dbc_, SQL_ATTR_PACKET_SIZE, &packetSize,
                                    SQL_IS_UINTEGER, nullptr),
                  SQL_HANDLE_DBC, dbc_);

    struct Case { SQLUSMALLINT infoType; SQLUINTEGER expected; const char* name; };
    const Case cases[] = {
        {SQL_BATCH_ROW_COUNT, SQL_BRC_EXPLICIT, "SQL_BATCH_ROW_COUNT"},
        {SQL_BATCH_SUPPORT,
         SQL_BS_SELECT_EXPLICIT | SQL_BS_ROW_COUNT_EXPLICIT | SQL_BS_SELECT_PROC |
             SQL_BS_ROW_COUNT_PROC,
         "SQL_BATCH_SUPPORT"},
        {SQL_ALTER_TABLE, 0x00009869, "SQL_ALTER_TABLE"},
        {SQL_CATALOG_USAGE,
         SQL_CU_DML_STATEMENTS | SQL_CU_PROCEDURE_INVOCATION | SQL_CU_TABLE_DEFINITION,
         "SQL_CATALOG_USAGE"},
        {SQL_QUALIFIER_USAGE,
         SQL_QU_DML_STATEMENTS | SQL_QU_PROCEDURE_INVOCATION | SQL_QU_TABLE_DEFINITION,
         "SQL_QUALIFIER_USAGE"},
        {SQL_CREATE_ASSERTION, 0, "SQL_CREATE_ASSERTION"},
        {SQL_DDL_INDEX, SQL_DI_CREATE_INDEX | SQL_DI_DROP_INDEX, "SQL_DDL_INDEX"},
        {SQL_OJ_CAPABILITIES,
         SQL_OJ_LEFT | SQL_OJ_RIGHT | SQL_OJ_FULL | SQL_OJ_NESTED | SQL_OJ_NOT_ORDERED |
             SQL_OJ_INNER | SQL_OJ_ALL_COMPARISON_OPS,
         "SQL_OJ_CAPABILITIES"},
        {SQL_SCHEMA_USAGE,
         SQL_SU_DML_STATEMENTS | SQL_SU_PROCEDURE_INVOCATION | SQL_SU_TABLE_DEFINITION |
             SQL_SU_INDEX_DEFINITION | SQL_SU_PRIVILEGE_DEFINITION,
         "SQL_SCHEMA_USAGE"},
        {SQL_OWNER_USAGE,
         SQL_OU_DML_STATEMENTS | SQL_OU_PROCEDURE_INVOCATION | SQL_OU_TABLE_DEFINITION |
             SQL_OU_INDEX_DEFINITION | SQL_OU_PRIVILEGE_DEFINITION,
         "SQL_OWNER_USAGE"},
        {SQL_SUBQUERIES,
         SQL_SQ_COMPARISON | SQL_SQ_EXISTS | SQL_SQ_IN | SQL_SQ_QUANTIFIED |
             SQL_SQ_CORRELATED_SUBQUERIES,
         "SQL_SUBQUERIES"},
        {SQL_UNION, SQL_U_UNION | SQL_U_UNION_ALL, "SQL_UNION"},
        {SQL_MAX_BINARY_LITERAL_LEN, 128u * packetSize, "SQL_MAX_BINARY_LITERAL_LEN"},
        {SQL_MAX_CHAR_LITERAL_LEN, 128u * packetSize, "SQL_MAX_CHAR_LITERAL_LEN"},
        {SQL_MAX_ROW_SIZE, 8060, "SQL_MAX_ROW_SIZE"},
    };

    for (const Case& c : cases) {
        SQLRETURN rc = SQL_ERROR;
        SQLSMALLINT len = -1;
        EXPECT_EQ(c.expected, GetInfoU32(dbc_, c.infoType, &rc, &len)) << c.name;
        EXPECT_EQ(SQL_SUCCESS, rc) << c.name;
        EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUINTEGER)), len) << c.name;
    }
}

TEST_F(GetInfoLiveTest, WorkItem48149CapabilitiesDescribeThisDriver) {
    SKIP_IF_COMPARING_MSODBCSQL();

    struct Case { SQLUSMALLINT infoType; SQLUINTEGER expected; const char* name; };
    const Case cases[] = {
        {SQL_ASYNC_MODE, SQL_AM_NONE, "SQL_ASYNC_MODE"},
        {SQL_DYNAMIC_CURSOR_ATTRIBUTES1, 0, "SQL_DYNAMIC_CURSOR_ATTRIBUTES1"},
        {SQL_DYNAMIC_CURSOR_ATTRIBUTES2, 0, "SQL_DYNAMIC_CURSOR_ATTRIBUTES2"},
        {SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES1, SQL_CA1_NEXT,
         "SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES1"},
        {SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2,
         SQL_CA2_READ_ONLY_CONCURRENCY | SQL_CA2_MAX_ROWS_SELECT,
         "SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2"},
        {SQL_KEYSET_CURSOR_ATTRIBUTES1, 0, "SQL_KEYSET_CURSOR_ATTRIBUTES1"},
        {SQL_KEYSET_CURSOR_ATTRIBUTES2, 0, "SQL_KEYSET_CURSOR_ATTRIBUTES2"},
        {SQL_STATIC_CURSOR_ATTRIBUTES1, 0, "SQL_STATIC_CURSOR_ATTRIBUTES1"},
        {SQL_STATIC_CURSOR_ATTRIBUTES2, 0, "SQL_STATIC_CURSOR_ATTRIBUTES2"},
        {SQL_BOOKMARK_PERSISTENCE, 0, "SQL_BOOKMARK_PERSISTENCE"},
        {SQL_CURSOR_SENSITIVITY, SQL_UNSPECIFIED, "SQL_CURSOR_SENSITIVITY"},
        {SQL_SCROLL_OPTIONS, SQL_SO_FORWARD_ONLY, "SQL_SCROLL_OPTIONS"},
        // AB#46384: {fn CONVERT}/{fn CAST} and {fn TIMESTAMPADD}/{fn
        // TIMESTAMPDIFF} are translated and forwarded to the server.
        {SQL_CONVERT_FUNCTIONS, 0x00000003u, "SQL_CONVERT_FUNCTIONS"},
        {SQL_TIMEDATE_ADD_INTERVALS, 0x000001FFu, "SQL_TIMEDATE_ADD_INTERVALS"},
        {SQL_TIMEDATE_DIFF_INTERVALS, 0x000001FFu, "SQL_TIMEDATE_DIFF_INTERVALS"},
        {SQL_FETCH_DIRECTION, SQL_FD_FETCH_NEXT, "SQL_FETCH_DIRECTION"},
        {SQL_POSITIONED_STATEMENTS, 0, "SQL_POSITIONED_STATEMENTS"},
        {SQL_SCROLL_CONCURRENCY, SQL_SCCO_READ_ONLY, "SQL_SCROLL_CONCURRENCY"},
        {SQL_STATIC_SENSITIVITY, 0, "SQL_STATIC_SENSITIVITY"},
    };

    for (const Case& c : cases) {
        SQLRETURN rc = SQL_ERROR;
        SQLSMALLINT len = -1;
        EXPECT_EQ(c.expected, GetInfoU32(dbc_, c.infoType, &rc, &len)) << c.name;
        EXPECT_EQ(SQL_SUCCESS, rc) << c.name;
        EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUINTEGER)), len) << c.name;
    }
}

TEST_F(GetInfoLiveTest, DatabaseNameMatchesCurrentCatalog) {
    SQLRETURN rc = SQL_ERROR;
    std::string database = GetInfoString(dbc_, SQL_DATABASE_NAME, &rc, nullptr);
    ASSERT_EQ(SQL_SUCCESS, rc);
    EXPECT_FALSE(database.empty());

    SQLTCHAR currentCatalog[256] = {};
    SQLINTEGER len = -1;
    ASSERT_SQL_OK(SQLGetConnectAttr(dbc_, SQL_ATTR_CURRENT_CATALOG, currentCatalog,
                                    sizeof(currentCatalog), &len),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(ODBCTestUtils::ToNarrow(SqlTString(currentCatalog)), database);
}

TEST_F(GetInfoLiveTest, DatabaseNameTracksCurrentCatalogChange) {
    SQLRETURN rc = SQL_ERROR;
    std::string original = GetInfoString(dbc_, SQL_DATABASE_NAME, &rc, nullptr);
    ASSERT_EQ(SQL_SUCCESS, rc);
    const std::string target = original == "master" ? "tempdb" : "master";
    SqlTString catalog = ODBCTestUtils::ToSqlTStr(target);

    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_CURRENT_CATALOG,
                                    const_cast<SQLTCHAR*>(catalog.c_str()), SQL_NTS),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(target, GetInfoString(dbc_, SQL_DATABASE_NAME, &rc, nullptr));
    EXPECT_EQ(SQL_SUCCESS, rc);
}

// ===================================================================
// ODBC buffer contract
// ===================================================================

// A null InfoValuePtr is a size probe: it must succeed and report a length
// without writing anything. The unit tests assert the exact byte count; on the
// narrow build the driver manager reports the driver's UTF-16 length unhalved,
// so only the shape is checked here.
TEST_F(GetInfoLiveTest, NullBufferReportsRequiredLength) {
    SQLSMALLINT probe = -1;
    SQLRETURN rc = SQLGetInfo(dbc_, SQL_KEYWORDS, nullptr, 0, &probe);
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    EXPECT_GT(probe, 0);
}

// A short buffer must be reported as a truncation, not a silent short read.
TEST_F(GetInfoLiveTest, ShortBufferTruncatesWith01004) {
    // unixODBC 2.3.11/2.3.12 SQLGetInfoInternal reuses its expanded wide-buffer
    // length for unicode_to_ansi_copy, overrunning a short ANSI output buffer.
    // Use the wide entry point on every platform to test the driver's contract.
    SQLSMALLINT fullLen = -1;
    ASSERT_EQ(SQL_SUCCESS, SQLGetInfoW(dbc_, SQL_KEYWORDS, nullptr, 0, &fullLen));

    // Not named `small`: the Windows SDK's rpcndr.h defines that as a macro for `char`.
    struct {
        SQLWCHAR value[4];
        SQLWCHAR guard;
    } tiny = {{0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF}, 0xFFFF};
    SQLSMALLINT len = -1;
    SQLRETURN rc = SQLGetInfoW(dbc_, SQL_KEYWORDS, tiny.value, sizeof(tiny.value), &len);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_TRUE(ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, dbc_, "01004"));
    EXPECT_EQ(fullLen, len);
    EXPECT_GT(len, static_cast<SQLSMALLINT>(sizeof(tiny.value)));
    EXPECT_EQ(static_cast<SQLWCHAR>('B'), tiny.value[0]);
    EXPECT_EQ(static_cast<SQLWCHAR>('A'), tiny.value[1]);
    EXPECT_EQ(static_cast<SQLWCHAR>('C'), tiny.value[2]);
    EXPECT_EQ(0, tiny.value[3]);
    EXPECT_EQ(0xFFFF, tiny.guard);
}

// ODBC defines BufferLength only for character information. The classic
// driver's Raidpp TCSQLGetInfo::Variation_3 regressed by applying it to numeric
// results, so exercise each numeric representation used by AB#48149.
TEST_F(GetInfoLiveTest, NumericInfoIgnoresBufferLength) {
    SQLUSMALLINT smallValue = 0xAAAA;
    SQLSMALLINT len = -1;
    SQLRETURN rc = SQLGetInfo(dbc_, SQL_CONCAT_NULL_BEHAVIOR, &smallValue, 0, &len);
    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(SQL_CB_NULL, smallValue);
    EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUSMALLINT)), len);

    struct Case { SQLUSMALLINT infoType; SQLUINTEGER expected; };
    const Case cases[] = {
        {SQL_MAX_ROW_SIZE, 8060},
        {SQL_UNION, SQL_U_UNION | SQL_U_UNION_ALL},
    };

    for (const Case& c : cases) {
        SQLUINTEGER value = 0xAAAAAAAA;
        len = -1;
        rc = SQLGetInfo(dbc_, c.infoType, &value, 0, &len);
        EXPECT_EQ(SQL_SUCCESS, rc) << c.infoType;
        EXPECT_EQ(c.expected, value) << c.infoType;
        EXPECT_EQ(static_cast<SQLSMALLINT>(sizeof(SQLUINTEGER)), len) << c.infoType;
    }
}

// Classic Raidpp TCSQLGetInfo::Variation_1: a negative BufferLength for a
// character result is an invalid buffer length, not a size probe.
TEST_F(GetInfoLiveTest, NegativeStringBufferLengthReturnsHy090) {
    SQLWCHAR value[16] = {};
    SQLSMALLINT len = -1;
    SQLRETURN rc = SQLGetInfoW(dbc_, SQL_DRIVER_ODBC_VER, value, -10, &len);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY090");
}

TEST_F(GetInfoLiveTest, SuccessfulCallClearsPreviousDiagnostic) {
    SQLWCHAR value[16] = {};
    SQLSMALLINT len = -1;
    ASSERT_EQ(SQL_ERROR, SQLGetInfoW(dbc_, SQL_DRIVER_ODBC_VER, value, -10, &len));
    ASSERT_TRUE(ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, dbc_, "HY090"));

    ASSERT_EQ(SQL_SUCCESS,
              SQLGetInfoW(dbc_, SQL_DRIVER_ODBC_VER, value, sizeof(value), &len));

    SQLWCHAR state[6] = {};
    SQLINTEGER native = 0;
    SQLWCHAR message[256] = {};
    SQLSMALLINT messageLen = 0;
    EXPECT_EQ(SQL_NO_DATA, SQLGetDiagRecW(SQL_HANDLE_DBC, dbc_, 1, state, &native,
                                          message, 256, &messageLen));
}

// SQLGetInfo must work while a cursor is open on a non-MARS connection, and
// must leave that cursor usable. mssql-odbc satisfies this by answering from
// state captured at login; msodbcsql spawns a second connection for its own
// lazy lookup rather than disturbing the busy one (sqlccmd.cpp, bug #656241).
TEST_F(GetInfoLiveTest, WorksWithAnOpenCursorAndLeavesItUsable) {
    ExecDirect("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3");
    ASSERT_EQ(SQL_SUCCESS, SQLFetch(stmt_));

    SQLRETURN rc = SQL_ERROR;
    EXPECT_FALSE(GetInfoString(dbc_, SQL_SERVER_NAME, &rc, nullptr).empty());
    EXPECT_TRUE(SQL_SUCCEEDED(rc));
    GetInfoString(dbc_, SQL_USER_NAME, &rc, nullptr);
    EXPECT_TRUE(SQL_SUCCEEDED(rc));

    EXPECT_EQ(SQL_SUCCESS, SQLFetch(stmt_)) << "cursor must survive SQLGetInfo";
    SQLCloseCursor(stmt_);
}

// The Windows Driver Manager answers a reserved information type itself with
// SQL_SUCCESS for both drivers. Unix forwards it, and both drivers return
// HY096. The Rust unit test bypasses the manager and pins the driver response.
TEST_F(GetInfoLiveTest, ReservedInfoTypeFollowsDriverManagerContract) {
    SQLUINTEGER value = 0;
    SQLSMALLINT len = -1;
    SQLRETURN rc = SQLGetInfo(dbc_, 65000, &value, sizeof(value), &len);
#ifdef _WIN32
    EXPECT_EQ(SQL_SUCCESS, rc);
#else
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY096");
#endif
}
