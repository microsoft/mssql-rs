// Copyright (c) Microsoft Corporation. All rights reserved.
// attributes_test.cpp  -  Tests for the ODBC connection and statement
//                         attributes mssql-python depends on.
//
// SQL_ATTR_QUERY_TIMEOUT and SQL_ATTR_CURRENT_CATALOG are cutover blockers:
// cursor.timeout maps to the first, and the connection-pool check-in path plus
// `Connection.setcatalog` map to the second. The rest of the statement
// attributes reach the driver through the SQLSetStmtAttr pass-through surface
// (plan §4.10). Every assertion here was measured against msodbcsql 18 first,
// so the suite doubles as the parity contract for
// `run_e2e.ps1 -CompareWithMsodbcsql`.
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
//   23. PreConnectDefaultCatalogIsNotASentinel - "(Default)" only means
//                                            "no change" once connected
//
// Attribute rejection policy:
//   24. UnknownStatementAttributeIsInvalidIdentifier   - HY092
//   25. UnknownConnectionAttributeIsInvalidIdentifier  - HY092
//   26. StatementOnlyAttributeIsRejectedOnAConnection  - scope-keyed HY092
//   27. ConnectionOnlyAttributeIsRejectedOnAStatement  - the mirror image
//   28. RowNumberIsNotSettable                         - operation-keyed HY092
//   29. EveryRecognizedStatementAttributeIsAnswered    - no HY092/HYC00 left
//   30. UnimplementedConnectionAttributeIsNotImplemented - HYC00 on the get path
//   31. HostileAttributePayloadDoesNotFault            - HYC00, session survives
//   32. KeystoreDataGetIsRecognizedNotUnknown          - recognized on both, never HY092
//
// Remaining statement attributes:
//   33. StatementAttributeDefaultsMatchMsodbcsql       - four defaults are not 0
//   34. StatementAttributesRoundTrip                   - written values survive
//   35. RowsetSizeIsIndependentOfRowArraySize          - 9 and 27 are not aliases
//   36. MaxLengthIsSubstitutedWithAWarning             - non-zero -> 8000 + 01S02
//   37. KeysetSizeIsRefusedWithAWarning                - non-zero -> 01S02, stays 0
//   38. SimulateCursorOnlyAcceptsUnique                - anything else -> 01S02
//   39. CursorSensitivityUnspecifiedNormalisesToInsensitive - silent normalisation
//   40. CursorScrollableAgreesWithCursorType           - one setting, two names
//   41. RowNumberRequiresAnOpenCursor                  - 24000 off a row
//   42. MaxRowsBoundsTheResultSet                      - the cap is enforced
//   43. MaxRowsAppliesToEachResultSet                  - per result set, not per stmt
//   44. MaxRowsCutoffLeavesTheCursorOffTheRow          - cap end == natural end
//   45. MaxRowsBoundsCatalogResultSets                 - the cap reaches SQLTables too
//   46. ParamBindOffsetShiftsTheBoundBuffers           - the offset is honored
//
// SQL Server vendor statement attributes (SQL_SOPT_SS_*):
//   47. VendorStatementAttributeDefaultsMatchMsodbcsql - four defaults are not 0
//   48. VendorBooleanAttributesRejectOutOfRangeValues  - driver-sourced HY024
//   49. VendorRangeAttributesAcceptTheirWholeRange     - masks, not enumerations
//   50. QueryNotificationTimeoutRejectsZeroAndKeepsItsValue - 0 is not "no limit"
//   51. VendorUnsupportedFeatureAttributesRefuseEveryValue - HY024, not HY092
//   52. VendorGetOnlyAttributesAreNotSettable          - HY092, not HY024
//   53. CurrentCommandTracksTheResultSetOrdinal        - per execute, not a flag
//   54. QueryNotificationStringsFollowTheByteLengthContract - bytes, not chars
//   55. IntegerGetsReportTheValueWidth                 - StringLength on success
//   56. CurrentCommandAdvancesThroughNonRowResults     - DML counts too
//   57. QueryNotificationStringsRejectBadNegativeLengths - only SQL_NTS
//   58. QueryNotificationTimeoutCeilingIsIntMax        - INT_MAX, not SQLULEN

#include "odbc_test_fixture.h"

#include <cstring>
#include <functional>
#include <vector>
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

// -- attribute identifiers used only to exercise the rejection policy --------
//
// Values measured against msodbcsql 18 rather than assumed; see
// `docs/attributes_plan.md` §8 for the sweep that produced them.

// Not an attribute in any scope. Both drivers must answer HY092.
constexpr SQLINTEGER kUnknownAttribute = 99999;

// Statement-only. msodbcsql's connection switch has no arm for it, because it
// falls outside the ODBC 2.x statement-option fan-out band (0-12, 29).
constexpr SQLINTEGER kRowArraySizeAttr = 27;

// Connection-only (SQL_ATTR_ENLIST_IN_DTC). Rejected on a statement handle.
constexpr SQLINTEGER kEnlistInDtcAttr = 1207;

// SQL_ROWSET_SIZE - the ODBC 2.x rowset size. It has no SQL_ATTR_ alias in
// sqlext.h, and msodbcsql keeps it in a slot of its own rather than aliasing
// SQL_ATTR_ROW_ARRAY_SIZE (Variation 35).
constexpr SQLINTEGER kRowsetSizeAttr = 9;

// SQL_SOPT_SS_* - the SQL Server vendor statement attributes (slice S6).
// They share the 1225-1238 band with the SQL_COPT_SS_* connection attributes,
// so recognition is keyed by scope as well as identifier: 1232 is
// DEFER_PREPARE on a statement and PRESERVE_CURSORS on a connection.
constexpr SQLINTEGER kSsTextptrLogging = 1225;
constexpr SQLINTEGER kSsCurrentCommand = 1226;
constexpr SQLINTEGER kSsHiddenColumns = 1227;
constexpr SQLINTEGER kSsNobrowsetable = 1228;
constexpr SQLINTEGER kSsRegionalize = 1229;
constexpr SQLINTEGER kSsCursorOptions = 1230;
constexpr SQLINTEGER kSsNocountStatus = 1231;
constexpr SQLINTEGER kSsDeferPrepare = 1232;
constexpr SQLINTEGER kSsQnTimeout = 1233;
constexpr SQLINTEGER kSsQnMsgtext = 1234;
constexpr SQLINTEGER kSsQnOptions = 1235;
constexpr SQLINTEGER kSsParamFocus = 1236;
constexpr SQLINTEGER kSsNameScope = 1237;
constexpr SQLINTEGER kSsColumnEncryption = 1238;

// SQL_ATTR_ROW_NUMBER - readable but not settable on msodbcsql, which makes it
// the statement-side mirror of the connection-side SQL_ATTR_QUERY_TIMEOUT
// asymmetry that Variation 13 pins down.
constexpr SQLINTEGER kRowNumberAttr = 14;

// SQL_COPT_SS_MARS_ENABLED - readable on msodbcsql. MARS is a deferred feature
// here (plan §4.12), and "is MARS available?" is exactly the question a caller
// needs a distinguishable answer to.
constexpr SQLINTEGER kMarsEnabledAttr = 1224;

