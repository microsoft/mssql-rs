// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "tcp_pause_proxy.h"
#include "odbc_test_fixture.h"

#include <atomic>
#include <chrono>
#include <future>
#include <string>
#include <thread>

class CancelLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
            << "Set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        Connect();
        SQLCHAR version[32] = {};
        ASSERT_SQL_OK(SQLGetInfoA(dbc_, SQL_DRIVER_VER, version, sizeof(version), nullptr),
                      SQL_HANDLE_DBC, dbc_);
        RecordProperty("driver_version", reinterpret_cast<const char*>(version));
        Log("SQL_DRIVER_VER=" + std::string(reinterpret_cast<const char*>(version)));
    }

    void AssertReusable() {
        ASSERT_NO_FATAL_FAILURE(ExecDirect("SELECT 1"));
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER value = 0;
        SQLLEN length = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_LONG, &value, sizeof(value), &length),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(1, value);
    }

    // The proxy relays to ODBC_TEST_SERVER, so it needs a direct TCP endpoint.
    static bool ProxyUsable() {
        const auto& config = ODBCTestConfig::Instance();
        return !config.HasConnStr() && !config.HasDSN() &&
               config.Server().find('\\') == std::string::npos;
    }

    // Reconnects dbc_ through |proxy|, which must outlive the connection, and
    // allocates a fresh stmt_.
    void ReconnectThrough(const TcpPauseProxy& proxy) {
        const auto& config = ODBCTestConfig::Instance();
        auto connection = ODBCTestUtils::ToNarrow(ODBCTestUtils::BuildConnectionString());
        const auto endpoint = "Server=" + config.Server() + ";";
        const auto position = connection.find(endpoint);
        ASSERT_NE(std::string::npos, position);
        connection.replace(position, endpoint.size(),
                           "Server=127.0.0.1," + std::to_string(proxy.Port()) + ";");
        ASSERT_SQL_OK(SQLFreeHandle(SQL_HANDLE_STMT, stmt_), SQL_HANDLE_STMT, stmt_);
        stmt_ = SQL_NULL_HSTMT;
        ASSERT_SQL_OK(SQLDisconnect(dbc_), SQL_HANDLE_DBC, dbc_);
        auto text = ODBCTestUtils::ToSqlTStr(connection);
        ASSERT_SQL_OK(SQLDriverConnect(dbc_, nullptr, const_cast<SQLTCHAR*>(text.c_str()),
                                      SQL_NTS, nullptr, 0, nullptr, SQL_DRIVER_NOPROMPT),
                      SQL_HANDLE_DBC, dbc_);
        ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_), SQL_HANDLE_DBC, dbc_);
    }

    // Undoes ReconnectThrough before the proxy goes away.
    void DisconnectFromProxy() {
        ASSERT_SQL_OK(SQLFreeHandle(SQL_HANDLE_STMT, stmt_), SQL_HANDLE_STMT, stmt_);
        stmt_ = SQL_NULL_HSTMT;
        ASSERT_SQL_OK(SQLDisconnect(dbc_), SQL_HANDLE_DBC, dbc_);
    }

    void CancelCursor(bool moreResults, bool closeImmediately) {
        using namespace std::chrono_literals;
        ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT,
                                    reinterpret_cast<SQLPOINTER>(15), 0),
                      SQL_HANDLE_STMT, stmt_);
        // A large first row forces a response packet before the WAITFOR.
        ASSERT_NO_FATAL_FAILURE(ExecDirect(
            "SELECT REPLICATE(CAST('x' AS varchar(max)), 16000); "
            "WAITFOR DELAY '00:00:10'; SELECT 2"));
        auto operation = std::async(std::launch::async, [&] {
            if (moreResults) {
                return SQLMoreResults(stmt_);
            }
            SQLRETURN rc;
            do {
                rc = SQLFetch(stmt_);
            } while (SQL_SUCCEEDED(rc));
            return rc;
        });
        EXPECT_EQ(std::future_status::timeout, operation.wait_for(500ms));
        const auto started = std::chrono::steady_clock::now();
        EXPECT_EQ(SQL_SUCCESS, SQLCancel(stmt_));
        if (closeImmediately) {
            EXPECT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
        }
        const auto settled = operation.wait_for(5s);
        const auto rc = operation.get();
        ASSERT_EQ(std::future_status::ready, settled);
        EXPECT_LT(std::chrono::steady_clock::now() - started, 5s);
        // Registry entry 27: native MoreResults reports exhaustion here.
        const auto* target = std::getenv("ODBC_TEST_TARGET");
        const bool nativeMoreResults =
            moreResults && target && std::string(target) == "msodbcsql";
        ASSERT_EQ(nativeMoreResults ? SQL_NO_DATA : SQL_ERROR, rc);
        if (!closeImmediately) {
            if (!nativeMoreResults) {
                EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY008");
            }
            ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
        }
        AssertReusable();
    }
};

