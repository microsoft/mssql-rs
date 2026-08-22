// Copyright (c) Microsoft Corporation. All rights reserved.
// session_recovery_test.cpp  –  E2E test for the SQLExecute recovery/epoch gate.
//
// STATUS: enabled. Exercises SQLExecute-after-KILL recovery against a live
// server that negotiates Idle Connection Resiliency (SESSIONRECOVERY).
//
// Requires a configured connection (ODBC_TEST_SERVER / ODBC_TEST_CONNSTR) to a
// server that negotiates Idle Connection Resiliency (SESSIONRECOVERY) — any
// SQL Server 2014+ or Azure SQL Database.
//
// `ConnectRetryCount` as a connection-string attribute is not required — the
// driver already requests recovery via the default connect_retry_count = 1.

#include "odbc_test_fixture.h"

#include <chrono>
#include <string>
#include <thread>
#include <vector>

class SessionRecoveryLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
            << "No connection configured – set ODBC_TEST_SERVER or "
               "ODBC_TEST_CONNSTR";
        Connect();
    }

    SQLRETURN Prepare(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLPrepare(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // Execute |sql| on |h|, read column 1 of the first row as a string, and
    // close the cursor. For single-value probe queries (@@SPID, SERVERPROPERTY).
    // Reads as SQL_C_CHAR: SQLGetData does not yet implement the integer C
    // types (SQL_C_SLONG returns HYC00).
    std::string GetStringScalar(SQLHSTMT h, const std::string& sql) {
        SqlTString tsql = ODBCTestUtils::ToSqlTStr(sql);
        EXPECT_SQL_OK(
            SQLExecDirect(h, const_cast<SQLTCHAR*>(tsql.c_str()), SQL_NTS),
            SQL_HANDLE_STMT, h);
        EXPECT_SQL_OK(SQLFetch(h), SQL_HANDLE_STMT, h);
        SQLCHAR buf[128] = {0};
        SQLLEN ind = 0;
        EXPECT_SQL_OK(SQLGetData(h, 1, SQL_C_CHAR, buf, sizeof(buf), &ind),
                      SQL_HANDLE_STMT, h);
        SQLFreeStmt(h, SQL_CLOSE);
        return std::string(reinterpret_cast<const char*>(buf));
    }

    // Read column 1 of the current row on stmt_ as a string (cursor already
    // positioned via SQLFetch).
    std::string ReadCurrentCol1() {
        SQLCHAR buf[128] = {0};
        SQLLEN ind = 0;
        EXPECT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind),
                      SQL_HANDLE_STMT, stmt_);
        return std::string(reinterpret_cast<const char*>(buf));
    }

    // Open an independent connection and KILL |spid|, discarding the target
    // session (and its server-side prepared handles) on the server.
    void KillSpidFromSecondConnection(const std::string& spid) {
        SQLHDBC killer_dbc = SQL_NULL_HDBC;
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &killer_dbc),
                      SQL_HANDLE_ENV, env_);

        SqlTString connstr = ODBCTestUtils::BuildConnectionString();
        SQLTCHAR outStr[1024] = {};
        SQLSMALLINT outLen = 0;
        ASSERT_SQL_OK(
            SQLDriverConnect(
                killer_dbc, nullptr, const_cast<SQLTCHAR*>(connstr.c_str()),
                static_cast<SQLSMALLINT>(connstr.size()), outStr,
                static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLTCHAR)),
                &outLen, SQL_DRIVER_NOPROMPT),
            SQL_HANDLE_DBC, killer_dbc);

        SQLHSTMT killer_stmt = SQL_NULL_HSTMT;
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, killer_dbc, &killer_stmt),
                      SQL_HANDLE_DBC, killer_dbc);

        std::string kill = "KILL " + spid;
        SqlTString tkill = ODBCTestUtils::ToSqlTStr(kill);
        EXPECT_SQL_OK(
            SQLExecDirect(killer_stmt, const_cast<SQLTCHAR*>(tkill.c_str()),
                          SQL_NTS),
            SQL_HANDLE_STMT, killer_stmt);

        SQLFreeHandle(SQL_HANDLE_STMT, killer_stmt);
        SQLDisconnect(killer_dbc);
        SQLFreeHandle(SQL_HANDLE_DBC, killer_dbc);
    }
};

