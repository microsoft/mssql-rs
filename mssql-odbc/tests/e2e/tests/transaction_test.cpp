// Copyright (c) Microsoft Corporation. All rights reserved.
// transaction_test.cpp  –  E2E tests for SQLEndTran, SQL_ATTR_AUTOCOMMIT and
// SQL_ATTR_TXN_ISOLATION.
//
// Every assertion here must hold for msodbcsql 18 as well, because the same
// binary runs against both drivers under `run_e2e.ps1 -CompareWithMsodbcsql`.
// Behaviour that is deliberately mssql-odbc-only is guarded with
// SKIP_IF_COMPARING_MSODBCSQL().
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <chrono>
#include <functional>
#include <string>

// SQL_TXN_SS_SNAPSHOT lives in msodbcsql.h, which these tests do not include.
#ifndef SQL_TXN_SS_SNAPSHOT
#define SQL_TXN_SS_SNAPSHOT 0x00000020L
#endif

// SQL Server's vendor isolation attribute, also from msodbcsql.h. This is the
// only route to SNAPSHOT: the Driver Manager screens SQL_ATTR_TXN_ISOLATION
// against the four standard ODBC bits and rejects anything else with HY024
// before the driver is called.
#ifndef SQL_COPT_SS_TXN_ISOLATION
#define SQL_COPT_SS_TXN_ISOLATION 1227
#endif

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

// The Driver Manager rejects a null connection handle before the driver runs.
TEST(TransactionTest, EndTranNullHandle) {
    SQLRETURN rc = SQLEndTran(SQL_HANDLE_DBC, SQL_NULL_HDBC, SQL_COMMIT);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// SQLGetFunctions must advertise SQLEndTran as implemented.
TEST_F(ODBCTest, EndTranIsReportedSupported) {
    if (!ODBCTestConfig::Instance().HasConnection()) {
        GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
    }
    Connect();

    SQLUSMALLINT supported = SQL_FALSE;
    SQLRETURN rc = SQLGetFunctions(dbc_, SQL_API_SQLENDTRAN, &supported);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_TRUE, supported);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class TransactionLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    void TearDown() override {
        // Leave the connection in autocommit so SQLDisconnect never trips the
        // "transaction still open" (25000) guard during teardown.
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

    static SQLUINTEGER GetAutocommit(SQLHDBC dbc) {
        SQLUINTEGER value = 0xDEAD;
        SQLRETURN rc = SQLGetConnectAttr(dbc, SQL_ATTR_AUTOCOMMIT, &value, SQL_IS_UINTEGER, nullptr);
        EXPECT_SQL_OK(rc, SQL_HANDLE_DBC, dbc);
        return value;
    }

    static SQLRETURN SetIsolation(SQLHDBC dbc, SQLUINTEGER level) {
        return SQLSetConnectAttr(dbc, SQL_ATTR_TXN_ISOLATION,
                                 reinterpret_cast<SQLPOINTER>(static_cast<SQLULEN>(level)),
                                 SQL_IS_UINTEGER);
    }

    static SQLUINTEGER GetIsolation(SQLHDBC dbc) {
        SQLUINTEGER value = 0xDEAD;
        SQLRETURN rc =
            SQLGetConnectAttr(dbc, SQL_ATTR_TXN_ISOLATION, &value, SQL_IS_UINTEGER, nullptr);
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

    /// Execute a single-column, single-row query and return the value.
    SQLINTEGER Scalar(SQLHSTMT hstmt, const std::string& sql) {
        SQLRETURN rc = Run(hstmt, sql);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, hstmt);
        rc = SQLFetch(hstmt);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, hstmt);

        SQLINTEGER value = -1;
        SQLLEN indicator = 0;
        rc = SQLGetData(hstmt, 1, SQL_C_SLONG, &value, sizeof(value), &indicator);
        EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, hstmt);
        SQLCloseCursor(hstmt);
        return value;
    }

    SQLINTEGER Scalar(const std::string& sql) { return Scalar(stmt_, sql); }

    SQLINTEGER RowCountOf(const std::string& table) {
        return Scalar("SELECT COUNT(*) FROM " + table);
    }

    /// A global temp table name unique to this connection, readable from a
    /// second connection so committed data can be verified independently.
    std::string GlobalTempTable(const std::string& suffix) {
        return "##txn_" + suffix + "_" + std::to_string(Scalar("SELECT @@SPID"));
    }

    /// Server-side view of the session isolation level (1..5), which is what
    /// SET TRANSACTION ISOLATION LEVEL actually changed.
    SQLINTEGER ServerIsolation() {
        return Scalar(
            "SELECT CAST(transaction_isolation_level AS int) FROM sys.dm_exec_sessions "
            "WHERE session_id = @@SPID");
    }
};

// -------------------------------------------------------------------
// Autocommit attribute
// -------------------------------------------------------------------

// ODBC (and msodbcsql) open connections in autocommit; mssql-python turns it
// off explicitly after connecting rather than relying on a driver default.
TEST_F(TransactionLiveTest, AutocommitDefaultsToOn) {
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_AUTOCOMMIT_ON), GetAutocommit(dbc_));
}