// Benefits-from-mock-tds: cancel.rs pins ATTENTION settlement and client reuse;
// this test verifies the same outcome through the Driver Manager.
TEST_F(CancelLiveTest, CrossThreadCancelInterruptsWaitforAndConnectionRemainsUsable) {
    using namespace std::chrono_literals;
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT,
                                reinterpret_cast<SQLPOINTER>(15), 0),
                  SQL_HANDLE_STMT, stmt_);
    auto sql = ODBCTestUtils::ToSqlTStr("WAITFOR DELAY '00:00:10'; SELECT 2");
    auto execution = std::async(std::launch::async, [&] {
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS);
    });
    const auto initial = execution.wait_for(500ms);
    EXPECT_EQ(std::future_status::timeout, initial);
    const auto started = std::chrono::steady_clock::now();
    const auto cancelRc = SQLCancel(stmt_);
    // No fatal assertion before joining: the statement and SQL buffer must
    // outlive the executing thread even when cancellation regresses.
    EXPECT_EQ(SQL_SUCCESS, cancelRc);
    const auto settled = execution.wait_for(5s);
    const auto executeRc = execution.get();
    ASSERT_EQ(std::future_status::ready, settled);
    EXPECT_LT(std::chrono::steady_clock::now() - started, 5s);
    ASSERT_EQ(SQL_ERROR, executeRc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY008");

    sql = ODBCTestUtils::ToSqlTStr("SELECT 1");
    ASSERT_SQL_OK(SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    SQLLEN length = 0;
    ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_LONG, &value, sizeof(value), &length),
                  SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(1, value);
    EXPECT_EQ(static_cast<SQLLEN>(sizeof(value)), length);
}

// Benefits-from-mock-tds: cancel.rs pins that ATTENTION was acknowledged before
// SQLCancel returned; this observes only that the immediate close succeeds.
TEST_F(CancelLiveTest, FetchCancelThenImmediateCloseIsSafe) {
    CancelCursor(false, true);
}

// Benefits-from-mock-tds: cancel.rs pins that ATTENTION was acknowledged before
// SQLCancel returned; this observes only that the immediate close succeeds.
TEST_F(CancelLiveTest, MoreResultsCancelThenImmediateCloseIsSafe) {
    CancelCursor(true, true);
}

// Benefits-from-mock-tds: assert ATTENTION and its DONE_ATTN acknowledgement on
// the wire, not just the HY008 outcome and connection reuse.
TEST_F(CancelLiveTest, FetchCancellationReportsHy008) {
    CancelCursor(false, false);
}

// Benefits-from-mock-tds: assert ATTENTION and its DONE_ATTN acknowledgement on
// the wire, not just each driver's return code and connection reuse.
TEST_F(CancelLiveTest, MoreResultsCancellationReturnCode) {
    CancelCursor(true, false);
}

