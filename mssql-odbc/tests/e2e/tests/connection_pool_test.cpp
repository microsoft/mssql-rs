// Copyright (c) Microsoft Corporation. All rights reserved.
// connection_pool_test.cpp  –  E2E tests for the connection-pool check-in reset
// (SQL_ATTR_RESET_CONNECTION), SQL_ATTR_CONNECTION_DEAD liveness, and the
// same-physical-connection reuse a client-side pool (e.g. mssql-python) depends
// on.
//
// Structure mirrors transaction_test.cpp: a live SQL Server is required and the
// suite skips cleanly when none is configured. Every assertion here must also
// hold for msodbcsql 18, because the same binary runs against both drivers under
// `run_e2e.ps1 -CompareWithMsodbcsql`.
//
// The scenario reproduces one physical connection serving two consecutive
// borrowers: borrower A mutates session state (isolation, temp table, open
// transaction, current database), the pool checks the connection in and resets
// it, then borrower B — on the SAME @@SPID — must observe clean login defaults.

#include "odbc_test_fixture.h"

#include <string>

// SQL_ATTR_RESET_CONNECTION / SQL_RESET_CONNECTION_YES arrived in ODBC 3.8.
// Older Driver Manager headers may lack them.
#ifndef SQL_ATTR_RESET_CONNECTION
#define SQL_ATTR_RESET_CONNECTION 116
#endif
#ifndef SQL_RESET_CONNECTION_YES
#define SQL_RESET_CONNECTION_YES 1
#endif

// SQL_ATTR_CONNECTION_DEAD and its SQL_CD_* values.
#ifndef SQL_ATTR_CONNECTION_DEAD
#define SQL_ATTR_CONNECTION_DEAD 1209
#endif
#ifndef SQL_CD_TRUE
#define SQL_CD_TRUE 1
#endif
#ifndef SQL_CD_FALSE
#define SQL_CD_FALSE 0
#endif

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class ConnectionPoolLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    void TearDown() override {
        // Leave the connection idle and in autocommit so SQLDisconnect never
        // trips the "transaction still open" (25000) guard during teardown.
        if (dbc_ != SQL_NULL_HDBC) {
            SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK);
            SetAutocommit(dbc_, SQL_AUTOCOMMIT_ON);
        }
        ODBCTest::TearDown();
    }

    // --- Attribute helpers -------------------------------------------------

    static SQLRETURN SetAutocommit(SQLHDBC dbc, SQLUINTEGER mode) {
        return SQLSetConnectAttr(dbc, SQL_ATTR_AUTOCOMMIT,
                                 reinterpret_cast<SQLPOINTER>(static_cast<SQLULEN>(mode)),
                                 SQL_IS_UINTEGER);
    }

    static SQLRETURN SetIsolation(SQLHDBC dbc, SQLUINTEGER level) {
        return SQLSetConnectAttr(dbc, SQL_ATTR_TXN_ISOLATION,
                                 reinterpret_cast<SQLPOINTER>(static_cast<SQLULEN>(level)),
                                 SQL_IS_UINTEGER);
    }

    static SQLRETURN ResetConnection(SQLHDBC dbc) {
        return SQLSetConnectAttr(
            dbc, SQL_ATTR_RESET_CONNECTION,
            reinterpret_cast<SQLPOINTER>(static_cast<SQLULEN>(SQL_RESET_CONNECTION_YES)),
            SQL_IS_UINTEGER);
    }

    static SQLUINTEGER GetConnectionDead(SQLHDBC dbc) {
        SQLUINTEGER value = 0xDEAD;
        SQLRETURN rc =
            SQLGetConnectAttr(dbc, SQL_ATTR_CONNECTION_DEAD, &value, SQL_IS_UINTEGER, nullptr);
        EXPECT_SQL_OK(rc, SQL_HANDLE_DBC, dbc);
        return value;
    }

    // --- SQL helpers -------------------------------------------------------

    static SQLRETURN Run(SQLHSTMT hstmt, const std::string& sql) {
        SqlTString text = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(hstmt, const_cast<SQLTCHAR*>(text.c_str()), SQL_NTS);
    }

    void Exec(const std::string& sql) {
        SQLRETURN rc = Run(stmt_, sql);
        ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    }

    /// Execute a single-column, single-row integer query and return the value.
    SQLINTEGER Scalar(const std::string& sql) {
        SQLRETURN rc = Run(stmt_, sql);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        rc = SQLFetch(stmt_);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

        SQLINTEGER value = -1;
        SQLLEN indicator = 0;
        rc = SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), &indicator);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
        SQLCloseCursor(stmt_);
        return value;
    }

    SQLINTEGER Spid() { return Scalar("SELECT @@SPID"); }
    SQLINTEGER TranCount() { return Scalar("SELECT @@TRANCOUNT"); }
    SQLINTEGER DatabaseId() { return Scalar("SELECT DB_ID()"); }

    /// Server-side view of the session isolation level (1..5), which is what
    /// SET TRANSACTION ISOLATION LEVEL actually changed.
    SQLINTEGER ServerIsolation() {
        return Scalar(
            "SELECT CAST(transaction_isolation_level AS int) FROM sys.dm_exec_sessions "
            "WHERE session_id = @@SPID");
    }

    /// True if |name| (e.g. "#probe") resolves in tempdb for this session.
    bool TempTableExists(const std::string& name) {
        return Scalar("SELECT CASE WHEN OBJECT_ID('tempdb.." + name +
                      "') IS NULL THEN 0 ELSE 1 END") != 0;
    }

    /// Model the pool check-in + next-acquire the way mssql-python does: roll
    /// back the borrower's transaction and return to autocommit (Connection
    /// close), then arm the reset and re-apply READ COMMITTED (Connection reset
    /// on the next acquire — the isolation SET is deliberate because
    /// sp_reset_connection does not restore isolation, D9/#343).
    void CheckInAndReset() {
        ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
        ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_ON), SQL_HANDLE_DBC, dbc_);
        ASSERT_SQL_OK(ResetConnection(dbc_), SQL_HANDLE_DBC, dbc_);
        ASSERT_SQL_OK(SetIsolation(dbc_, SQL_TXN_READ_COMMITTED), SQL_HANDLE_DBC, dbc_);
    }
};