// SQL_COPT_SS_CEKEYSTOREDATA - on the set path msodbcsql dereferences the
// pointer as a struct without validating it, so a caller passing a plain buffer
// faults the process. This driver must refuse it cleanly instead. The get path
// is safe on both and answers HY010 there, so it is asserted on both drivers.
constexpr SQLINTEGER kCekeystoreDataAttr = 1252;

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

    // --- remaining statement attributes -------------------------------------

    SQLRETURN SetStmtULen(SQLINTEGER attribute, SQLULEN value) {
        return SQLSetStmtAttr(stmt_, attribute,
                              reinterpret_cast<SQLPOINTER>(value), 0);
    }

    SQLULEN GetStmtULen(SQLINTEGER attribute) {
        SQLULEN out = 0xdeadbeef;
        SQLRETURN rc =
            SQLGetStmtAttr(stmt_, attribute, &out, sizeof(out), nullptr);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        return out;
    }

    /// Rows a `SELECT` actually hands back, which is what SQL_ATTR_MAX_ROWS has
    /// to bound. Counted through SQLFetch rather than a server-side aggregate,
    /// because the cap is a driver-side promise.
    int FetchedRowCount(const std::string& sql) {
        SqlTString wide = ODBCTestUtils::ToSqlTStr(sql);
        EXPECT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(wide.c_str()),
                                    SQL_NTS),
                      SQL_HANDLE_STMT, stmt_);
        int rows = 0;
        while (SQL_SUCCEEDED(SQLFetch(stmt_))) {
            ++rows;
        }
        SQLCloseCursor(stmt_);
        return rows;
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

// -------------------------------------------------------------------
// Variation 23 - "(Default)" is only a sentinel once connected. Before
// connect it is stored like any other name, so the login fails on it just
// as it would on a database that does not exist. msodbcsql uses the string
// only in its DSN-setup dialog (DEFAULT_STRING, sqlsrv.h:2065), never as a
// pre-connect attribute value, and both drivers were measured to reject it.
// -------------------------------------------------------------------
TEST_F(AttributesTest, PreConnectDefaultCatalogIsNotASentinel) {
    const auto& cfg = ODBCTestConfig::Instance();
    if (cfg.Server().empty() || cfg.Driver().empty()) {
        GTEST_SKIP() << "Needs an explicit server/driver to omit DATABASE";
    }

    SQLHDBC dbc = SQL_NULL_HDBC;
    ASSERT_TRUE(SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc)));

    SqlTString sentinel = ODBCTestUtils::ToSqlTStr("(Default)");
    EXPECT_TRUE(SQL_SUCCEEDED(SQLSetConnectAttr(
        dbc, SQL_ATTR_CURRENT_CATALOG, sentinel.data(), SQL_NTS)));

    // No DATABASE keyword: the connection string would otherwise outrank the
    // attribute and hide what the sentinel does.
    std::string cs = "DRIVER={" + cfg.Driver() + "};SERVER=" + cfg.Server() + ";";
    if (!cfg.TrustCert().empty()) {
        cs += "TrustServerCertificate=" + cfg.TrustCert() + ";";
    }
    cs += cfg.HasCredentials()
              ? "UID=" + cfg.Uid() + ";PWD=" + cfg.Pwd() + ";"
              : "Trusted_Connection=Yes;";

    SqlTString conn = ODBCTestUtils::ToSqlTStr(cs);
    SQLTCHAR out[1024] = {};
    SQLSMALLINT out_len = 0;
    SQLRETURN ret = SQLDriverConnect(dbc, nullptr, conn.data(), SQL_NTS, out,
                                     1024, &out_len, SQL_DRIVER_NOPROMPT);

    // The server refuses the login rather than silently falling back, so a
    // caller that meant "use the login default" learns it did not happen.
    EXPECT_FALSE(SQL_SUCCEEDED(ret));
    if (SQL_SUCCEEDED(ret)) {
        SQLDisconnect(dbc);
    }
    SQLFreeHandle(SQL_HANDLE_DBC, dbc);
}

// ===========================================================================
// Attribute rejection policy (§4.10 pass-through hardening)
//
// mssql-python forwards arbitrary identifiers through `attrs_before` and
// `set_attr` without filtering, so the answer to "is this an attribute at all?"
// is part of the contract. Variations 24-28 are pure parity: both drivers must
// agree. Variations 29-31 assert behavior that is deliberately this driver's
// own, because msodbcsql implements the attribute (or faults on it). Variation
// 32 runs on both but expects a different SQLSTATE from each, asserting only
// the shared half of the contract: a recognized id is never HY092.
// ===========================================================================

