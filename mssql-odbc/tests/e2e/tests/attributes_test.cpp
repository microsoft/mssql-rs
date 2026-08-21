// Copyright (c) Microsoft Corporation. All rights reserved.
// attributes_test.cpp  -  Tests for SQL_ATTR_QUERY_TIMEOUT and
//                         SQL_ATTR_CURRENT_CATALOG.
//
// Both attributes are cutover blockers for mssql-python: cursor.timeout maps to
// SQL_ATTR_QUERY_TIMEOUT, and the connection-pool check-in path plus
// `Connection.setcatalog` map to SQL_ATTR_CURRENT_CATALOG. Every assertion here
// was measured against msodbcsql 18 first, so the suite doubles as the parity
// contract for `run_e2e.ps1 -CompareWithMsodbcsql`.
//
// Query timeout:
//   1.  QueryTimeoutDefaultsToNoLimit      - statement default is 0
//   2.  QueryTimeoutRoundTrips             - set 30, read back 30
//   3.  QueryTimeoutAtCapIsSilent          - 0xFFFE accepted without a warning
//   4.  QueryTimeoutPastCapIsClamped       - 0x10000 -> 0xFFFE + 01S02
//   5.  ConnectionQueryTimeoutFansOut      - existing statements see the change
//   6.  ConnectionQueryTimeoutIsInherited  - statements allocated later see it
//   7.  ConnectionQueryTimeoutIsWriteOnly  - SQLGetConnectAttr -> HY092
//   8.  ConnectionQueryTimeoutClamps       - 0x10000 -> 01S02
//
// Current catalog:
//   9.  CurrentCatalogReportsConnectedDb   - get matches DB_NAME()
//   10. SetCurrentCatalogSwitchesDatabase  - USE round trip + 5701 info message
//   11. SetCurrentCatalogIsCaseInsensitive - same db, different case -> no-op
//   12. SetCurrentCatalogDefaultIsNoOp     - "(Default)" leaves the db alone
//   13. SetCurrentCatalogUnknownDbFails    - HY024, database unchanged
//   14. SetCurrentCatalogEmptyFails        - HY024
//   15. SetCurrentCatalogTooLongFails      - > 128 chars -> HY024
//   16. SetCurrentCatalogNegativeLenFails  - cbValue = -7 -> HY090
//   17. CurrentCatalogTracksRawUseStatement- ENVCHANGE is observed
//   18. CurrentCatalogTruncatesWithInfo    - short buffer -> 01004 + full length
//   19. CurrentCatalogLengthQuery          - null buffer returns the length
//   20. SetCurrentCatalogRejectsOpenCursor - 24000
//   21. SetCurrentCatalogKeepsTransaction  - @@TRANCOUNT survives the switch
//   22. SetCurrentCatalogQuotesIdentifier  - injection attempt stays one name

#include "odbc_test_fixture.h"

#include <string>

#ifndef SQL_ATTR_CURRENT_CATALOG
#define SQL_ATTR_CURRENT_CATALOG 109
#endif

namespace {

// msodbcsql caps the timeout at 0xFFFE and reports 01S02 above it
// (sqlcmisc.cpp:3988). Larger requests are clamped, never rejected.
constexpr SQLULEN kMaxQueryTimeout = 0xfffe;

// SQL Server's sysname limit; longer catalog names are rejected outright.
constexpr size_t kSysnameLen = 128;

} // namespace

class AttributesTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured";
        }
        Connect();
    }

    // --- query timeout ------------------------------------------------------

    SQLRETURN SetStmtTimeout(SQLHSTMT hstmt, SQLULEN seconds) {
        return SQLSetStmtAttr(hstmt, SQL_ATTR_QUERY_TIMEOUT,
                              reinterpret_cast<SQLPOINTER>(seconds), 0);
    }

    SQLULEN GetStmtTimeout(SQLHSTMT hstmt) {
        SQLULEN out = 0xdeadbeef;
        SQLRETURN rc = SQLGetStmtAttr(hstmt, SQL_ATTR_QUERY_TIMEOUT, &out,
                                      sizeof(out), nullptr);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, hstmt);
        return out;
    }

    SQLRETURN SetConnTimeout(SQLULEN seconds) {
        return SQLSetConnectAttr(dbc_, SQL_ATTR_QUERY_TIMEOUT,
                                 reinterpret_cast<SQLPOINTER>(seconds), 0);
    }

    // --- current catalog ----------------------------------------------------

    SQLRETURN SetCatalog(const std::string& name) {
        SqlTString wide = ODBCTestUtils::ToSqlTStr(name);
        return SQLSetConnectAttr(dbc_, SQL_ATTR_CURRENT_CATALOG,
                                 const_cast<SQLTCHAR*>(wide.c_str()), SQL_NTS);
    }

    /// Read SQL_ATTR_CURRENT_CATALOG into a generously sized buffer.
    std::string GetCatalog() {
        SQLTCHAR buf[256] = {};
        SQLINTEGER len = -1;
        SQLRETURN rc = SQLGetConnectAttr(dbc_, SQL_ATTR_CURRENT_CATALOG, buf,
                                         sizeof(buf), &len);
        EXPECT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
        return ODBCTestUtils::ToNarrow(SqlTString(buf));
    }

    /// The database the server actually thinks the session is using. The
    /// attribute is driver-side state; this is ground truth.
    std::string DbName() { return ScalarString("SELECT DB_NAME()"); }

    std::string ScalarString(const std::string& sql) {
        SqlTString wide = ODBCTestUtils::ToSqlTStr(sql);
        SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(wide.c_str()),
                                     SQL_NTS);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLTCHAR buf[256] = {};
        SQLLEN ind = 0;
        rc = SQLGetData(stmt_, 1, SQL_C_TCHAR, buf, sizeof(buf), &ind);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        SQLCloseCursor(stmt_);
        return ODBCTestUtils::ToNarrow(SqlTString(buf));
    }

    SQLINTEGER ScalarInt(const std::string& sql) {
        SqlTString wide = ODBCTestUtils::ToSqlTStr(sql);
        SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(wide.c_str()),
                                     SQL_NTS);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        SQLINTEGER value = -1;
        SQLLEN ind = 0;
        rc = SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &ind);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        SQLCloseCursor(stmt_);
        return value;
    }

    /// A database guaranteed to exist and to differ from the current one, so a
    /// switch is a real round trip rather than the case-insensitive short
    /// circuit.
    std::string OtherDatabase() {
        std::string current = DbName();
        for (char& c : current) {
            c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
        }
        return current == "master" ? "tempdb" : "master";
    }

    static bool EqualsIgnoreCase(const std::string& a, const std::string& b) {
        if (a.size() != b.size()) {
            return false;
        }
        for (size_t i = 0; i < a.size(); ++i) {
            if (std::tolower(static_cast<unsigned char>(a[i])) !=
                std::tolower(static_cast<unsigned char>(b[i]))) {
                return false;
            }
        }
        return true;
    }
};

// ===========================================================================
// SQL_ATTR_QUERY_TIMEOUT
// ===========================================================================

// -------------------------------------------------------------------
// Variation 1 - ODBC's documented default is 0, meaning "no limit".
// -------------------------------------------------------------------
TEST_F(AttributesTest, QueryTimeoutDefaultsToNoLimit) {
    EXPECT_EQ(0u, GetStmtTimeout(stmt_));
}

// -------------------------------------------------------------------
// Variation 2 - plain round trip; this is the only path mssql-python's
// cursor.timeout uses.
// -------------------------------------------------------------------
TEST_F(AttributesTest, QueryTimeoutRoundTrips) {
    EXPECT_SQL_OK(SetStmtTimeout(stmt_, 30), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(30u, GetStmtTimeout(stmt_));
}

// -------------------------------------------------------------------
// Variation 3 - exactly at the cap: accepted with no diagnostic.
// -------------------------------------------------------------------
TEST_F(AttributesTest, QueryTimeoutAtCapIsSilent) {
    EXPECT_EQ(SQL_SUCCESS, SetStmtTimeout(stmt_, kMaxQueryTimeout));
    EXPECT_EQ(kMaxQueryTimeout, GetStmtTimeout(stmt_));
}

// -------------------------------------------------------------------
// Variation 4 - past the cap: clamped and reported, never rejected. A
// rejection would break callers that pass a "very large" sentinel.
// -------------------------------------------------------------------
TEST_F(AttributesTest, QueryTimeoutPastCapIsClamped) {
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SetStmtTimeout(stmt_, 0x10000));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S02");
    EXPECT_EQ(kMaxQueryTimeout, GetStmtTimeout(stmt_));
}

// -------------------------------------------------------------------
// Variation 5 - setting on the connection rewrites statements that already
// exist (sqlcmisc.cpp:2879).
// -------------------------------------------------------------------
TEST_F(AttributesTest, ConnectionQueryTimeoutFansOut) {
    SQLHSTMT other = AllocStmt();
    ASSERT_NE(other, nullptr);

    EXPECT_SQL_OK(SetConnTimeout(17), SQL_HANDLE_DBC, dbc_);

    EXPECT_EQ(17u, GetStmtTimeout(stmt_));
    EXPECT_EQ(17u, GetStmtTimeout(other));
}

