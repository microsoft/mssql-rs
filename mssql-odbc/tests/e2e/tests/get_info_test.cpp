// Copyright (c) Microsoft Corporation. All rights reserved.
// get_info_test.cpp  –  E2E tests for SQLGetInfoW.
//
// Values are pinned against retail msodbcsql18 18.6.2.1, so this file runs
// unchanged on both legs of `run_e2e.sh --compare-with-msodbcsql`. Where
// mssql-odbc deliberately diverges for its first release the assertion is
// widened rather than skipped, and the divergence is named in a comment.
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

// An identifier this driver does not implement stays an error rather than
// silently returning a zeroed buffer.
//
// mssql-odbc-specific. Retail msodbcsql18 rejects 65000 with HY096 on Linux but
// answers SQL_SUCCESS on Windows (observed in ADO build 173877), so the parity
// leg cannot share an assertion that is about this driver's own contract.
TEST_F(GetInfoLiveTest, UnknownInfoTypeIsRejected) {
    SKIP_IF_COMPARING_MSODBCSQL();
    SQLUINTEGER value = 0;
    SQLSMALLINT len = -1;
    SQLRETURN rc = SQLGetInfo(dbc_, 65000, &value, sizeof(value), &len);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY096");
}