// -------------------------------------------------------------------
// Variation 24 - an identifier that is not an attribute in any scope is
// HY092 on a statement, on both drivers.
// -------------------------------------------------------------------
TEST_F(AttributesTest, UnknownStatementAttributeIsInvalidIdentifier) {
    EXPECT_EQ(SQL_ERROR,
              SQLSetStmtAttr(stmt_, kUnknownAttribute,
                             reinterpret_cast<SQLPOINTER>(1), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY092");

    SQLULEN out = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetStmtAttr(stmt_, kUnknownAttribute, &out,
                                        sizeof(out), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY092");
}

// -------------------------------------------------------------------
// Variation 25 - the same identifier on a connection. Asserted after
// connect: before connect the Driver Manager buffers the call and the
// driver never sees it, so pre-connect proves nothing about either.
// -------------------------------------------------------------------
TEST_F(AttributesTest, UnknownConnectionAttributeIsInvalidIdentifier) {
    EXPECT_EQ(SQL_ERROR,
              SQLSetConnectAttr(dbc_, kUnknownAttribute,
                                reinterpret_cast<SQLPOINTER>(1), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY092");

    SQLULEN out = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetConnectAttr(dbc_, kUnknownAttribute, &out,
                                           sizeof(out), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY092");
}

// -------------------------------------------------------------------
// Variation 26 - recognition is scoped. SQL_ATTR_ROW_ARRAY_SIZE is a real
// statement attribute, but it sits outside the ODBC 2.x statement-option
// band that msodbcsql fans out from a connection, so on a connection it is
// as unknown as garbage.
// -------------------------------------------------------------------
TEST_F(AttributesTest, StatementOnlyAttributeIsRejectedOnAConnection) {
    EXPECT_EQ(SQL_ERROR,
              SQLSetConnectAttr(dbc_, kRowArraySizeAttr,
                                reinterpret_cast<SQLPOINTER>(10), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY092");
}

// -------------------------------------------------------------------
// Variation 27 - the mirror image: a connection-only attribute on a
// statement handle. Vendor ranges overlap between the two scopes, so this
// is the case a flat identifier table would get wrong.
// -------------------------------------------------------------------
TEST_F(AttributesTest, ConnectionOnlyAttributeIsRejectedOnAStatement) {
    EXPECT_EQ(SQL_ERROR,
              SQLSetStmtAttr(stmt_, kEnlistInDtcAttr,
                             reinterpret_cast<SQLPOINTER>(0), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY092");
}

// -------------------------------------------------------------------
// Variation 28 - recognition is keyed by operation as well as scope.
// SQL_ATTR_ROW_NUMBER is readable but not settable, so the set path
// rejects an identifier the get path accepts.
// -------------------------------------------------------------------
TEST_F(AttributesTest, RowNumberIsNotSettable) {
    EXPECT_EQ(SQL_ERROR,
              SQLSetStmtAttr(stmt_, kRowNumberAttr,
                             reinterpret_cast<SQLPOINTER>(1), 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY092");
}

// -------------------------------------------------------------------
// Variation 29 - every statement attribute msodbcsql recognizes on the
// set path is recognized here too. Before slice S6 this variation held
// the opposite: an attribute msodbcsql accepted and this driver answered
// with HYC00. That class is now empty, so the useful assertion is that it
// stays empty - a new row in the recognition table that nobody wired up
// fails here rather than reaching an application as HYC00.
//
// Recognition is asserted independently of the value, because 21 of these
// reject the probe value with HY024; what must never come back is HY092
// ("no such attribute") or HYC00 ("known but unavailable").
//
// SQL_ATTR_ASYNC_STMT_EVENT (29) is deliberately absent: the Driver
// Manager answers it itself with HY118 for any driver that does not
// advertise asynchronous notification, so it never reaches either
// driver's set path and cannot be compared here.
//
// The descriptor-handle attributes (SQL_ATTR_APP_ROW_DESC and its three
// neighbours, 10010-10013) are absent for a different reason: their value
// is a handle, and msodbcsql dereferences it without checking, so probing
// them with a placeholder access-violates inside the driver. Recognition
// of those is covered by the get path instead.
// -------------------------------------------------------------------
TEST_F(AttributesTest, EveryRecognizedStatementAttributeIsAnswered) {
    const SQLINTEGER attributes[] = {
        -2,   -1,   0,    1,    2,    3,    4,    5,    6,    7,
        8,    9,    10,   11,   12,   15,   16,   17,   18,   19,
        20,   21,   22,   23,   24,   25,   26,   27,   1225, 1227,
        1228, 1229, 1230, 1232, 1233, 1234, 1235, 1236, 1237, 1238,
        10014,
    };

    for (SQLINTEGER attribute : attributes) {
        SCOPED_TRACE(attribute);
        SQLRETURN rc = SetStmtULen(attribute, 1);
        if (SQL_SUCCEEDED(rc)) {
            continue;
        }
        const std::string state =
            ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
        EXPECT_NE("HY092", state) << "identifier not recognized";
        EXPECT_NE("HYC00", state) << "recognized but not implemented";
    }
}

// -------------------------------------------------------------------
// Variation 30 - the connection-side equivalent, on the get path. MARS is
// deferred here, so a caller probing for it gets "not implemented" rather
// than "no such attribute" and can fall back deliberately.
// -------------------------------------------------------------------
TEST_F(AttributesTest, UnimplementedConnectionAttributeIsNotImplemented) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SQLULEN out = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetConnectAttr(dbc_, kMarsEnabledAttr, &out,
                                           sizeof(out), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HYC00");
}

// -------------------------------------------------------------------
// Variation 31 - msodbcsql reads SQL_COPT_SS_CEKEYSTOREDATA as a struct
// with no validation and faults on a plain buffer. Refusing it is the
// whole point of routing unimplemented identifiers through a table
// instead of a pointer read, so this runs on this driver only - the
// msodbcsql leg would take down the test binary.
// -------------------------------------------------------------------
TEST_F(AttributesTest, HostileAttributePayloadDoesNotFault) {
    SKIP_IF_COMPARING_MSODBCSQL();

    unsigned char payload[8] = {};
    EXPECT_EQ(SQL_ERROR,
              SQLSetConnectAttr(dbc_, kCekeystoreDataAttr, payload,
                                sizeof(payload)));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HYC00");

    // Still usable afterwards: a rejected attribute must not disturb the
    // session.
    EXPECT_FALSE(DbName().empty());
}

// -------------------------------------------------------------------
// Variation 32 - the get half of the same identifier. This one does run
// on both drivers: msodbcsql only faults on the set path, and answers the
// get with HY010 ("function sequence error" - no keystore provider has
// been selected). The states differ, but the contract this table exists
// to enforce is the one both must satisfy: 1252 is a real attribute, so
// neither driver may claim it does not exist.
// -------------------------------------------------------------------
TEST_F(AttributesTest, KeystoreDataGetIsRecognizedNotUnknown) {
    unsigned char out[64] = {};
    SQLINTEGER written = 0;
    EXPECT_EQ(SQL_ERROR, SQLGetConnectAttr(dbc_, kCekeystoreDataAttr, out,
                                           sizeof(out), &written));

    // The shared assertion: recognized, therefore not HY092.
    const std::string state =
        ODBCTestUtils::GetDiagState(SQL_HANDLE_DBC, dbc_);
    EXPECT_NE("HY092", state);

    const char* target = std::getenv("ODBC_TEST_TARGET");
    if (target && std::string(target) == "msodbcsql") {
        // No keystore provider selected, so the sequence is wrong.
        EXPECT_EQ("HY010", state);
    } else {
        // Always Encrypted is deferred, so the feature is what is missing.
        EXPECT_EQ("HYC00", state);
    }

    EXPECT_FALSE(DbName().empty());
}

// ===========================================================================
// Remaining statement attributes
//
// mssql-python does not set these itself, but pyodbc-shaped code and ORMs do,
// and SQLSetStmtAttr is a pass-through surface (plan §4.10). Every default and
// every warning below was measured against msodbcsql 18 first.
// ===========================================================================

// -------------------------------------------------------------------
// Variation 33 - the defaults a caller reads before deciding whether to
// change anything. Four of them are non-zero, so answering a blanket 0
// would send an application down a different branch on each driver.
// -------------------------------------------------------------------
TEST_F(AttributesTest, StatementAttributeDefaultsMatchMsodbcsql) {
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_NOSCAN));
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_MAX_ROWS));
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_MAX_LENGTH));
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_ASYNC_ENABLE));
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_KEYSET_SIZE));
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_USE_BOOKMARKS));
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_ENABLE_AUTO_IPD));
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_METADATA_ID));

    // The four that are not zero.
    EXPECT_EQ(1u, GetStmtULen(kRowsetSizeAttr));
    EXPECT_EQ(static_cast<SQLULEN>(SQL_SC_UNIQUE),
              GetStmtULen(SQL_ATTR_SIMULATE_CURSOR));
    EXPECT_EQ(static_cast<SQLULEN>(SQL_RD_ON),
              GetStmtULen(SQL_ATTR_RETRIEVE_DATA));
    EXPECT_EQ(static_cast<SQLULEN>(SQL_INSENSITIVE),
              GetStmtULen(SQL_ATTR_CURSOR_SENSITIVITY));
}

