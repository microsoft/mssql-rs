// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "tcp_pause_proxy.h"
#include "odbc_test_fixture.h"

#include <future>
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
    auto& config = ODBCTestConfig::Instance();
    if (config.HasConnStr() || config.HasDSN() || config.Server().find('\\') != std::string::npos)
        GTEST_SKIP() << "Backpressure proxy requires ODBC_TEST_SERVER as a direct TCP endpoint";
    TcpPauseProxy proxy(config.Server());
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
    ASSERT_SQL_OK(SQLFreeHandle(SQL_HANDLE_STMT, stmt_), SQL_HANDLE_STMT, stmt_);
    stmt_ = SQL_NULL_HSTMT;
    ASSERT_SQL_OK(SQLDisconnect(dbc_), SQL_HANDLE_DBC, dbc_);
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