// The full check-in reset: borrower A raises isolation, creates a temp table,
// opens a transaction and switches database; after the pool resets the
// connection, borrower B on the SAME physical connection sees login defaults —
// no leaked temp table, no open transaction, isolation back to READ COMMITTED,
// and the original database. This is the pooling analogue of
// transaction_test.cpp's cross-connection checks.
TEST_F(ConnectionPoolLiveTest, ResetRestoresCleanStateForNextBorrower) {
    const SQLINTEGER spid_before = Spid();
    const SQLINTEGER login_db = DatabaseId();

    // Borrower A dirties the session.
    ASSERT_SQL_OK(SetIsolation(dbc_, SQL_TXN_SERIALIZABLE), SQL_HANDLE_DBC, dbc_);
    Exec("CREATE TABLE #pool_probe(i int)");
    Exec("INSERT INTO #pool_probe VALUES (1), (2)");
    ASSERT_TRUE(TempTableExists("#pool_probe"));
    Exec("USE master");
    EXPECT_EQ(4, ServerIsolation()) << "A raised isolation to SERIALIZABLE";
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("INSERT INTO #pool_probe VALUES (3)");
    EXPECT_GE(TranCount(), 1) << "A left a transaction open";

    // Pool check-in + next acquire.
    CheckInAndReset();

    // Borrower B, same physical connection, sees a clean session.
    EXPECT_EQ(spid_before, Spid()) << "reset must reuse the same physical connection";
    EXPECT_EQ(0, TranCount()) << "no transaction may leak across check-in";
    EXPECT_FALSE(TempTableExists("#pool_probe")) << "the temp table must not survive the reset";
    EXPECT_EQ(2, ServerIsolation()) << "isolation must return to READ COMMITTED";
    EXPECT_EQ(login_db, DatabaseId()) << "database must return to the login default";
}

// The reset itself keeps the connection usable: CONNECTION_DEAD reads FALSE on a
// healthy connection both before and after the reset, so a pool does not discard
// a perfectly good connection.
TEST_F(ConnectionPoolLiveTest, ConnectionDeadStaysFalseAcrossReset) {
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_CD_FALSE), GetConnectionDead(dbc_));
    ASSERT_SQL_OK(ResetConnection(dbc_), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_CD_FALSE), GetConnectionDead(dbc_))
        << "a successful reset leaves the connection alive and reusable";
    EXPECT_EQ(0, TranCount());
}