// Prepare + execute (caches a handle), kill the owning session, then execute
// again: the driver must transparently reconnect, invalidate the stale handle,
// re-prepare, and return the fresh result.
//
// Benefits-from-mock-tds: a mock TDS server could assert the stale prepared
// handle is not executed and a fresh sp_prepexec fires, which the returned
// result alone cannot verify.
TEST_F(SessionRecoveryLiveTest, StaleHandleAfterReconnectIsInvalidatedAndReprepared) {
    // 1. Record the owning session's SPID (the KILL target) and its physical
    //    connect time. A transparent reconnect performs a fresh login with a
    //    later connect_time; unlike @@SPID (which SQL Server reuses once a SPID
    //    is freed), connect_time reliably distinguishes the recovered session.
    std::string original_spid = GetStringScalar(stmt_, "SELECT @@SPID");
    ASSERT_FALSE(original_spid.empty());
    const std::string kConnectTimeSql =
        "SELECT CONVERT(varchar(30), connect_time, 121) "
        "FROM sys.dm_exec_connections WHERE session_id = @@SPID";
    std::string original_connect_time = GetStringScalar(stmt_, kConnectTimeSql);
    ASSERT_FALSE(original_connect_time.empty());

    // 2. Prepare + execute a parameterized statement so the driver caches a
    //    server prepared-statement handle (sp_prepexec) on this session.
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> value = {'7', '\0'};
    SQLLEN ind = SQL_NTS;
    ASSERT_SQL_OK(
        SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR,
                         value.size(), 0, value.data(),
                         static_cast<SQLLEN>(value.size()), &ind),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("7", ReadCurrentCol1());
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // 3. Kill the owning session from a second connection — the server discards
    //    the prepared handle along with the session.
    KillSpidFromSecondConnection(original_spid);

    // 4. Let the server tear the session down so the next use observes a dead
    //    connection and triggers transparent recovery.
    std::this_thread::sleep_for(std::chrono::seconds(1));

    // 5. Re-execute the SAME prepared statement. The driver must reconnect (new
    //    session, bumped epoch), detect the cached handle belongs to the dead
    //    session, scrub + re-prepare, and run on the new session — never firing
    //    the stale handle (no 8179).
    ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ("7", ReadCurrentCol1());
    ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);

    // 6. Confirm a real transparent reconnect happened: the recovered session is
    //    a fresh physical login, so its connect_time is later than the original.
    //    (@@SPID is not used here — SQL Server reuses freed SPIDs, so it can
    //    repeat across a genuine reconnect.)
    std::string new_connect_time = GetStringScalar(stmt_, kConnectTimeSql);
    ASSERT_FALSE(new_connect_time.empty());
    EXPECT_NE(original_connect_time, new_connect_time)
        << "expected a fresh physical login (later connect_time) after "
           "transparent reconnect";
}

// A data-at-execution sequence that dies on the wire cannot be retracted: the
// half-written RPC is already partly on the socket, and EOM | IGNORE only
// retracts a message the server has not finished reading. The driver therefore
// drops the transport (TdsClient::abort_streamed_write). The *session* survives
// that, though: the aborted sequence clears the open-batch flag and hands the
// client back to the connection, so the next execute sees a dead socket and
// recovers through the same check_and_reconnect path every other command uses.
// This is the lazy recovery msodbcsql performs in GetBatchCtxOrRecover, which
// likewise runs at the top of the next API call rather than at failure time.
TEST_F(SessionRecoveryLiveTest, ConnectionRecoversAfterDataAtExecutionDiesOnTheWire) {
    const std::string kConnectTimeSql =
        "SELECT CONVERT(varchar(30), connect_time, 121) "
        "FROM sys.dm_exec_connections WHERE session_id = @@SPID";
    std::string original_spid = GetStringScalar(stmt_, "SELECT @@SPID");
    ASSERT_FALSE(original_spid.empty());
    std::string original_connect_time = GetStringScalar(stmt_, kConnectTimeSql);
    ASSERT_FALSE(original_connect_time.empty());

    // Park a streamed parameter: SQLExecute returns SQL_NEED_DATA and
    // SQLParamData opens the parameter for data.
    ASSERT_SQL_OK(Prepare("SELECT ? AS v"), SQL_HANDLE_STMT, stmt_);
    std::vector<SQLCHAR> buffer(64 * 1024, 'x');
    // SQL_DATA_AT_EXEC, not SQL_LEN_DATA_AT_EXEC(n): a declared length would make
    // the driver reject an over-long chunk locally with 22026, and the chunk
    // below would never reach the socket.
    SQLLEN ind = SQL_DATA_AT_EXEC;
    ASSERT_SQL_OK(
        SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_LONGVARCHAR,
                         0, 0, buffer.data(), 0, &ind),
        SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER token = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &token));

    // Kill the session out from under the parked request. The sequence is now
    // unfinishable: the RPC prefix is already framed and the socket is gone.
    KillSpidFromSecondConnection(original_spid);
    std::this_thread::sleep_for(std::chrono::seconds(1));

    // SQLPutData succeeds: the chunk is handed to a socket the peer has reset,
    // and the local send buffer accepts it without an error surfacing yet.
    ASSERT_EQ(SQL_SUCCESS, SQLPutData(stmt_, buffer.data(),
                                      static_cast<SQLLEN>(buffer.size())));

    // SQLParamData is where the sequence dies: it writes the PLP terminator,
    // finalizes the RPC, and reads the response, so it is the first call that
    // has to hear back from the server. HY000 is the streamed-write failure
    // surfacing from TDS -- 22026 or HY010 would mean the driver rejected the
    // call locally and the abort/recovery path this test exists for never ran.
    ASSERT_EQ(SQL_ERROR, SQLParamData(stmt_, &token));
    EXPECT_EQ("HY000", ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_));

    // The sequence is over and the client is back on the connection. A fresh
    // statement must reconnect transparently rather than inheriting the dead
    // socket.
    SQLHSTMT probe = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &probe), SQL_HANDLE_DBC,
                  dbc_);
    std::string recovered_connect_time = GetStringScalar(probe, kConnectTimeSql);
    EXPECT_FALSE(recovered_connect_time.empty())
        << "the connection did not recover after the streamed write aborted";
    EXPECT_NE(original_connect_time, recovered_connect_time)
        << "expected a fresh physical login after the aborted streamed write";
    SQLFreeHandle(SQL_HANDLE_STMT, probe);
}