TEST_F(TransactionLiveTest, AutocommitRoundTripsBothWays) {
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_AUTOCOMMIT_OFF), GetAutocommit(dbc_));

    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_ON), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_AUTOCOMMIT_ON), GetAutocommit(dbc_));
}

// Anything other than the two defined modes is rejected.
TEST_F(TransactionLiveTest, AutocommitRejectsInvalidValue) {
    EXPECT_SQL_ERROR(SetAutocommit(dbc_, 7));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_AUTOCOMMIT_ON), GetAutocommit(dbc_));
}

// -------------------------------------------------------------------
// Commit / rollback
// -------------------------------------------------------------------

TEST_F(TransactionLiveTest, CommitPersistsInsert) {
    Exec("CREATE TABLE #commit_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);

    Exec("INSERT INTO #commit_t VALUES (1), (2)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);

    EXPECT_EQ(2, RowCountOf("#commit_t"));
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(2, RowCountOf("#commit_t")) << "committed rows must survive a later rollback";
}

TEST_F(TransactionLiveTest, RollbackDiscardsInsert) {
    Exec("CREATE TABLE #rollback_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);

    Exec("INSERT INTO #rollback_t VALUES (1), (2), (3)");
    EXPECT_EQ(3, RowCountOf("#rollback_t")) << "uncommitted rows are visible to their own session";

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(0, RowCountOf("#rollback_t"));
}

// The batch-processing shape mssql-python's test suite uses: commit a batch,
// roll the next one back, commit a third, and expect only the committed batches
// to survive. It read back zero rows because the parameterized INSERTs — not
// the transaction handling — failed with 24000 from the second execute onward,
// so every batch ended in the caller's error path (AB#47531).
TEST_F(TransactionLiveTest, CommitRollbackCommitKeepsOnlyCommittedBatches) {
    Exec("CREATE TABLE #batch_t(id int, batch int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);

    SqlTString insert = ODBCTestUtils::ToSqlTStr("INSERT INTO #batch_t VALUES (?, ?)");
    for (SQLINTEGER batch = 0; batch < 3; ++batch) {
        const bool rollback = (batch == 1);
        for (SQLINTEGER i = 0; i < 5; ++i) {
            SCOPED_TRACE("batch " + std::to_string(batch) + " row " + std::to_string(i));
            SQLINTEGER id = batch * 5 + i + 1;
            SQLINTEGER batch_value = batch;
            ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0,
                                           0, &id, 0, nullptr),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLBindParameter(stmt_, 2, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0,
                                           0, &batch_value, 0, nullptr),
                          SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(insert.c_str()), SQL_NTS),
                          SQL_HANDLE_STMT, stmt_);
        }
        ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, rollback ? SQL_ROLLBACK : SQL_COMMIT),
                      SQL_HANDLE_DBC, dbc_);
    }
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_RESET_PARAMS), SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(10, RowCountOf("#batch_t")) << "the two committed batches must both survive";
    EXPECT_EQ(5, Scalar("SELECT COUNT(*) FROM #batch_t WHERE batch = 0"));
    EXPECT_EQ(0, Scalar("SELECT COUNT(*) FROM #batch_t WHERE batch = 1"));
    EXPECT_EQ(5, Scalar("SELECT COUNT(*) FROM #batch_t WHERE batch = 2"));
}

// A committed row is visible to a completely separate connection, proving the
// commit reached the server rather than only clearing driver state.
TEST_F(TransactionLiveTest, CommittedRowIsVisibleToAnotherConnection) {
    const std::string table = GlobalTempTable("visible");
    Exec("CREATE TABLE " + table + "(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("INSERT INTO " + table + " VALUES (42)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);

    SQLHDBC reader = SQL_NULL_HDBC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &reader), SQL_HANDLE_ENV, env_);
    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;
    ASSERT_SQL_OK(SQLDriverConnect(reader, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                                   static_cast<SQLSMALLINT>(connstr.size()), outStr,
                                   static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLTCHAR)),
                                   &outLen, SQL_DRIVER_NOPROMPT),
                  SQL_HANDLE_DBC, reader);

    SQLHSTMT reader_stmt = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, reader, &reader_stmt), SQL_HANDLE_DBC, reader);
    EXPECT_EQ(42, Scalar(reader_stmt, "SELECT i FROM " + table));

    SQLFreeHandle(SQL_HANDLE_STMT, reader_stmt);
    SQLDisconnect(reader);
    SQLFreeHandle(SQL_HANDLE_DBC, reader);
}

// With autocommit on, each statement stands alone: a later rollback is a no-op.
TEST_F(TransactionLiveTest, AutocommitInsertIsImmediatelyDurable) {
    Exec("CREATE TABLE #auto_t(i int)");
    Exec("INSERT INTO #auto_t VALUES (1)");

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(1, RowCountOf("#auto_t"));
}

