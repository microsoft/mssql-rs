// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
        // Registry entry 26: native MoreResults reports exhaustion here.
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

TEST_F(CancelLiveTest, FetchCancelThenImmediateCloseIsSafe) {
    CancelCursor(false, true);
}

TEST_F(CancelLiveTest, MoreResultsCancelThenImmediateCloseIsSafe) {
    CancelCursor(true, true);
}

TEST_F(CancelLiveTest, FetchCancellationReportsHy008) {
    CancelCursor(false, false);
}

TEST_F(CancelLiveTest, MoreResultsCancellationReturnCode) {
    CancelCursor(true, false);
}