// -------------------------------------------------------------------
// Variation 6 - and becomes the default for statements allocated afterwards
// (sqlcfunc.cpp:173).
// -------------------------------------------------------------------
TEST_F(AttributesTest, ConnectionQueryTimeoutIsInherited) {
    EXPECT_SQL_OK(SetConnTimeout(23), SQL_HANDLE_DBC, dbc_);

    SQLHSTMT fresh = AllocStmt();
    ASSERT_NE(fresh, nullptr);
    EXPECT_EQ(23u, GetStmtTimeout(fresh));
}

// -------------------------------------------------------------------
// Variation 7 - the connection accepts the attribute but will not return it:
// msodbcsql has no DBC get arm for it, so it falls through to HY092.
// -------------------------------------------------------------------
TEST_F(AttributesTest, ConnectionQueryTimeoutIsWriteOnly) {
    EXPECT_SQL_OK(SetConnTimeout(11), SQL_HANDLE_DBC, dbc_);

    SQLULEN out = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetConnectAttr(dbc_, SQL_ATTR_QUERY_TIMEOUT, &out,
                                           sizeof(out), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY092");
}

// -------------------------------------------------------------------
// Variation 8 - the connection clamps with the same warning as a statement.
// -------------------------------------------------------------------
TEST_F(AttributesTest, ConnectionQueryTimeoutClamps) {
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SetConnTimeout(0x10000));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "01S02");
    EXPECT_EQ(kMaxQueryTimeout, GetStmtTimeout(stmt_));
}

// ===========================================================================
// SQL_ATTR_CURRENT_CATALOG
// ===========================================================================

// -------------------------------------------------------------------
// Variation 9 - after connecting, the attribute agrees with the server.
// -------------------------------------------------------------------
TEST_F(AttributesTest, CurrentCatalogReportsConnectedDb) {
    EXPECT_TRUE(EqualsIgnoreCase(GetCatalog(), DbName()))
        << "attribute=" << GetCatalog() << " DB_NAME()=" << DbName();
}

// -------------------------------------------------------------------
// Variation 10 - a real switch issues USE and surfaces the server's 5701
// "Changed database context" info message.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogSwitchesDatabase) {
    const std::string target = OtherDatabase();

    SQLRETURN rc = SetCatalog(target);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_TRUE(ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, dbc_, "01000"));

    EXPECT_TRUE(EqualsIgnoreCase(target, DbName()));
    EXPECT_TRUE(EqualsIgnoreCase(target, GetCatalog()));
}

// -------------------------------------------------------------------
// Variation 11 - SQL Server database names are case insensitive, so setting
// the current database under a different case is a no-op. Not short
// circuiting would cost a round trip on every pool check-in.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogIsCaseInsensitive) {
    std::string current = DbName();
    std::string shouted = current;
    for (char& c : shouted) {
        c = static_cast<char>(std::toupper(static_cast<unsigned char>(c)));
    }

    EXPECT_EQ(SQL_SUCCESS, SetCatalog(shouted));
    EXPECT_TRUE(EqualsIgnoreCase(current, DbName()));
}

// -------------------------------------------------------------------
// Variation 12 - "(Default)" is the documented "leave it alone" sentinel.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogDefaultIsNoOp) {
    const std::string before = DbName();
    EXPECT_EQ(SQL_SUCCESS, SetCatalog("(Default)"));
    EXPECT_EQ(before, DbName());
}

// -------------------------------------------------------------------
// Variation 13 - a missing database fails with HY024, not the 08004 the
// error-number map would give: msodbcsql overwrites the state for this path
// (sqlcmisc.cpp:1873). The session must keep working on the old database.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogUnknownDbFails) {
    const std::string before = DbName();

    EXPECT_EQ(SQL_ERROR, SetCatalog("no_such_db_2f8c1a"));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");

    EXPECT_EQ(before, DbName());
    EXPECT_TRUE(EqualsIgnoreCase(before, GetCatalog()));
}

// -------------------------------------------------------------------
// Variation 14 - an empty name is not "reset to default".
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogEmptyFails) {
    const std::string before = DbName();
    EXPECT_EQ(SQL_ERROR, SetCatalog(""));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");
    EXPECT_EQ(before, DbName());
}

// -------------------------------------------------------------------
// Variation 15 - longer than sysname is rejected before any round trip.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogTooLongFails) {
    EXPECT_EQ(SQL_ERROR, SetCatalog(std::string(kSysnameLen + 1, 'x')));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");
}