// msodbcsql returns plain SQL_SUCCESS when no transaction was ever started —
// no warning, no error (`CommitAbortTran`, sqlctran.cpp).
TEST_F(TransactionLiveTest, EndTranWithoutTransactionSucceeds) {
    EXPECT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);
    EXPECT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);

    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    EXPECT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);
}

// A completion type outside {SQL_COMMIT, SQL_ROLLBACK} is invalid. The Windows
// Driver Manager screens this before the driver is reached, so both drivers
// answer identically.
TEST_F(TransactionLiveTest, EndTranRejectsInvalidCompletionType) {
    SQLRETURN rc = SQLEndTran(SQL_HANDLE_DBC, dbc_, 42);
    EXPECT_EQ(SQL_ERROR, rc);
}

// Committing through the environment handle reaches every connection it owns.
TEST_F(TransactionLiveTest, EndTranOnEnvironmentFansOutToConnections) {
    Exec("CREATE TABLE #env_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("INSERT INTO #env_t VALUES (1), (2)");

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_ENV, env_, SQL_ROLLBACK), SQL_HANDLE_ENV, env_);
    EXPECT_EQ(0, RowCountOf("#env_t"));

    Exec("INSERT INTO #env_t VALUES (3)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_ENV, env_, SQL_COMMIT), SQL_HANDLE_ENV, env_);
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(1, RowCountOf("#env_t"));
}

// A connection allocated but never connected takes no part in an
// environment-wide transaction. SQLEndTran: "Connections that are not active do
// not affect the transaction."
TEST_F(TransactionLiveTest, EndTranOnEnvironmentSkipsUnconnectedConnections) {
    SQLHDBC spare = SQL_NULL_HDBC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &spare), SQL_HANDLE_ENV, env_);
    ASSERT_NE(nullptr, spare);

    Exec("CREATE TABLE #env_skip_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("INSERT INTO #env_skip_t VALUES (1)");

    // The spare connection must be skipped rather than failing the fan-out.
    EXPECT_SQL_OK(SQLEndTran(SQL_HANDLE_ENV, env_, SQL_COMMIT), SQL_HANDLE_ENV, env_);
    EXPECT_EQ(1, RowCountOf("#env_skip_t"));

    SQLFreeHandle(SQL_HANDLE_DBC, spare);
}

// SQLEndTran sweeps the connection's statements through the equivalent of
// SQLFreeStmt(SQL_CLOSE), and that discards each statement's diagnostic records
// — including on a statement that failed and so never opened a cursor. Both
// drivers behave identically here; see the unit test
// `txn::tests::closing_cursors_clears_statement_diagnostics`.
TEST_F(TransactionLiveTest, EndTranClearsStatementDiagnostics) {
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);

    SQLRETURN rc = Run(stmt_, "SELECT * FROM this_table_does_not_exist_xyz");
    ASSERT_EQ(SQL_ERROR, rc) << "the statement was expected to fail";

    // The record is readable right up until the rollback.
    SQLTCHAR state[6] = {};
    SQLINTEGER native = 0;
    SQLTCHAR message[1024] = {};
    SQLSMALLINT length = 0;
    rc = SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state, &native, message,
                       static_cast<SQLSMALLINT>(std::size(message)), &length);
    ASSERT_NE(SQL_NO_DATA, rc) << "the failed statement should have posted a diagnostic record";

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);

    rc = SQLGetDiagRec(SQL_HANDLE_STMT, stmt_, 1, state, &native, message,
                       static_cast<SQLSMALLINT>(std::size(message)), &length);
    EXPECT_EQ(SQL_NO_DATA, rc)
        << "the cursor sweep in SQLEndTran must clear the statement's diagnostic records";
}

// The vendor isolation attribute is accepted and reads back, including SNAPSHOT
// — the level no application can reach through SQL_ATTR_TXN_ISOLATION.
TEST_F(TransactionLiveTest, VendorIsolationAttributeCarriesSnapshot) {
    SQLRETURN rc = SQLSetConnectAttr(
        dbc_, SQL_COPT_SS_TXN_ISOLATION,
        reinterpret_cast<SQLPOINTER>(static_cast<SQLULEN>(SQL_TXN_SS_SNAPSHOT)), SQL_IS_UINTEGER);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);

    SQLUINTEGER value = 0xDEAD;
    rc = SQLGetConnectAttr(dbc_, SQL_COPT_SS_TXN_ISOLATION, &value, SQL_IS_UINTEGER, nullptr);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_SS_SNAPSHOT), value);

    // Restore the default so teardown and any later test start clean.
    SQLSetConnectAttr(dbc_, SQL_COPT_SS_TXN_ISOLATION,
                      reinterpret_cast<SQLPOINTER>(static_cast<SQLULEN>(SQL_TXN_READ_COMMITTED)),
                      SQL_IS_UINTEGER);
}

// Selecting the level already in effect is accepted and changes nothing.
TEST_F(TransactionLiveTest, SettingCurrentIsolationLevelAgainSucceeds) {
    ASSERT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_READ_COMMITTED), GetIsolation(dbc_));
    ASSERT_SQL_OK(SetIsolation(dbc_, SQL_TXN_READ_COMMITTED), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_READ_COMMITTED), GetIsolation(dbc_));
}