// -------------------------------------------------------------------
// Variation 34 - a written value has to survive the round trip. Silently
// discarding the write and answering the default is the failure mode this
// slice exists to remove: it looks like success at the call site.
// -------------------------------------------------------------------
TEST_F(AttributesTest, StatementAttributesRoundTrip) {
    struct AttrCase {
        SQLINTEGER attribute;
        SQLULEN value;
    };
    const AttrCase cases[] = {
        {SQL_ATTR_NOSCAN, SQL_NOSCAN_ON},
        {SQL_ATTR_ASYNC_ENABLE, SQL_ASYNC_ENABLE_OFF},
        {SQL_ATTR_RETRIEVE_DATA, SQL_RD_ON},
        {SQL_ATTR_USE_BOOKMARKS, SQL_UB_VARIABLE},
        {SQL_ATTR_ENABLE_AUTO_IPD, SQL_FALSE},
        {SQL_ATTR_METADATA_ID, SQL_TRUE},
        {SQL_ATTR_PARAM_BIND_TYPE, 16},
        {kRowsetSizeAttr, 10},
    };
    for (const AttrCase& c : cases) {
        EXPECT_EQ(SQL_SUCCESS, SetStmtULen(c.attribute, c.value))
            << "set " << c.attribute;
        EXPECT_EQ(c.value, GetStmtULen(c.attribute))
            << "readback " << c.attribute;
    }
}

// -------------------------------------------------------------------
// Variation 35 - SQL_ROWSET_SIZE (9) and SQL_ATTR_ROW_ARRAY_SIZE (27)
// have overlapping documentation but separate storage on msodbcsql.
// Aliasing them would silently resize an application's rowset.
// -------------------------------------------------------------------
TEST_F(AttributesTest, RowsetSizeIsIndependentOfRowArraySize) {
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(kRowsetSizeAttr, 7));
    EXPECT_EQ(7u, GetStmtULen(kRowsetSizeAttr));
    EXPECT_EQ(1u, GetStmtULen(SQL_ATTR_ROW_ARRAY_SIZE));

    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_ROW_ARRAY_SIZE, 5));
    EXPECT_EQ(5u, GetStmtULen(SQL_ATTR_ROW_ARRAY_SIZE));
    EXPECT_EQ(7u, GetStmtULen(kRowsetSizeAttr));
}

// -------------------------------------------------------------------
// Variation 36 - SQL_ATTR_MAX_LENGTH is not honoured, so msodbcsql
// substitutes its own limit and says so rather than accepting a cap it
// will not apply. Zero means "no limit" and passes silently.
// -------------------------------------------------------------------
TEST_F(AttributesTest, MaxLengthIsSubstitutedWithAWarning) {
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_LENGTH, 0));
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_MAX_LENGTH));

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SetStmtULen(SQL_ATTR_MAX_LENGTH, 100));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S02");
    EXPECT_EQ(8000u, GetStmtULen(SQL_ATTR_MAX_LENGTH));
}

// -------------------------------------------------------------------
// Variation 37 - keyset-driven cursors are not available, so a non-zero
// keyset size is reported as changed and the attribute stays at 0. A
// caller that reads it back learns the cursor is not keyset-driven.
// -------------------------------------------------------------------
TEST_F(AttributesTest, KeysetSizeIsRefusedWithAWarning) {
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_KEYSET_SIZE, 0));

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, SetStmtULen(SQL_ATTR_KEYSET_SIZE, 10));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S02");
    EXPECT_EQ(0u, GetStmtULen(SQL_ATTR_KEYSET_SIZE));
}

// -------------------------------------------------------------------
// Variation 38 - the only cursor simulation on offer is SQL_SC_UNIQUE,
// which is also the default. Asking for a different one is answered with
// the substitution warning, not an error.
// -------------------------------------------------------------------
TEST_F(AttributesTest, SimulateCursorOnlyAcceptsUnique) {
    EXPECT_EQ(SQL_SUCCESS,
              SetStmtULen(SQL_ATTR_SIMULATE_CURSOR, SQL_SC_UNIQUE));
    EXPECT_EQ(static_cast<SQLULEN>(SQL_SC_UNIQUE),
              GetStmtULen(SQL_ATTR_SIMULATE_CURSOR));

    EXPECT_EQ(SQL_SUCCESS_WITH_INFO,
              SetStmtULen(SQL_ATTR_SIMULATE_CURSOR, SQL_SC_NON_UNIQUE));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01S02");
    EXPECT_EQ(static_cast<SQLULEN>(SQL_SC_UNIQUE),
              GetStmtULen(SQL_ATTR_SIMULATE_CURSOR));
}

// -------------------------------------------------------------------
// Variation 39 - SQL_ATTR_CURSOR_SENSITIVITY defaults to SQL_INSENSITIVE
// and normalises SQL_UNSPECIFIED to it silently: "I don't care" is
// answered with what the driver actually does, with no diagnostic.
// -------------------------------------------------------------------
TEST_F(AttributesTest, CursorSensitivityUnspecifiedNormalisesToInsensitive) {
    EXPECT_EQ(SQL_SUCCESS,
              SetStmtULen(SQL_ATTR_CURSOR_SENSITIVITY, SQL_UNSPECIFIED));
    EXPECT_EQ("", ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_));
    EXPECT_EQ(static_cast<SQLULEN>(SQL_INSENSITIVE),
              GetStmtULen(SQL_ATTR_CURSOR_SENSITIVITY));

    EXPECT_EQ(SQL_SUCCESS,
              SetStmtULen(SQL_ATTR_CURSOR_SENSITIVITY, SQL_INSENSITIVE));
    EXPECT_EQ(static_cast<SQLULEN>(SQL_INSENSITIVE),
              GetStmtULen(SQL_ATTR_CURSOR_SENSITIVITY));
}

// -------------------------------------------------------------------
// Variation 40 - SQL_ATTR_CURSOR_SCROLLABLE is the boolean face of
// SQL_ATTR_CURSOR_TYPE, so the two must never disagree. That invariant is
// asserted on both drivers; the reachable values differ, because
// msodbcsql really does have scrollable cursors and this driver does not
// (the same divergence Variation 26's cursor-type sibling records).
// -------------------------------------------------------------------
TEST_F(AttributesTest, CursorScrollableAgreesWithCursorType) {
    EXPECT_EQ(static_cast<SQLULEN>(SQL_NONSCROLLABLE),
              GetStmtULen(SQL_ATTR_CURSOR_SCROLLABLE));
    EXPECT_EQ(SQL_SUCCESS,
              SetStmtULen(SQL_ATTR_CURSOR_SCROLLABLE, SQL_NONSCROLLABLE));
    EXPECT_EQ(static_cast<SQLULEN>(SQL_NONSCROLLABLE),
              GetStmtULen(SQL_ATTR_CURSOR_SCROLLABLE));

    const SQLRETURN rc = SetStmtULen(SQL_ATTR_CURSOR_SCROLLABLE, SQL_SCROLLABLE);
    const SQLULEN scrollable = GetStmtULen(SQL_ATTR_CURSOR_SCROLLABLE);
    const SQLULEN cursor_type = GetStmtULen(SQL_ATTR_CURSOR_TYPE);

    // The shared invariant: scrollability and cursor type are one setting.
    EXPECT_EQ(scrollable == static_cast<SQLULEN>(SQL_NONSCROLLABLE),
              cursor_type == static_cast<SQLULEN>(SQL_CURSOR_FORWARD_ONLY));

    const char* target = std::getenv("ODBC_TEST_TARGET");
    if (target && std::string(target) == "msodbcsql") {
        EXPECT_EQ(SQL_SUCCESS, rc);
        EXPECT_EQ(static_cast<SQLULEN>(SQL_SCROLLABLE), scrollable);
    } else {
        // Only forward-only cursors exist here, so the request is reported
        // as changed rather than silently accepted and ignored.
        EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc);
        EXPECT_EQ(static_cast<SQLULEN>(SQL_NONSCROLLABLE), scrollable);
    }
}