// Benefits-from-mock-tds: assert the paused packet completed before IGNORE or
// ATTENTION withdrew the request; the proxy only shows the connection survived.
TEST_F(CancelLiveTest, PutDataCancelFinishesPendingPacketAndPreservesConnection) {
    using namespace std::chrono_literals;
    if (!ProxyUsable())
        GTEST_SKIP() << "Backpressure proxy requires ODBC_TEST_SERVER as a direct TCP endpoint";
    TcpPauseProxy proxy(ODBCTestConfig::Instance().Server());
    ASSERT_NO_FATAL_FAILURE(ReconnectThrough(proxy));
    ASSERT_SQL_OK(SQLSetConnectAttr(dbc_, SQL_ATTR_CONNECTION_TIMEOUT,
                                   reinterpret_cast<SQLPOINTER>(5), 0),
                  SQL_HANDLE_DBC, dbc_);
    auto sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS v");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    SQLLEN indicator = SQL_DATA_AT_EXEC;
    char token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR,
                                  0, 0, &token, 0, &indicator), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value));

    std::vector<char> chunk(8 * 1024 * 1024, 'a');
    proxy.Pause();
    auto writing = std::async(std::launch::async, [&] {
        return SQLPutData(stmt_, chunk.data(), static_cast<SQLLEN>(chunk.size()));
    });
    EXPECT_EQ(std::future_status::timeout, writing.wait_for(200ms));
    auto cancelling = std::async(std::launch::async, [&] { return SQLCancel(stmt_); });
    const auto pendingCancel = cancelling.wait_for(50ms);
    // Always release backpressure before asserting or destroying the futures.
    proxy.Resume();
    const auto cancelReady = cancelling.wait_for(5s);
    const auto writeReady = writing.wait_for(5s);
    const auto cancelRc = cancelling.get();
    const auto writeRc = writing.get();
    EXPECT_EQ(std::future_status::timeout, pendingCancel);
    ASSERT_EQ(std::future_status::ready, cancelReady);
    ASSERT_EQ(std::future_status::ready, writeReady);
    ASSERT_EQ(SQL_SUCCESS, cancelRc);
    ASSERT_EQ(SQL_ERROR, writeRc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY008");
    SQLUINTEGER dead = SQL_CD_TRUE;
    ASSERT_SQL_OK(SQLGetConnectAttr(dbc_, SQL_ATTR_CONNECTION_DEAD, &dead,
                                   SQL_IS_UINTEGER, nullptr), SQL_HANDLE_DBC, dbc_);
    EXPECT_EQ(SQL_CD_FALSE, dead);
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    AssertReusable();
    EXPECT_FALSE(proxy.Failed());
    DisconnectFromProxy();
}

// Benefits-from-mock-tds: assert ATTENTION after the completed RPC and that the
// retry reuses the prepared handle; this observes only the restored outcome.
TEST_F(CancelLiveTest, FinalParamDataCancellationRestoresPreparedStatement) {
    using namespace std::chrono_literals;
    auto sql = ODBCTestUtils::ToSqlTStr("WAITFOR DELAY '00:00:10'; SELECT ? AS v");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT,
                                reinterpret_cast<SQLPOINTER>(15), 0), SQL_HANDLE_STMT, stmt_);
    SQLLEN indicator = SQL_DATA_AT_EXEC;
    char token = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_CHAR, SQL_VARCHAR,
                                  0, 0, &token, 0, &indicator), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    SQLPOINTER value = nullptr;
    ASSERT_EQ(SQL_NEED_DATA, SQLParamData(stmt_, &value));
    char data[] = "cancel-me";
    ASSERT_SQL_OK(SQLPutData(stmt_, data, sizeof(data) - 1), SQL_HANDLE_STMT, stmt_);
    auto execution = std::async(std::launch::async, [&] {
        SQLPOINTER next = nullptr;
        return SQLParamData(stmt_, &next);
    });
    EXPECT_EQ(std::future_status::timeout, execution.wait_for(500ms));
    EXPECT_EQ(SQL_SUCCESS, SQLCancel(stmt_));
    const auto ready = execution.wait_for(5s);
    const auto rc = execution.get();
    ASSERT_EQ(std::future_status::ready, ready);
    ASSERT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY008");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    ASSERT_EQ(SQL_NEED_DATA, SQLExecute(stmt_));
    ASSERT_SQL_OK(SQLCancel(stmt_), SQL_HANDLE_STMT, stmt_);
    AssertReusable();
}