// One transaction spans however many statements the application runs.
TEST_F(TransactionLiveTest, TransactionSpansMultipleStatements) {
    Exec("CREATE TABLE #multi_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);

    Exec("INSERT INTO #multi_t VALUES (1)");
    Exec("INSERT INTO #multi_t VALUES (2)");
    Exec("UPDATE #multi_t SET i = i + 10");
    EXPECT_EQ(2, RowCountOf("#multi_t"));

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(0, RowCountOf("#multi_t"));
}

// SQL_CURSOR_COMMIT_BEHAVIOR is SQL_CB_CLOSE, so an open cursor does not
// survive SQLEndTran. The statement must still be reusable afterwards.
TEST_F(TransactionLiveTest, CommitClosesOpenCursors) {
    Exec("CREATE TABLE #cursor_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("INSERT INTO #cursor_t VALUES (1), (2), (3)");

    ASSERT_SQL_OK(Run(stmt_, "SELECT i FROM #cursor_t ORDER BY i"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);

    // The cursor is gone on both drivers, and the insert survived the commit.
    EXPECT_EQ(SQL_ERROR, SQLFetch(stmt_));
    // SQL_CLOSE is a no-op when no cursor is open, unlike SQLCloseCursor; the
    // two drivers disagree about whether one still is (see
    // StatementIsReusableAfterCommitWithoutClosingCursor).
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(3, RowCountOf("#cursor_t"));
}

// msodbcsql answers SQL_CB_PRESERVE to the Driver Manager's first
// SQL_CURSOR_COMMIT_BEHAVIOR query (sqlcinfo.cpp), so the DM keeps a
// cursor-open state across the commit and rejects the next statement with
// 24000 until the application calls SQLCloseCursor. mssql-odbc answers
// truthfully (SQL_CB_CLOSE, non-goal N11), so no explicit close is needed.
TEST_F(TransactionLiveTest, StatementIsReusableAfterCommitWithoutClosingCursor) {
    SKIP_IF_COMPARING_MSODBCSQL();

    Exec("CREATE TABLE #reuse_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("INSERT INTO #reuse_t VALUES (1), (2), (3)");

    ASSERT_SQL_OK(Run(stmt_, "SELECT i FROM #reuse_t ORDER BY i"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);

    EXPECT_EQ(3, RowCountOf("#reuse_t"));
}

// A successful SQLEndTran must leave every statement without a cursor, so the
// connection is free for other work the moment it returns. Skipping the sweep
// left the row stream open and the connection claimed behind the Driver
// Manager's back, locking out every other statement.
//
// This is the reachable half of the sweep contract. The no-transaction-started
// half cannot be driven from here: in manual-commit mode every cursor-opening
// entry point (SQLExecute, SQLExecDirect, SQLGetTypeInfo) starts a transaction
// first, and in autocommit mode the Driver Manager answers SQLEndTran itself
// without ever calling the driver. That path is covered by the unit test
// txn::tests::end_tran_sweeps_cursors_even_with_no_transaction_started, which
// calls end_transaction directly.
TEST_F(TransactionLiveTest, EndTranClosesCursorsAndFreesTheConnection) {
    Exec("CREATE TABLE #ac_close_t(i int)");
    Exec("INSERT INTO #ac_close_t VALUES (1), (2), (3)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);

    // Leaves a cursor open mid-stream, so the connection is claimed.
    ASSERT_SQL_OK(Run(stmt_, "SELECT i FROM #ac_close_t ORDER BY i"), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);

    // The sweep released the row stream, so the connection is free for other work.
    SQLHSTMT other = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &other), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(3, Scalar(other, "SELECT COUNT(*) FROM #ac_close_t"));
    SQLFreeHandle(SQL_HANDLE_STMT, other);
}

// Switching autocommit back on commits work already done under manual commit
// and reports it with a 01000 informational record.
TEST_F(TransactionLiveTest, SwitchingAutocommitOnCommitsOpenWork) {
    Exec("CREATE TABLE #switch_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("INSERT INTO #switch_t VALUES (1), (2)");

    SQLRETURN rc = SetAutocommit(dbc_, SQL_AUTOCOMMIT_ON);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_SUCCESS_WITH_INFO, rc) << "the implicit commit must be reported";
    EXPECT_TRUE(ODBCTestUtils::HasDiagState(SQL_HANDLE_DBC, dbc_, "01000"));

    EXPECT_EQ(2, RowCountOf("#switch_t"));
}

// Setting the same mode twice is a no-op: no implicit commit, no warning.
TEST_F(TransactionLiveTest, SettingAutocommitToItsCurrentValueIsANoOp) {
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("CREATE TABLE #noop_t(i int)");
    Exec("INSERT INTO #noop_t VALUES (1)");

    EXPECT_EQ(SQL_SUCCESS, SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF));

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    // The CREATE was inside the transaction too, so the whole table is gone.
    EXPECT_SQL_ERROR(Run(stmt_, "SELECT COUNT(*) FROM #noop_t"));
}

// -------------------------------------------------------------------
// Isolation level
// -------------------------------------------------------------------

TEST_F(TransactionLiveTest, IsolationDefaultsToReadCommitted) {
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_READ_COMMITTED), GetIsolation(dbc_));
    EXPECT_EQ(2, ServerIsolation()) << "sys.dm_exec_sessions reports READ COMMITTED as 2";
}

TEST_F(TransactionLiveTest, AllIsolationLevelsRoundTrip) {
    // SQL_TXN_SS_SNAPSHOT is deliberately absent: the Windows Driver Manager
    // validates SQL_ATTR_TXN_ISOLATION against the four ODBC-standard levels and
    // rejects anything else before the driver is called — see
    // SnapshotIsolationIsRejectedByTheDriverManager below. The driver's own
    // handling of SNAPSHOT is covered by the unit tests in src/api/txn.rs and
    // src/api/set_connect_attr.rs, which call it without a Driver Manager.
    for (SQLUINTEGER level : {static_cast<SQLUINTEGER>(SQL_TXN_READ_UNCOMMITTED),
                              static_cast<SQLUINTEGER>(SQL_TXN_READ_COMMITTED),
                              static_cast<SQLUINTEGER>(SQL_TXN_REPEATABLE_READ),
                              static_cast<SQLUINTEGER>(SQL_TXN_SERIALIZABLE)}) {
        ASSERT_SQL_OK(SetIsolation(dbc_, level), SQL_HANDLE_DBC, dbc_);
        EXPECT_EQ(level, GetIsolation(dbc_)) << "level 0x" << std::hex << level;
    }
    ASSERT_SQL_OK(SetIsolation(dbc_, SQL_TXN_READ_COMMITTED), SQL_HANDLE_DBC, dbc_);
}

// Both drivers advertise SNAPSHOT through SQL_TXN_ISOLATION_OPTION, but the
// Windows Driver Manager only knows the four ODBC-standard levels and screens
// the attribute itself, so no driver ever sees the value.
TEST_F(TransactionLiveTest, SnapshotIsolationIsRejectedByTheDriverManager) {
    EXPECT_SQL_ERROR(SetIsolation(dbc_, SQL_TXN_SS_SNAPSHOT));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_READ_COMMITTED), GetIsolation(dbc_));
}