// -------------------------------------------------------------------
// Variation 41 - SQL_ATTR_ROW_NUMBER reports the cursor position, so it
// is only answerable while there is one. Returning 0 for "no cursor"
// would be indistinguishable from a legitimate position.
// -------------------------------------------------------------------
TEST_F(AttributesTest, RowNumberRequiresAnOpenCursor) {
    SQLULEN out = 0xdeadbeef;

    // No cursor at all.
    EXPECT_EQ(SQL_ERROR, SQLGetStmtAttr(stmt_, kRowNumberAttr, &out,
                                        sizeof(out), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1 UNION ALL SELECT 2");
    EXPECT_SQL_OK(
        SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
        SQL_HANDLE_STMT, stmt_);

    // Executed, but not yet positioned on a row.
    EXPECT_EQ(SQL_ERROR, SQLGetStmtAttr(stmt_, kRowNumberAttr, &out,
                                        sizeof(out), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    // Positioned: answerable. Forward-only cursors do not track an absolute
    // row number, so both drivers report 0 rather than a running count.
    EXPECT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0u, GetStmtULen(kRowNumberAttr));

    // And unanswerable again once the cursor is gone.
    SQLCloseCursor(stmt_);
    EXPECT_EQ(SQL_ERROR, SQLGetStmtAttr(stmt_, kRowNumberAttr, &out,
                                        sizeof(out), nullptr));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");
}

// -------------------------------------------------------------------
// Variation 42 - SQL_ATTR_MAX_ROWS is the one attribute in this group
// that changes what comes back over the wire, so it is asserted on rows
// rather than on a read-back.
// -------------------------------------------------------------------
TEST_F(AttributesTest, MaxRowsBoundsTheResultSet) {
    const std::string sql =
        "SELECT TOP 10 object_id FROM sys.objects ORDER BY object_id";
    ASSERT_EQ(10, FetchedRowCount(sql));

    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, 3));
    EXPECT_EQ(3u, GetStmtULen(SQL_ATTR_MAX_ROWS));
    EXPECT_EQ(3, FetchedRowCount(sql));

    // A cap above the row count is not a cap.
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, 50));
    EXPECT_EQ(10, FetchedRowCount(sql));

    // 0 is the documented "unlimited", not "no rows".
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, 0));
    EXPECT_EQ(10, FetchedRowCount(sql));
}

// -------------------------------------------------------------------
// Variation 43 - the cap applies per result set, not per statement, so a
// batch returns up to MAX_ROWS rows from each. Counting across the whole
// batch instead would truncate every result set after the first.
// -------------------------------------------------------------------
TEST_F(AttributesTest, MaxRowsAppliesToEachResultSet) {
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, 2));

    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SELECT TOP 5 object_id FROM sys.objects ORDER BY object_id; "
        "SELECT TOP 5 object_id FROM sys.objects ORDER BY object_id");
    EXPECT_SQL_OK(
        SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
        SQL_HANDLE_STMT, stmt_);

    int first = 0;
    while (SQL_SUCCEEDED(SQLFetch(stmt_))) {
        ++first;
    }
    EXPECT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);

    int second = 0;
    while (SQL_SUCCEEDED(SQLFetch(stmt_))) {
        ++second;
    }
    SQLCloseCursor(stmt_);

    EXPECT_EQ(2, first);
    EXPECT_EQ(2, second);
}

// -------------------------------------------------------------------
// Variation 44 - stopping at SQL_ATTR_MAX_ROWS must leave the cursor in the
// same state as running off the end of the result set: off the row, with
// SQL_ATTR_ROW_NUMBER and SQLGetData both reporting 24000. A driver that
// short-circuits the fetch without invalidating the row stream would keep
// the last row readable past SQL_NO_DATA, so an application looping on
// SQLFetch and then reading columns would see one phantom row here and not
// on msodbcsql.
// -------------------------------------------------------------------
TEST_F(AttributesTest, MaxRowsCutoffLeavesTheCursorOffTheRow) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SELECT TOP 4 object_id FROM sys.objects ORDER BY object_id");

    // State after the cap cuts the third fetch off, and after the fourth fetch
    // of a two-row-capped set would have run out anyway: both must match the
    // state at the natural end of an uncapped set.
    struct Case {
        SQLULEN max_rows;
        int fetches;
        const char* label;
    } const cases[] = {
        {2, 3, "cap cutoff"},
        {0, 5, "natural end"},
    };

    for (const Case& c : cases) {
        SCOPED_TRACE(c.label);
        EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, c.max_rows));
        EXPECT_SQL_OK(
            SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, stmt_);

        SQLRETURN last = SQL_SUCCESS;
        for (int i = 0; i < c.fetches; ++i) {
            last = SQLFetch(stmt_);
        }
        EXPECT_EQ(SQL_NO_DATA, last);

        SQLULEN row = 0xdeadbeef;
        EXPECT_EQ(SQL_ERROR, SQLGetStmtAttr(stmt_, SQL_ATTR_ROW_NUMBER, &row,
                                            sizeof(row), nullptr));
        EXPECT_EQ("24000", ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_));

        SQLINTEGER value = 0;
        SQLLEN indicator = 0;
        EXPECT_EQ(SQL_ERROR,
                  SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value),
                             &indicator));
        EXPECT_EQ("24000", ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_));

        SQLCloseCursor(stmt_);
    }
}

// -------------------------------------------------------------------
// 45. MAX_ROWS bounds catalog result sets as well.
//
// Catalog functions are metadata RPCs whose rows come back through the
// ordinary fetch path, and msodbcsql caps them exactly like a query. A
// driver that special-cased catalog cursors would return more rows than
// the application asked for.
// -------------------------------------------------------------------
TEST_F(AttributesTest, MaxRowsBoundsCatalogResultSets) {
    struct Case {
        const char* label;
        std::function<SQLRETURN()> call;
    };
    const Case cases[] = {
        {"SQLTables", [&] {
             return SQLTables(stmt_, nullptr, 0, nullptr, 0, nullptr, 0,
                              nullptr, 0);
         }},
        {"SQLColumns", [&] {
             return SQLColumns(stmt_, nullptr, 0, nullptr, 0, nullptr, 0,
                               nullptr, 0);
         }},
        {"SQLGetTypeInfo", [&] {
             return SQLGetTypeInfo(stmt_, SQL_ALL_TYPES);
         }},
    };

    auto count_open_cursor = [&] {
        int rows = 0;
        while (SQL_SUCCEEDED(SQLFetch(stmt_))) {
            ++rows;
        }
        return rows;
    };

    for (const Case& c : cases) {
        SCOPED_TRACE(c.label);
        EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, 2));
        ASSERT_SQL_OK(c.call(), SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(2, count_open_cursor());
        SQLCloseCursor(stmt_);
    }

    // And the cap really was the reason: without it there is more to read.
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, 0));
    ASSERT_SQL_OK(SQLGetTypeInfo(stmt_, SQL_ALL_TYPES), SQL_HANDLE_STMT, stmt_);
    EXPECT_GT(count_open_cursor(), 2);
    SQLCloseCursor(stmt_);
}