// A cancel just before an execute must not reach that execute or the next one,
// through a Driver Manager; the driver's unit tests pin the same directly. Even
// rounds cancel the idle statement and then execute, which must return its own
// row. Odd rounds race the cancel against the execute, which it may interrupt
// (HY008; it did in every measured round). Each round ends with an execute and
// no cancel, which must succeed.
//
// Benefits-from-mock-tds: delay the response so the racing cancel lands at a
// chosen point instead of wherever scheduling puts it.
TEST_F(CancelLiveTest, CancelJustBeforeAnExecuteDoesNotReachThatOrTheNextExecute) {
    auto sql = ODBCTestUtils::ToSqlTStr("SELECT ? AS v");
    ASSERT_SQL_OK(SQLPrepare(stmt_, const_cast<SQLTCHAR*>(sql.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, stmt_);
    SQLINTEGER value = 0;
    SQLLEN length = 0;
    ASSERT_SQL_OK(SQLBindParameter(stmt_, 1, SQL_PARAM_INPUT, SQL_C_SLONG, SQL_INTEGER, 0, 0,
                                  &value, 0, &length),
                  SQL_HANDLE_STMT, stmt_);
    int interrupted = 0;
    for (int round = 0; round < 50; ++round) {
        SCOPED_TRACE(round);
        value = round;
        if (round % 2 == 0) {
            ASSERT_SQL_OK(SQLCancel(stmt_), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
            ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
            SQLINTEGER result = -1;
            ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &result, 0, nullptr),
                          SQL_HANDLE_STMT, stmt_);
            EXPECT_EQ(round, result);
        } else {
            auto cancelling = std::async(std::launch::async, [&] { return SQLCancel(stmt_); });
            const SQLRETURN raced = SQLExecute(stmt_);
            const SQLRETURN cancelRc = cancelling.get();
            EXPECT_TRUE(SQL_SUCCEEDED(cancelRc));
            if (raced == SQL_ERROR) {
                EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY008");
                ++interrupted;
            } else {
                EXPECT_SQL_OK(raced, SQL_HANDLE_STMT, stmt_);
            }
        }
        ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);

        value = 1000 + round;
        ASSERT_SQL_OK(SQLExecute(stmt_), SQL_HANDLE_STMT, stmt_);
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER result = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_SLONG, &result, 0, nullptr), SQL_HANDLE_STMT,
                      stmt_);
        EXPECT_EQ(1000 + round, result);
        ASSERT_SQL_OK(SQLCloseCursor(stmt_), SQL_HANDLE_STMT, stmt_);
    }
    RecordProperty("interrupted_executes", interrupted);
}

// A fetch waiting on a row lock another connection holds, rather than on a
// WAITFOR inside the batch. The unlocked rows ahead of it fill whole packets, so
// the execute returns and the fetch loop is what blocks on the server.
//
// Benefits-from-mock-tds: assert ATTENTION and DONE_ATTN on the wire; a live
// server can only show the lock wait ended and the connection survived.
TEST_F(CancelLiveTest, CancelInterruptsAFetchBlockedByAnotherConnectionsRowLock) {
    using namespace std::chrono_literals;
    const std::string table =
        "##cancel_lock_" +
        std::to_string(std::chrono::steady_clock::now().time_since_epoch().count());
    ASSERT_NO_FATAL_FAILURE(ExecDirect(
        "CREATE TABLE " + table + " (id int PRIMARY KEY, v varchar(200)); "
        "WITH n AS (SELECT TOP (301) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS id "
        "FROM sys.all_objects) INSERT " + table + " SELECT id, REPLICATE('x', 150) FROM n"));
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);

    SQLHDBC holder = SQL_NULL_HDBC;
    SQLHSTMT holding = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_DBC, env_, &holder), SQL_HANDLE_ENV, env_);
    auto connection = ODBCTestUtils::BuildConnectionString();
    ASSERT_SQL_OK(SQLDriverConnect(holder, nullptr, const_cast<SQLTCHAR*>(connection.c_str()),
                                  SQL_NTS, nullptr, 0, nullptr, SQL_DRIVER_NOPROMPT),
                  SQL_HANDLE_DBC, holder);
    ASSERT_SQL_OK(SQLSetConnectAttr(holder, SQL_ATTR_AUTOCOMMIT,
                                   reinterpret_cast<SQLPOINTER>(SQL_AUTOCOMMIT_OFF), 0),
                  SQL_HANDLE_DBC, holder);
    ASSERT_SQL_OK(SQLAllocHandle(SQL_HANDLE_STMT, holder, &holding), SQL_HANDLE_DBC, holder);
    auto lock = ODBCTestUtils::ToSqlTStr("UPDATE " + table + " SET v = 'held' WHERE id = 301");
    ASSERT_SQL_OK(SQLExecDirect(holding, const_cast<SQLTCHAR*>(lock.c_str()), SQL_NTS),
                  SQL_HANDLE_STMT, holding);

    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT,
                                reinterpret_cast<SQLPOINTER>(15), 0),
                  SQL_HANDLE_STMT, stmt_);
    // READCOMMITTEDLOCK: a database using row versioning would not block the read.
    ASSERT_NO_FATAL_FAILURE(
        ExecDirect("SELECT id, v FROM " + table + " WITH (READCOMMITTEDLOCK) ORDER BY id"));
    std::atomic<int> fetched{0};
    auto fetching = std::async(std::launch::async, [&] {
        SQLRETURN rc;
        while (SQL_SUCCEEDED(rc = SQLFetch(stmt_))) {
            ++fetched;
        }
        return rc;
    });
    const auto deadline = std::chrono::steady_clock::now() + 5s;
    while (fetched.load() == 0 && std::chrono::steady_clock::now() < deadline) {
        std::this_thread::sleep_for(10ms);
    }
    EXPECT_EQ(std::future_status::timeout, fetching.wait_for(500ms));
    const int beforeCancel = fetched.load();
    const auto started = std::chrono::steady_clock::now();
    const SQLRETURN cancelRc = SQLCancel(stmt_);
    const auto settled = fetching.wait_for(5s);
    // Release the lock before any fatal assertion, so cleanup cannot block.
    EXPECT_SQL_OK(SQLEndTran(SQL_HANDLE_DBC, holder, SQL_ROLLBACK), SQL_HANDLE_DBC, holder);
    const SQLRETURN fetchRc = fetching.get();
    SQLFreeHandle(SQL_HANDLE_STMT, holding);
    SQLDisconnect(holder);
    SQLFreeHandle(SQL_HANDLE_DBC, holder);

    EXPECT_GT(beforeCancel, 0) << "the execute, not a fetch, was blocked";
    EXPECT_LT(beforeCancel, 301);
    EXPECT_EQ(SQL_SUCCESS, cancelRc);
    ASSERT_EQ(std::future_status::ready, settled);
    EXPECT_LT(std::chrono::steady_clock::now() - started, 5s);
    ASSERT_EQ(SQL_ERROR, fetchRc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY008");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    AssertReusable();
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    ExecDirectIgnoreError("DROP TABLE " + table);
}