// The attribute must actually change the session, not just driver state.
// SNAPSHOT is excluded: it needs ALLOW_SNAPSHOT_ISOLATION on the database.
TEST_F(TransactionLiveTest, IsolationIsAppliedOnTheServer) {
    const std::pair<SQLUINTEGER, SQLINTEGER> levels[] = {
        {static_cast<SQLUINTEGER>(SQL_TXN_READ_UNCOMMITTED), 1},
        {static_cast<SQLUINTEGER>(SQL_TXN_READ_COMMITTED), 2},
        {static_cast<SQLUINTEGER>(SQL_TXN_REPEATABLE_READ), 3},
        {static_cast<SQLUINTEGER>(SQL_TXN_SERIALIZABLE), 4},
    };
    for (const auto& [odbc_level, server_level] : levels) {
        ASSERT_SQL_OK(SetIsolation(dbc_, odbc_level), SQL_HANDLE_DBC, dbc_);
        EXPECT_EQ(server_level, ServerIsolation()) << "level 0x" << std::hex << odbc_level;
    }
    ASSERT_SQL_OK(SetIsolation(dbc_, SQL_TXN_READ_COMMITTED), SQL_HANDLE_DBC, dbc_);
}

// A server-side lock timeout is a statement error, not a client query timeout.
// Execution must consume the ERROR/DONE response and return it instead of
// waiting indefinitely for another token (AB#47771).
//
// Both execution paths are covered: mssql-python's cursor.execute() defaults to
// use_prepare=True and so drives SQLPrepare + SQLExecute, not SQLExecDirect
// (AB#47510). The fix lives in the shared finish_execute, but only a test per
// path can prove neither branch regresses.
class BlockedSelectTest : public TransactionLiveTest {
  protected:
    // Holds an exclusive row lock on |table| from dbc_, then runs a blocked
    // SELECT on a second connection via |execute_blocked| and asserts it
    // reports native error 1222 rather than hanging or succeeding.
    void RunBlockedSelect(const std::string& suffix,
                          const std::function<SQLRETURN(SQLHSTMT, const std::string&)>&
                              execute_blocked) {
        const std::string table = GlobalTempTable(suffix);
        Exec("CREATE TABLE " + table + "(id int PRIMARY KEY, value int)");
        Exec("INSERT INTO " + table + " VALUES (1, 0)");

        ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
        Exec("UPDATE " + table + " SET value = 1 WHERE id = 1");

        SQLHDBC reader = SQL_NULL_HDBC;
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &reader), SQL_HANDLE_ENV, env_);
        SqlTString connstr = ODBCTestUtils::BuildConnectionString();
        SQLTCHAR out_str[1024] = {};
        SQLSMALLINT out_len = 0;
        ASSERT_SQL_OK(SQLDriverConnect(reader, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                                       static_cast<SQLSMALLINT>(connstr.size()), out_str,
                                       static_cast<SQLSMALLINT>(std::size(out_str)), &out_len,
                                       SQL_DRIVER_NOPROMPT),
                      SQL_HANDLE_DBC, reader);

        SQLHSTMT reader_stmt = SQL_NULL_HSTMT;
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, reader, &reader_stmt), SQL_HANDLE_DBC,
                      reader);
        // Bounds the wait if execution regresses to waiting for a token the
        // server will never send. This is a real deadline only once the
        // query-timeout enforcement of AB#46385 (#442) is in; this PR merges
        // after it.
        ASSERT_SQL_OK(SQLSetStmtAttr(reader_stmt, SQL_ATTR_QUERY_TIMEOUT,
                                     reinterpret_cast<SQLPOINTER>(static_cast<SQLULEN>(10)), 0),
                      SQL_HANDLE_STMT, reader_stmt);
        ASSERT_SQL_OK(Run(reader_stmt, "SET LOCK_TIMEOUT 1000"), SQL_HANDLE_STMT, reader_stmt);

        const auto start = std::chrono::steady_clock::now();
        const SQLRETURN rc = execute_blocked(reader_stmt, table);
        const auto elapsed = std::chrono::steady_clock::now() - start;

        EXPECT_EQ(SQL_ERROR, rc) << "the lock timeout must surface from execution, not a later "
                                    "SQLFetch";
        SQLTCHAR state[6] = {};
        SQLINTEGER native = 0;
        SQLTCHAR message[1024] = {};
        SQLSMALLINT length = 0;
        ASSERT_SQL_OK(SQLGetDiagRec(SQL_HANDLE_STMT, reader_stmt, 1, state, &native, message,
                                    static_cast<SQLSMALLINT>(std::size(message)), &length),
                      SQL_HANDLE_STMT, reader_stmt);
        EXPECT_EQ(1222, native);
        EXPECT_LT(elapsed, std::chrono::seconds(5));

        ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
        SQLFreeHandle(SQL_HANDLE_STMT, reader_stmt);
        SQLDisconnect(reader);
        SQLFreeHandle(SQL_HANDLE_DBC, reader);
    }
};