// -------------------------------------------------------------------
// 46. SQL_ATTR_PARAM_BIND_OFFSET_PTR shifts the bound buffers.
//
// The attribute stores a pointer to an SQLLEN, and the driver adds that
// many bytes to the parameter's value pointer and its length/indicator
// pointer at execute time. Accepting the attribute and ignoring it would
// silently send the wrong value, so this checks the parameter the server
// actually received rather than a round-trip of the attribute.
//
// Both buffers advance by one offset, so they are laid out in a single
// struct whose size is the stride.
// -------------------------------------------------------------------
TEST_F(AttributesTest, ParamBindOffsetShiftsTheBoundBuffers) {
#pragma pack(push, 8)
    struct Slot {
        SQLLEN indicator;
        char text[8];
    };
#pragma pack(pop)

    Slot slots[2] = {};
    slots[0].indicator = 4;
    std::memcpy(slots[0].text, "AAAA", 4);
    slots[1].indicator = 4;
    std::memcpy(slots[1].text, "BBBB", 4);

    const SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS v");

    struct Case {
        SQLLEN offset;
        const char* expected;
    };
    const Case cases[] = {
        {0, "AAAA"},
        {static_cast<SQLLEN>(sizeof(Slot)), "BBBB"},
        {0, "AAAA"},  // and it is not sticky
    };

    for (const Case& c : cases) {
        SCOPED_TRACE(c.offset);
        SQLLEN offset = c.offset;
        EXPECT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_PARAM_BIND_OFFSET_PTR,
                                     &offset, 0),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR,
                                       SQL_VARCHAR, 8, 0, slots[0].text,
                                       sizeof(slots[0].text),
                                       &slots[0].indicator),
                      SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(
            SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

        char received[32] = {};
        SQLLEN got = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, received,
                                 sizeof(received), &got),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_STREQ(c.expected, received);

        SQLCloseCursor(stmt_);
        SQLFreeStmt(stmt_, SQL_RESET_PARAMS);
    }
}


// -------------------------------------------------------------------
// Variation 47 - the SQL Server vendor statement attributes have
// defaults, and four of them are not 0. A caller that assumes a
// zero-initialised block reads TEXTPTR_LOGGING, NOCOUNT_STATUS and
// DEFER_PREPARE as "off" when the driver has them on, and sees a
// query-notification timeout of 0 rather than the five-day default.
// -------------------------------------------------------------------
TEST_F(AttributesTest, VendorStatementAttributeDefaultsMatchMsodbcsql) {
    struct Case {
        SQLINTEGER attribute;
        SQLULEN expected;
        const char* label;
    } const cases[] = {
        {kSsTextptrLogging, 1, "TEXTPTR_LOGGING"},
        {kSsCurrentCommand, 0, "CURRENT_COMMAND"},
        {kSsHiddenColumns, 0, "HIDDEN_COLUMNS"},
        {kSsNobrowsetable, 0, "NOBROWSETABLE"},
        {kSsRegionalize, 0, "REGIONALIZE"},
        {kSsCursorOptions, 0, "CURSOR_OPTIONS"},
        {kSsNocountStatus, 1, "NOCOUNT_STATUS"},
        {kSsDeferPrepare, 1, "DEFER_PREPARE"},
        {kSsQnTimeout, 432000, "QUERYNOTIFICATION_TIMEOUT"},
        {kSsParamFocus, 0, "PARAM_FOCUS"},
        {kSsNameScope, 0, "NAME_SCOPE"},
        {kSsColumnEncryption, 0, "COLUMN_ENCRYPTION"},
    };

    for (const Case& c : cases) {
        SCOPED_TRACE(c.label);
        EXPECT_EQ(c.expected, GetStmtULen(c.attribute));
    }
}

// -------------------------------------------------------------------
// Variation 48 - the boolean vendor attributes take 0 and 1 and reject
// everything else with HY024. Unlike the standard attributes, this
// rejection comes from the driver rather than the Driver Manager, which
// does not know these identifiers and passes their values straight
// through.
// -------------------------------------------------------------------
TEST_F(AttributesTest, VendorBooleanAttributesRejectOutOfRangeValues) {
    const SQLINTEGER attributes[] = {kSsTextptrLogging, kSsHiddenColumns,
                                     kSsNobrowsetable, kSsRegionalize,
                                     kSsDeferPrepare};

    for (SQLINTEGER attribute : attributes) {
        SCOPED_TRACE(attribute);
        for (SQLULEN value : {SQLULEN{0}, SQLULEN{1}}) {
            EXPECT_EQ(SQL_SUCCESS, SetStmtULen(attribute, value));
            EXPECT_EQ(value, GetStmtULen(attribute));
        }

        EXPECT_EQ(SQL_ERROR, SetStmtULen(attribute, 2));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY024");
    }
}

// -------------------------------------------------------------------
// Variation 49 - CURSOR_OPTIONS is a three-bit mask rather than an
// enumeration, so the whole 0..7 range is legal. Treating it as a set of
// named constants would reject combinations a caller is entitled to pass.
// NAME_SCOPE is the neighbouring bounded range, capped at 3.
// -------------------------------------------------------------------
TEST_F(AttributesTest, VendorRangeAttributesAcceptTheirWholeRange) {
    for (SQLULEN value = 0; value <= 7; ++value) {
        SCOPED_TRACE(value);
        EXPECT_EQ(SQL_SUCCESS, SetStmtULen(kSsCursorOptions, value));
        EXPECT_EQ(value, GetStmtULen(kSsCursorOptions));
    }
    EXPECT_EQ(SQL_ERROR, SetStmtULen(kSsCursorOptions, 8));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY024");

    for (SQLULEN value = 0; value <= 3; ++value) {
        SCOPED_TRACE(value);
        EXPECT_EQ(SQL_SUCCESS, SetStmtULen(kSsNameScope, value));
        EXPECT_EQ(value, GetStmtULen(kSsNameScope));
    }
    EXPECT_EQ(SQL_ERROR, SetStmtULen(kSsNameScope, 4));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY024");
}