// A SQLGetData waiting for the rest of a PLP value: the proxy holds the server's
// response after the first chunk, so the read blocks on the network mid-value.
//
// Benefits-from-mock-tds: stall the PLP stream at a chosen chunk boundary and
// assert ATTENTION followed by DONE_ATTN.
TEST_F(CancelLiveTest, CancelInterruptsAGetDataBlockedMidPlpValue) {
    using namespace std::chrono_literals;
    if (!ProxyUsable())
        GTEST_SKIP() << "Backpressure proxy requires ODBC_TEST_SERVER as a direct TCP endpoint";
    TcpPauseProxy proxy(ODBCTestConfig::Instance().Server());
    ASSERT_NO_FATAL_FAILURE(ReconnectThrough(proxy));
    ASSERT_SQL_OK(SQLSetStmtAttr(stmt_, SQL_ATTR_QUERY_TIMEOUT,
                                reinterpret_cast<SQLPOINTER>(15), 0),
                  SQL_HANDLE_STMT, stmt_);
    ASSERT_NO_FATAL_FAILURE(ExecDirect("SELECT REPLICATE(CAST('p' AS varchar(max)), 8388608)"));
    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
    char buffer[4096];
    SQLLEN length = 0;
    ASSERT_EQ(SQL_SUCCESS_WITH_INFO,
              SQLGetData(stmt_, 1, SQL_C_CHAR, buffer, sizeof(buffer), &length));

    proxy.PauseResponses();
    auto reading = std::async(std::launch::async, [&] {
        SQLRETURN rc;
        do {
            rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buffer, sizeof(buffer), &length);
        } while (rc == SQL_SUCCESS_WITH_INFO);
        return rc;
    });
    const auto blocked = reading.wait_for(500ms);
    const auto started = std::chrono::steady_clock::now();
    auto cancelling = std::async(std::launch::async, [&] { return SQLCancel(stmt_); });
    const auto pendingCancel = cancelling.wait_for(50ms);
    // Always release the response before asserting or destroying the futures.
    proxy.Resume();
    const auto cancelReady = cancelling.wait_for(5s);
    const auto readReady = reading.wait_for(5s);
    const SQLRETURN cancelRc = cancelling.get();
    const SQLRETURN readRc = reading.get();
    RecordProperty("cancel_waited_for_response",
                   pendingCancel == std::future_status::timeout ? 1 : 0);
    EXPECT_EQ(std::future_status::timeout, blocked) << "the read was not blocked mid-value";
    ASSERT_EQ(std::future_status::ready, cancelReady);
    ASSERT_EQ(std::future_status::ready, readReady);
    EXPECT_LT(std::chrono::steady_clock::now() - started, 5s);
    ASSERT_EQ(SQL_SUCCESS, cancelRc);
    ASSERT_EQ(SQL_ERROR, readRc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "HY008");
    ASSERT_SQL_OK(SQLFreeStmt(stmt_, SQL_CLOSE), SQL_HANDLE_STMT, stmt_);
    AssertReusable();
    EXPECT_FALSE(proxy.Failed());
    DisconnectFromProxy();
}