TEST_F(BlockedSelectTest, ExecDirectReturnsServerLockTimeout) {
    RunBlockedSelect("lock_timeout", [](SQLHSTMT hstmt, const std::string& table) {
        return Run(hstmt, "SELECT value FROM " + table + " WHERE id = 1");
    });
}

// The path mssql-python actually uses. SQLPrepare materializes the statement
// (its COLMETADATA arrives before the row), so the ERROR token trails metadata
// on SQLExecute exactly as it does on SQLExecDirect.
TEST_F(BlockedSelectTest, PreparedExecuteReturnsServerLockTimeout) {
    RunBlockedSelect("lock_timeout_prep", [](SQLHSTMT hstmt, const std::string& table) {
        SqlTString sql =
            ODBCTestUtils::ToSqlTStr("SELECT value FROM " + table + " WHERE id = 1");
        SQLRETURN prepare_rc =
            SQLPrepare(hstmt, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
        EXPECT_SQL_OK(prepare_rc, SQL_HANDLE_STMT, hstmt);
        return SQLExecute(hstmt);
    });
}

// The isolation level survives commit and rollback — it is a session setting.
TEST_F(TransactionLiveTest, IsolationSurvivesEndTran) {
    ASSERT_SQL_OK(SetIsolation(dbc_, SQL_TXN_SERIALIZABLE), SQL_HANDLE_DBC, dbc_);
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("CREATE TABLE #iso_t(i int)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);

    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_SERIALIZABLE), GetIsolation(dbc_));
    EXPECT_EQ(4, ServerIsolation());
}

// A value that is not one of the four ODBC-standard levels never reaches the
// driver: the Windows Driver Manager screens SQL_ATTR_TXN_ISOLATION and answers
// HY024 itself, identically for both drivers. The driver's own response —
// HYC00, matching msodbcsql's SetTxnIsolation in sqlcmisc.cpp — is asserted by
// the unit tests in src/api/set_connect_attr.rs, which bypass the DM.
TEST_F(TransactionLiveTest, IsolationRejectsUnsupportedLevel) {
    EXPECT_SQL_ERROR(SetIsolation(dbc_, 0x10));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_READ_COMMITTED), GetIsolation(dbc_));
}

// A bitmask of two valid levels is still not a valid level.
TEST_F(TransactionLiveTest, IsolationRejectsCombinedLevels) {
    EXPECT_SQL_ERROR(SetIsolation(dbc_, SQL_TXN_READ_COMMITTED | SQL_TXN_SERIALIZABLE));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY024");
}