// -------------------------------------------------------------------
// Variation 50 - the query-notification timeout rejects 0, which is the
// opposite of the ODBC convention where 0 on a timeout means "no limit".
// A driver that copied that convention would silently accept a value
// msodbcsql refuses. A refused set also leaves the previous value alone:
// failing is not resetting.
// -------------------------------------------------------------------
TEST_F(AttributesTest, QueryNotificationTimeoutRejectsZeroAndKeepsItsValue) {
    EXPECT_EQ(SQL_ERROR, SetStmtULen(kSsQnTimeout, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY024");
    EXPECT_EQ(SQLULEN{432000}, GetStmtULen(kSsQnTimeout));

    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(kSsQnTimeout, 60));
    EXPECT_EQ(SQL_ERROR, SetStmtULen(kSsQnTimeout, 0));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY024");
    EXPECT_EQ(SQLULEN{60}, GetStmtULen(kSsQnTimeout));
}

// -------------------------------------------------------------------
// Variation 51 - PARAM_FOCUS and COLUMN_ENCRYPTION are recognized
// identifiers whose features are unavailable, so every value is refused
// with HY024 rather than the identifier being refused with HY092. The
// distinction matters: a caller probing for Always Encrypted must be able
// to tell "this driver knows the attribute but cannot honour it" from
// "this is not an attribute".
// -------------------------------------------------------------------
TEST_F(AttributesTest, VendorUnsupportedFeatureAttributesRefuseEveryValue) {
    for (SQLINTEGER attribute : {kSsParamFocus, kSsColumnEncryption}) {
        SCOPED_TRACE(attribute);
        for (SQLULEN value : {SQLULEN{0}, SQLULEN{1}, SQLULEN{2}}) {
            EXPECT_EQ(SQL_ERROR, SetStmtULen(attribute, value));
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY024");
        }
        EXPECT_EQ(SQLULEN{0}, GetStmtULen(attribute));
    }
}

// -------------------------------------------------------------------
// Variation 52 - CURRENT_COMMAND and NOCOUNT_STATUS are readable but not
// settable, and the refusal is HY092 rather than the HY024 of Variation
// 49. This is the vendor-band mirror of Variation 28: recognition is
// keyed by operation, so "not settable" reads as an unknown identifier
// for the set operation, not as a bad value.
// -------------------------------------------------------------------
TEST_F(AttributesTest, VendorGetOnlyAttributesAreNotSettable) {
    for (SQLINTEGER attribute : {kSsCurrentCommand, kSsNocountStatus}) {
        SCOPED_TRACE(attribute);
        EXPECT_EQ(SQL_ERROR, SetStmtULen(attribute, 0));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY092");
    }
}

// -------------------------------------------------------------------
// Variation 53 - CURRENT_COMMAND is the ordinal of the result set being
// processed, not a boolean. It starts at 0, reaches 1 on the first result
// set, advances with SQLMoreResults, holds once the batch is exhausted,
// and restarts at 1 on the next execution. Modelling it as a bare counter
// agrees on the first query and diverges on the second.
// -------------------------------------------------------------------
TEST_F(AttributesTest, CurrentCommandTracksTheResultSetOrdinal) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 1; SELECT 2; SELECT 3");

    EXPECT_EQ(SQLULEN{0}, GetStmtULen(kSsCurrentCommand)) << "fresh statement";

    for (int pass = 0; pass < 2; ++pass) {
        SCOPED_TRACE(pass);
        ASSERT_SQL_OK(
            SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, stmt_);

        for (SQLULEN expected = 1; expected <= 3; ++expected) {
            EXPECT_EQ(expected, GetStmtULen(kSsCurrentCommand));
            if (expected < 3) {
                ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
            }
        }

        // Exhausting the batch holds the last ordinal rather than clearing it,
        // and so does closing the cursor.
        EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
        EXPECT_EQ(SQLULEN{3}, GetStmtULen(kSsCurrentCommand));

        SQLCloseCursor(stmt_);
        EXPECT_EQ(SQLULEN{3}, GetStmtULen(kSsCurrentCommand))
            << "close does not reset";
    }
}

// -------------------------------------------------------------------
// Variation 54 - the two query-notification attributes are the only
// string-valued statement attributes. StringLength is a byte count on
// both legs, so an explicit 6 stores three characters, and a get always
// reports the full byte width even when the buffer could not hold it.
// -------------------------------------------------------------------
TEST_F(AttributesTest, QueryNotificationStringsFollowTheByteLengthContract) {
    SQLTCHAR buffer[32] = {};
    SQLINTEGER written = -1;

    // Both default to empty.
    for (SQLINTEGER attribute : {kSsQnMsgtext, kSsQnOptions}) {
        SCOPED_TRACE(attribute);
        written = -1;
        EXPECT_SQL_OK(SQLGetStmtAttr(stmt_, attribute, buffer, sizeof(buffer),
                                     &written),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(0, written);
    }

    SqlTString value = ODBCTestUtils::ToSqlTStr("abcdefghij");
    SQLTCHAR* msg = const_cast<SQLTCHAR*>(value.c_str());

    // A NUL-terminated set round-trips, and the two are independent.
    EXPECT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, kSsQnMsgtext, msg, SQL_NTS));
    written = -1;
    EXPECT_SQL_OK(
        SQLGetStmtAttr(stmt_, kSsQnMsgtext, buffer, sizeof(buffer), &written),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(20, written);

    written = -1;
    EXPECT_SQL_OK(
        SQLGetStmtAttr(stmt_, kSsQnOptions, buffer, sizeof(buffer), &written),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(0, written) << "options untouched by a msgtext set";

    // StringLength on the set path is bytes, so 6 stores three characters.
    EXPECT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, kSsQnMsgtext, msg, 6));
    written = -1;
    EXPECT_SQL_OK(
        SQLGetStmtAttr(stmt_, kSsQnMsgtext, buffer, sizeof(buffer), &written),
        SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(6, written);

    // A short buffer truncates with 01004 but still reports the full width, so
    // a caller can size a second call from the first one's answer. A null
    // pointer is the documented length-only query.
    EXPECT_EQ(SQL_SUCCESS, SQLSetStmtAttr(stmt_, kSsQnMsgtext, msg, SQL_NTS));
    written = -1;
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO,
              SQLGetStmtAttr(stmt_, kSsQnMsgtext, buffer, 10, &written));
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "01004");
    EXPECT_EQ(20, written);

    written = -1;
    EXPECT_EQ(SQL_SUCCESS,
              SQLGetStmtAttr(stmt_, kSsQnMsgtext, nullptr, 0, &written));
    EXPECT_EQ(20, written);
}