// mssql-python re-applies READ COMMITTED on every checkout because
// sp_reset_connection does not reset isolation (D9). A borrower that raised
// isolation via the attribute must see it restored after the reset+re-apply,
// and the same physical connection stays reusable across repeated cycles.
TEST_F(ConnectionPoolLiveTest, IsolationReturnsToReadCommittedEachCheckout) {
    const SQLINTEGER spid_before = Spid();

    for (int cycle = 0; cycle < 2; ++cycle) {
        ASSERT_SQL_OK(SetIsolation(dbc_, SQL_TXN_SERIALIZABLE), SQL_HANDLE_DBC, dbc_);
        EXPECT_EQ(4, ServerIsolation()) << "cycle " << cycle << ": raised to SERIALIZABLE";

        CheckInAndReset();

        EXPECT_EQ(2, ServerIsolation()) << "cycle " << cycle << ": back to READ COMMITTED";
        EXPECT_EQ(spid_before, Spid()) << "cycle " << cycle << ": same physical connection";
    }
}

// Parity-safe portion: both drivers must keep the connection usable across a pool
// reset for newly prepared statements. A statement prepared and executed before
// the reset works, and after the reset a freshly prepared statement (a new
// prepare, not a reuse of the old server-side handle) also works. This holds on
// both mssql-odbc and msodbcsql, so it runs on both legs with no skip.
TEST_F(ConnectionPoolLiveTest, PreparedStatementUsableAcrossReset) {
    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 42");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS), SQL_HANDLE_STMT,
                  stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(42, value);
    SQLCloseCursor(stmt_);

    CheckInAndReset();

    // Freshly prepare the statement again after the reset (a new prepare, not a
    // reuse of the old server-side handle): the connection stays usable on both
    // drivers.
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS), SQL_HANDLE_STMT,
                  stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    value = 0;
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(42, value) << "a freshly prepared statement must work after the pool reset";
    SQLCloseCursor(stmt_);
}

// Isolates ONLY the mssql-odbc-specific transparent-re-prepare divergence: after
// the reset, re-executing the SAME already-prepared handle without re-preparing
// must succeed. Stage 1 clears session-bound prepared-statement handles on the
// reset ack, so the driver transparently re-prepares against the fresh session
// rather than aliasing a stale or dropped handle. sp_reset_connection drops the
// server-side prepared handles, so msodbcsql blindly re-executes the dropped
// handle and fails with native error 8179 ("Could not find prepared statement
// with handle N"). SKIP_IF_COMPARING_MSODBCSQL() keeps the parity comparison
// honest (see transaction_test.cpp / get_data_test.cpp); the shared "usable
// across reset" behavior is covered by PreparedStatementUsableAcrossReset, which
// runs on both legs.
TEST_F(ConnectionPoolLiveTest, PreparedStatementSurvivesResetViaReprepare) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SqlTString sql = ODBCTestUtils::ToSqlTStr("SELECT 42");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS), SQL_HANDLE_STMT,
                  stmt_);

    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(42, value);
    SQLCloseCursor(stmt_);

    CheckInAndReset();

    // Re-execute the same prepared handle after the reset without re-preparing: it
    // must transparently re-prepare and return the correct result, not fail on a
    // dropped server-side handle.
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    value = 0;
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &value, sizeof(value), nullptr), SQL_HANDLE_STMT,
                  stmt_);
    EXPECT_EQ(42, value) << "the statement must survive the reset via transparent re-prepare";
    SQLCloseCursor(stmt_);
}

// Value validation is enforced at the driver: only SQL_RESET_CONNECTION_YES is
// accepted; any other value is rejected with HY024 and leaves the connection
// untouched (D7). No server round trip is needed, so this holds without a live
// connection too, but it runs here alongside the rest of the pool surface.
TEST_F(ConnectionPoolLiveTest, ResetRejectsNonYesValue) {
    EXPECT_SQL_ERROR(SQLSetConnectAttr(dbc_, SQL_ATTR_RESET_CONNECTION,
                                       reinterpret_cast<SQLPOINTER>(static_cast<SQLULEN>(2)),
                                       SQL_IS_UINTEGER));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");
    // The connection is still healthy and usable after the rejected set.
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_CD_FALSE), GetConnectionDead(dbc_));
}