// Changing isolation once a transaction has started is a sequencing error.
TEST_F(TransactionLiveTest, IsolationCannotChangeInsideATransaction) {
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("CREATE TABLE #iso_open_t(i int)");

    EXPECT_SQL_ERROR(SetIsolation(dbc_, SQL_TXN_SERIALIZABLE));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc_, "HY011");

    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    EXPECT_SQL_OK(SetIsolation(dbc_, SQL_TXN_SERIALIZABLE), SQL_HANDLE_DBC, dbc_);
    ASSERT_SQL_OK(SetIsolation(dbc_, SQL_TXN_READ_COMMITTED), SQL_HANDLE_DBC, dbc_);
}

// Attributes set before SQLDriverConnect are held and applied at connect time.
TEST_F(TransactionLiveTest, PreConnectAttributesAreAppliedOnConnect) {
    SQLHDBC dbc = SQL_NULL_HDBC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc), SQL_HANDLE_ENV, env_);

    ASSERT_SQL_OK(SetIsolation(dbc, SQL_TXN_SERIALIZABLE), SQL_HANDLE_DBC, dbc);
    ASSERT_SQL_OK(SetAutocommit(dbc, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc);

    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;
    ASSERT_SQL_OK(SQLDriverConnect(dbc, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                                   static_cast<SQLSMALLINT>(connstr.size()), outStr,
                                   static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLTCHAR)),
                                   &outLen, SQL_DRIVER_NOPROMPT),
                  SQL_HANDLE_DBC, dbc);

    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_SERIALIZABLE), GetIsolation(dbc));
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_AUTOCOMMIT_OFF), GetAutocommit(dbc));

    SQLHSTMT hstmt = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &hstmt), SQL_HANDLE_DBC, dbc);
    EXPECT_EQ(4, Scalar(hstmt, "SELECT CAST(transaction_isolation_level AS int) "
                               "FROM sys.dm_exec_sessions WHERE session_id = @@SPID"));

    SQLFreeHandle(SQL_HANDLE_STMT, hstmt);
    SQLEndTran(SQL_HANDLE_DBC, dbc, SQL_ROLLBACK);
    SQLDisconnect(dbc);
    SQLFreeHandle(SQL_HANDLE_DBC, dbc);
}

// -------------------------------------------------------------------
// Disconnect
// -------------------------------------------------------------------

// Disconnecting with user work still pending is refused with 25000 rather than
// silently discarding it (`SQLDisconnect`, sqlcconn.cpp). Applications are
// expected to commit or roll back first — which is exactly what
// mssql-python's Connection.close() does.
TEST_F(TransactionLiveTest, DisconnectWithOpenTransactionIsRefused) {
    SQLHDBC dbc = SQL_NULL_HDBC;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc), SQL_HANDLE_ENV, env_);

    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;
    ASSERT_SQL_OK(SQLDriverConnect(dbc, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                                   static_cast<SQLSMALLINT>(connstr.size()), outStr,
                                   static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLTCHAR)),
                                   &outLen, SQL_DRIVER_NOPROMPT),
                  SQL_HANDLE_DBC, dbc);

    ASSERT_SQL_OK(SetAutocommit(dbc, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc);
    SQLHSTMT hstmt = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc, &hstmt), SQL_HANDLE_DBC, dbc);
    ASSERT_SQL_OK(Run(hstmt, "CREATE TABLE #disc_t(i int)"), SQL_HANDLE_STMT, hstmt);
    SQLFreeHandle(SQL_HANDLE_STMT, hstmt);

    EXPECT_SQL_ERROR(SQLDisconnect(dbc));
    EXPECT_SQLSTATE(SQL_HANDLE_DBC, dbc, "25000");

    // After resolving the transaction the disconnect succeeds.
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc);
    EXPECT_SQL_OK(SQLDisconnect(dbc), SQL_HANDLE_DBC, dbc);
    SQLFreeHandle(SQL_HANDLE_DBC, dbc);
}

// -------------------------------------------------------------------
// Recovery: the driver re-opens a transaction the server ended underneath it
// -------------------------------------------------------------------