// -------------------------------------------------------------------
// Variation 55 - a successful integer get writes the value's width into
// StringLength, and a failed one leaves the caller's variable alone.
// Skipping the write on success hands back whatever was in that memory.
// -------------------------------------------------------------------
TEST_F(AttributesTest, IntegerGetsReportTheValueWidth) {
    const SQLINTEGER attributes[] = {
        SQL_ATTR_QUERY_TIMEOUT, SQL_ATTR_MAX_ROWS,       SQL_ATTR_NOSCAN,
        SQL_ATTR_CURSOR_TYPE,   SQL_ATTR_CONCURRENCY,    SQL_ATTR_ROW_ARRAY_SIZE,
        SQL_ATTR_METADATA_ID,   kSsDeferPrepare,         kSsCurrentCommand,
    };

    for (SQLINTEGER attribute : attributes) {
        SCOPED_TRACE(attribute);
        SQLULEN value = 0;
        SQLINTEGER written = -1;
        EXPECT_SQL_OK(
            SQLGetStmtAttr(stmt_, attribute, &value, sizeof(value), &written),
            SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(static_cast<SQLINTEGER>(sizeof(SQLULEN)), written);
    }

    // SQL_ATTR_ROW_NUMBER without an open cursor fails (Variation 41), and the
    // failure must not have written a length first.
    SQLULEN value = 0;
    SQLINTEGER written = -12345;
    EXPECT_EQ(SQL_ERROR, SQLGetStmtAttr(stmt_, kRowNumberAttr, &value,
                                        sizeof(value), &written));
    EXPECT_EQ(-12345, written);
}

// -------------------------------------------------------------------
// Variation 56 - CURRENT_COMMAND counts every statement in the batch,
// not just the row-returning ones. A batch that mixes SELECTs, DML and
// PRINT still reports 1, 2, 3. Advancing the ordinal only where a
// cursor opens agrees on an all-SELECT batch and stalls on every other
// shape, which is exactly the batch a stored procedure produces.
// -------------------------------------------------------------------
TEST_F(AttributesTest, CurrentCommandAdvancesThroughNonRowResults) {
    struct Batch {
        const char* label;
        const char* sql;
    };
    const Batch batches[] = {
        {"select-dml-select",
         "SELECT 1; DECLARE @t TABLE(i INT); INSERT INTO @t VALUES(1); SELECT 2"},
        {"pure-dml",
         "DECLARE @t TABLE(i INT); INSERT INTO @t VALUES(1); "
         "INSERT INTO @t VALUES(2); INSERT INTO @t VALUES(3)"},
        {"print-select-print", "PRINT 'a'; SELECT 1; PRINT 'b'"},
    };

    for (const Batch& batch : batches) {
        SCOPED_TRACE(batch.label);
        SqlTString sql = ODBCTestUtils::ToSqlTStr(batch.sql);
        SQLRETURN rc = SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()),
                                     SQL_NTS);
        ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

        for (SQLULEN expected = 1; expected <= 3; ++expected) {
            EXPECT_EQ(expected, GetStmtULen(kSsCurrentCommand))
                << "result " << expected;
            if (expected < 3) {
                ASSERT_SQL_OK(SQLMoreResults(stmt_), SQL_HANDLE_STMT, stmt_);
            }
        }
        EXPECT_EQ(SQL_NO_DATA, SQLMoreResults(stmt_));
        EXPECT_EQ(SQLULEN{3}, GetStmtULen(kSsCurrentCommand));
        SQLCloseCursor(stmt_);
    }
}

// -------------------------------------------------------------------
// Variation 57 - SQL_NTS is the only negative StringLength a character
// attribute accepts. Any other negative value is HY024 and leaves the
// stored string untouched; reading it as a terminated string instead
// would silently clear an attribute the caller never meant to write.
// Note the SQLSTATE differs from SQL_ATTR_CURRENT_CATALOG's HY090 -
// these are vendor attributes the driver validates itself.
// -------------------------------------------------------------------
TEST_F(AttributesTest, QueryNotificationStringsRejectBadNegativeLengths) {
    const SQLINTEGER attributes[] = {kSsQnMsgtext, kSsQnOptions};
    SqlTString seed = ODBCTestUtils::ToSqlTStr("SEED");
    SqlTString other = ODBCTestUtils::ToSqlTStr("REPLACED");

    for (SQLINTEGER attribute : attributes) {
        SCOPED_TRACE(attribute);
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, attribute,
                                     const_cast<SQLTCHAR*>(seed.c_str()),
                                     SQL_NTS),
                      SQL_HANDLE_STMT, stmt_);

        for (SQLINTEGER length : {-2, -5, -100}) {
            SCOPED_TRACE(length);
            EXPECT_EQ(SQL_ERROR,
                      SQLSetStmtAttr(stmt_, attribute,
                                     const_cast<SQLTCHAR*>(other.c_str()),
                                     length));
            EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY024");

            SQLTCHAR buffer[32] = {};
            SQLINTEGER written = -1;
            EXPECT_SQL_OK(SQLGetStmtAttr(stmt_, attribute, buffer,
                                         sizeof(buffer), &written),
                          SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ("SEED", ODBCTestUtils::ToNarrow(SqlTString(buffer)));
        }
    }
}

// -------------------------------------------------------------------
// Variation 58 - the query-notification timeout tops out at INT_MAX,
// not at the full SQLULEN width the pointer slot can carry. Values
// above it are HY024 and leave the previous timeout in place.
// -------------------------------------------------------------------
TEST_F(AttributesTest, QueryNotificationTimeoutCeilingIsIntMax) {
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, kSsQnTimeout,
                                 reinterpret_cast<SQLPOINTER>(
                                     static_cast<SQLULEN>(2147483647)),
                                 0),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQLULEN{2147483647}, GetStmtULen(kSsQnTimeout));

    const SQLULEN rejected[] = {2147483648ULL, 3000000000ULL, 4294967295ULL};
    for (SQLULEN value : rejected) {
        SCOPED_TRACE(value);
        EXPECT_EQ(SQL_ERROR,
                  SQLSetStmtAttr(stmt_, kSsQnTimeout,
                                 reinterpret_cast<SQLPOINTER>(value), 0));
        EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY024");
        EXPECT_EQ(SQLULEN{2147483647}, GetStmtULen(kSsQnTimeout))
            << "a rejected set must leave the previous value in place";
    }
}

// -------------------------------------------------------------------
// 59. MAX_ROWS truncates a rowset rather than rounding it.
//
// SQL_ATTR_MAX_ROWS was measured against single-row fetches, which leaves
// open whether the cap is a row budget or a rowset boundary. Measured on
// msodbcsql it is a budget: a cap of 5 under SQL_ATTR_ROW_ARRAY_SIZE = 4
// returns 4 rows and then a partial rowset of 1. A driver that stopped on
// the boundary instead would hand back either 8 rows or 4.
// -------------------------------------------------------------------
TEST_F(AttributesTest, MaxRowsTruncatesARowsetRatherThanRoundingIt) {
    constexpr SQLULEN kArraySize = 4;
    SQLINTEGER values[kArraySize] = {};
    SQLLEN indicators[kArraySize] = {};
    SQLULEN fetched = 0;

    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_ROW_ARRAY_SIZE, kArraySize));
    EXPECT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_ROWS_FETCHED_PTR, &fetched, 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLBindCol(stmt_, 1, SQL_C_SLONG, values, sizeof(values[0]),
                             indicators),
                  SQL_HANDLE_STMT, stmt_);

    const SqlTString sql = ODBCTestUtils::ToSqlTStr(
        "SELECT TOP 20 ROW_NUMBER() OVER (ORDER BY object_id) AS n "
        "FROM sys.objects");

    struct Case {
        SQLULEN cap;
        std::vector<SQLULEN> rowsets;
    };
    const Case cases[] = {
        {5, {4, 1}},   // the cap lands inside the second rowset
        {6, {4, 2}},   //
        {8, {4, 4}},   // and on a boundary it is simply two full rowsets
        {3, {3}},      // a cap below one rowset truncates the first
    };

    for (const Case& c : cases) {
        SCOPED_TRACE(c.cap);
        EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, c.cap));
        ASSERT_SQL_OK(
            SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, stmt_);

        SQLINTEGER next = 1;
        for (SQLULEN expected : c.rowsets) {
            fetched = 0;
            ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(expected, fetched);
            // The rows are the next ones in order, so a short rowset is a
            // truncation and not a skip.
            for (SQLULEN i = 0; i < expected; ++i) {
                EXPECT_EQ(next++, values[i]);
            }
        }
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
        SQLCloseCursor(stmt_);
    }

    SQLFreeStmt(stmt_, SQL_UNBIND);
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_MAX_ROWS, 0));
    EXPECT_EQ(SQL_SUCCESS, SetStmtULen(SQL_ATTR_ROW_ARRAY_SIZE, 1));
}