// -------------------------------------------------------------------
// Variation 16 - a negative length that is not SQL_NTS is a buffer-length
// error (HY090), distinct from an invalid value.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogNegativeLenFails) {
    SqlTString wide = ODBCTestUtils::ToSqlTStr("master");
    SQLRETURN rc = SQLSetConnectAttr(dbc_, SQL_ATTR_CURRENT_CATALOG,
                                     const_cast<SQLTCHAR*>(wide.c_str()), -7);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY090");
}

// -------------------------------------------------------------------
// Variation 17 - the attribute reflects the session, not just what was set
// through it: a raw USE has to move it too (TDS ENVCHANGE).
// -------------------------------------------------------------------
TEST_F(AttributesTest, CurrentCatalogTracksRawUseStatement) {
    const std::string target = OtherDatabase();

    SqlTString sql = ODBCTestUtils::ToSqlTStr("USE [" + target + "]");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()),
                                SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    SQLCloseCursor(stmt_);

    EXPECT_TRUE(EqualsIgnoreCase(target, GetCatalog()));
}

// -------------------------------------------------------------------
// Variation 18 - a short buffer truncates with 01004 but still reports the
// full length in bytes, so a caller can size a second call correctly.
// -------------------------------------------------------------------
TEST_F(AttributesTest, CurrentCatalogTruncatesWithInfo) {
    const std::string full = GetCatalog();
    ASSERT_GE(full.size(), 3u);

    SQLTCHAR buf[3] = {};
    SQLINTEGER len = -1;
    SQLRETURN rc = SQLGetConnectAttr(dbc_, SQL_ATTR_CURRENT_CATALOG, buf,
                                     sizeof(buf), &len);

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "01004");
    EXPECT_EQ(static_cast<SQLINTEGER>(full.size() * sizeof(SQLTCHAR)), len);
    // Room for two characters plus the terminator.
    EXPECT_EQ(full.substr(0, 2), ODBCTestUtils::ToNarrow(SqlTString(buf)));
}

// -------------------------------------------------------------------
// Variation 19 - the standard "ask for the length first" probe.
// -------------------------------------------------------------------
TEST_F(AttributesTest, CurrentCatalogLengthQuery) {
    const std::string full = GetCatalog();

    SQLINTEGER len = -1;
    SQLRETURN rc = SQLGetConnectAttr(dbc_, SQL_ATTR_CURRENT_CATALOG, nullptr, 0,
                                     &len);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLINTEGER>(full.size() * sizeof(SQLTCHAR)), len);
}

// -------------------------------------------------------------------
// Variation 20 - an open cursor blocks the switch with 24000. Allowing it
// would invalidate the rows still being read.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogRejectsOpenCursor) {
    const std::string before = DbName();
    // Resolve the target before opening the cursor: the helper runs its own
    // query on stmt_, which the open cursor would block.
    const std::string target = OtherDatabase();

    SqlTString sql =
        ODBCTestUtils::ToSqlTStr("SELECT TOP 10 object_id FROM sys.objects");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()),
                                SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_ERROR, SetCatalog(target));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "24000");

    SQLCloseCursor(stmt_);
    EXPECT_EQ(before, DbName());
}

// -------------------------------------------------------------------
// Variation 21 - an open transaction is preserved across the switch. USE
// does not implicitly commit, and neither driver rejects the call.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogKeepsTransaction) {
    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT,
                                    reinterpret_cast<SQLPOINTER>(
                                        SQL_AUTOCOMMIT_OFF),
                                    0),
                  SQL_HANDLE_DBC, dbc_);

    ASSERT_EQ(1, ScalarInt("SELECT @@TRANCOUNT"));

    const std::string target = OtherDatabase();
    ASSERT_SQL_OK(SetCatalog(target), SQL_HANDLE_DBC, dbc_);

    EXPECT_TRUE(EqualsIgnoreCase(target, DbName()));
    EXPECT_EQ(1, ScalarInt("SELECT @@TRANCOUNT"));

    SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK);
    SQLSetConnectAttr(dbc_, SQL_ATTR_AUTOCOMMIT,
                      reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_ON), 0);
}

// -------------------------------------------------------------------
// Variation 22 - the name is bracket-quoted, so a payload that would be a
// second statement in raw SQL stays a single (missing) identifier.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SetCurrentCatalogQuotesIdentifier) {
    const std::string before = DbName();

    EXPECT_EQ(SQL_ERROR, SetCatalog("tempdb]; SELECT 1--"));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");

    EXPECT_EQ(before, DbName());
}