// A T-SQL ROLLBACK issued by the application ends the driver's transaction.
// The next statement must start a fresh one instead of running unprotected
// (`CheckOptions`, sqlccmd.cpp).
//
// Benefits-from-mock-tds: a mock TDS server could assert the transaction-manager
// BEGIN that precedes the second INSERT.
TEST_F(TransactionLiveTest, TransactionIsReopenedAfterServerSideRollback) {
    Exec("CREATE TABLE #reopen_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);

    Exec("INSERT INTO #reopen_t VALUES (1)");
    Exec("ROLLBACK TRANSACTION");
    EXPECT_EQ(0, RowCountOf("#reopen_t"));

    // The next write is protected by a new transaction, so it can be undone.
    Exec("INSERT INTO #reopen_t VALUES (2)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(0, RowCountOf("#reopen_t"));
}

// Same recovery path, but the ROLLBACK is hidden inside a stored procedure so
// the driver cannot see it in the batch text it sent. Recovery must therefore
// come from the server's transaction state, not from inspecting SQL.
//
// Benefits-from-mock-tds: a mock TDS server could assert that the driver sends
// a fresh transaction-manager BEGIN before the second INSERT rather than
// inferring it from row counts.
TEST_F(TransactionLiveTest, TransactionIsReopenedAfterRollbackInsideStoredProcedure) {
    Exec("CREATE TABLE #proc_t(i int)");
    Exec("CREATE PROCEDURE #rollback_proc AS BEGIN SET NOCOUNT ON; ROLLBACK TRANSACTION; END");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);

    Exec("INSERT INTO #proc_t VALUES (1)");

    // SQL Server raises error 266 because the procedure leaves @@TRANCOUNT
    // lower than it found it. Either outcome is fine; the rollback still
    // happened, which is what the driver has to notice.
    Run(stmt_, "EXEC #rollback_proc");
    SQLFreeStmt(stmt_, SQL_CLOSE);
    EXPECT_EQ(0, RowCountOf("#proc_t")) << "the procedure's ROLLBACK undid the insert";

    // The next write must be protected by a newly opened transaction.
    Exec("INSERT INTO #proc_t VALUES (2)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_ROLLBACK), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(0, RowCountOf("#proc_t")) << "the driver reopened a transaction after the procedure";
}

// XACT_ABORT aborts the transaction on the first error; the connection stays
// usable and the next statement gets a fresh transaction.
//
// Benefits-from-mock-tds: a mock TDS server could assert the driver observes the
// server's transaction-ended notification rather than tracking state blindly.
TEST_F(TransactionLiveTest, TransactionIsReopenedAfterXactAbort) {
    Exec("CREATE TABLE #abort_t(i int)");
    ASSERT_SQL_OK(SetAutocommit(dbc_, SQL_AUTOCOMMIT_OFF), SQL_HANDLE_DBC, dbc_);
    Exec("SET XACT_ABORT ON");

    Exec("INSERT INTO #abort_t VALUES (1)");
    EXPECT_SQL_ERROR(Run(stmt_, "INSERT INTO #abort_t VALUES (1/0)"));

    EXPECT_EQ(0, RowCountOf("#abort_t")) << "XACT_ABORT rolls the whole transaction back";

    Exec("INSERT INTO #abort_t VALUES (2)");
    ASSERT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, dbc_, SQL_COMMIT), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(1, RowCountOf("#abort_t"));
}

// -------------------------------------------------------------------
// SQLGetInfo transaction capabilities
// -------------------------------------------------------------------

TEST_F(TransactionLiveTest, TransactionInfoTypesReportExpectedValues) {
    SQLUSMALLINT txn_capable = 0xFFFF;
    ASSERT_SQL_OK(SQLGetInfo(dbc_, SQL_TXN_CAPABLE, &txn_capable, sizeof(txn_capable), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_TC_ALL, txn_capable) << "SQL Server transacts DML and DDL alike";

    SQLUINTEGER default_isolation = 0;
    ASSERT_SQL_OK(SQLGetInfo(dbc_, SQL_DEFAULT_TXN_ISOLATION, &default_isolation,
                             sizeof(default_isolation), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(static_cast<SQLUINTEGER>(SQL_TXN_READ_COMMITTED), default_isolation);

    SQLUINTEGER options = 0;
    ASSERT_SQL_OK(
        SQLGetInfo(dbc_, SQL_TXN_ISOLATION_OPTION, &options, sizeof(options), nullptr),
        SQL_HANDLE_DBC, dbc_);
    const SQLUINTEGER expected_options = SQL_TXN_READ_UNCOMMITTED | SQL_TXN_READ_COMMITTED |
                                         SQL_TXN_REPEATABLE_READ | SQL_TXN_SERIALIZABLE |
                                         SQL_TXN_SS_SNAPSHOT;
    EXPECT_EQ(expected_options, options);

    SQLTCHAR multiple[8] = {};
    ASSERT_SQL_OK(
        SQLGetInfo(dbc_, SQL_MULTIPLE_ACTIVE_TXN, multiple, sizeof(multiple), nullptr),
        SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ("Y", ODBCTestUtils::ToNarrow(SqlTString(multiple)));
}

// Cursors do not survive SQLEndTran on either driver. msodbcsql additionally
// reports SQL_CB_PRESERVE to the Windows Driver Manager on the very first
// query so the DM does not close cursors itself; mssql-odbc always answers
// truthfully, so only the mssql-odbc leg can assert an exact value.
TEST_F(TransactionLiveTest, CursorBehaviorIsClose) {
    SKIP_IF_COMPARING_MSODBCSQL();

    SQLUSMALLINT commit_behavior = 0xFFFF;
    ASSERT_SQL_OK(SQLGetInfo(dbc_, SQL_CURSOR_COMMIT_BEHAVIOR, &commit_behavior,
                             sizeof(commit_behavior), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_CB_CLOSE, commit_behavior);

    SQLUSMALLINT rollback_behavior = 0xFFFF;
    ASSERT_SQL_OK(SQLGetInfo(dbc_, SQL_CURSOR_ROLLBACK_BEHAVIOR, &rollback_behavior,
                             sizeof(rollback_behavior), nullptr),
                  SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_CB_CLOSE, rollback_behavior);
}
