// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "../connection_busy_fixture.h"

#include <atomic>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

class SharedConnectionBusyLiveTest : public ConnectionBusyLiveTest {};

// AB#49225: statements on threads sharing one connection may only ever be told
// the connection is busy, never an internal error, and a busy execute is
// retryable.
// Benefits-from-mock-tds: hold one worker's response and assert no other
// worker's request reaches the wire until it drains; the diagnostics here imply
// that but cannot see the wire, and scheduling decides whether workers collide.
TEST_F(SharedConnectionBusyLiveTest, ConcurrentStatementsOnSharedConnectionOnlyReportBusy) {
    constexpr int kThreads = 8;
    constexpr int kIterations = 20;
    constexpr int kMaxBusyRetries = 100000;
    std::atomic<int> busy_total{0};
    std::mutex mutex;
    std::vector<std::string> failures;
    auto fail = [&](const std::string& what) {
        std::lock_guard<std::mutex> lock(mutex);
        failures.push_back(what);
    };

    auto worker = [&](int worker_index) {
        SQLHSTMT hstmt = SQL_NULL_HSTMT;
        if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &hstmt))) {
            fail("SQLAllocHandle: " + ODBCTestUtils::GetDiagMessage(SQL_HANDLE_DBC, dbc_));
            return;
        }
        int busy = 0;
        for (int i = 0; i < kIterations;) {
            // Unique per execution, so a row delivered to the wrong statement fails.
            const int run = worker_index * kIterations + i;
            const std::string token = "run" + std::to_string(run);
            if (!SQL_SUCCEEDED(Run(hstmt, "SELECT CAST('" + token + "' AS varchar(max)), " +
                                              std::to_string(run)))) {
                if (!IsBusy(SQL_HANDLE_STMT, hstmt)) {
                    fail("SQLExecDirect: " +
                         ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, hstmt));
                    break;
                }
                ++busy_total;
                if (++busy > kMaxBusyRetries) {
                    fail("connection stayed busy");
                    break;
                }
                std::this_thread::yield();
                continue;
            }
            SQLRETURN rc;
            bool wrong_data = false;
            while (SQL_SUCCEEDED(rc = SQLFetch(hstmt))) {
                char text[16] = {};
                SQLINTEGER number = 0;
                SQLLEN ind = 0;
                if (!SQL_SUCCEEDED(SQLGetData(hstmt, 1, SQL_C_CHAR, text, sizeof(text), &ind)) ||
                    !SQL_SUCCEEDED(SQLGetData(hstmt, 2, SQL_C_SLONG, &number, 0, &ind))) {
                    rc = SQL_ERROR;
                    break;
                }
                if (std::string(text) != token || number != run) {
                    wrong_data = true;
                    fail("run " + std::to_string(run) + " fetched (" + text + ", " +
                         std::to_string(number) + ")");
                    break;
                }
            }
            if (wrong_data) {
                break;
            }
            if (rc != SQL_NO_DATA) {
                fail("fetch: " + ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, hstmt));
                break;
            }
            if (!SQL_SUCCEEDED(SQLCloseCursor(hstmt))) {
                fail("SQLCloseCursor: " + ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, hstmt));
                break;
            }
            ++i;
        }
        SQLFreeHandle(SQL_HANDLE_STMT, hstmt);
    };

    std::vector<std::thread> threads;
    for (int t = 0; t < kThreads; ++t) {
        threads.emplace_back(worker, t);
    }
    for (auto& thread : threads) {
        thread.join();
    }

    EXPECT_TRUE(failures.empty()) << failures.size() << " failure(s), first: "
                                  << (failures.empty() ? "" : failures.front());
    EXPECT_GT(busy_total.load(), 0) << "the workers never contended for the connection";
    EXPECT_SQL_OK(Run(stmt_, "SELECT 1"), SQL_HANDLE_STMT, stmt_);
}

// AB#49225: executes racing the driver's own SQL_USER_NAME lookup, a
// connection-scoped round trip with no statement of the application's, may
// only be told the connection is busy. Each `USE` changes the database, which
// makes the next SQLGetInfo look the user up again.
// Benefits-from-mock-tds: hold the `USER_NAME()` response so an execute races
// the lookup by construction, and assert the execute's batch never reaches the
// wire while the lookup holds it.
TEST_F(SharedConnectionBusyLiveTest, ExecutesRacingTheUserNameLookupOnlyReportBusy) {
    constexpr int kThreads = 4;
    constexpr int kIterations = 50;
    constexpr int kMaxBusyRetries = 100000;
    std::atomic<int> busy_total{0};
    std::atomic<bool> workers_done{false};
    std::mutex mutex;
    std::vector<std::string> failures;
    auto fail = [&](const std::string& what) {
        std::lock_guard<std::mutex> lock(mutex);
        failures.push_back(what);
    };

    std::thread lookup([&] {
        while (!workers_done) {
            SQLTCHAR name[256];
            SQLSMALLINT length = 0;
            if (!SQL_SUCCEEDED(SQLGetInfo(dbc_, SQL_USER_NAME, name, sizeof(name), &length))) {
                fail("SQLGetInfo: " + ODBCTestUtils::GetDiagMessage(SQL_HANDLE_DBC, dbc_));
                return;
            }
        }
    });

    auto worker = [&] {
        SQLHSTMT hstmt = SQL_NULL_HSTMT;
        if (!SQL_SUCCEEDED(SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &hstmt))) {
            fail("SQLAllocHandle: " + ODBCTestUtils::GetDiagMessage(SQL_HANDLE_DBC, dbc_));
            return;
        }
        int busy = 0;
        for (int i = 0; i < kIterations;) {
            SQLRETURN rc = Run(hstmt, "USE tempdb");
            if (SQL_SUCCEEDED(rc) || rc == SQL_NO_DATA) {
                ++i;
                continue;
            }
            if (!IsBusy(SQL_HANDLE_STMT, hstmt)) {
                fail("SQLExecDirect: " + ODBCTestUtils::GetDiagMessage(SQL_HANDLE_STMT, hstmt));
                break;
            }
            ++busy_total;
            if (++busy > kMaxBusyRetries) {
                fail("connection stayed busy");
                break;
            }
            std::this_thread::yield();
        }
        SQLFreeHandle(SQL_HANDLE_STMT, hstmt);
    };

    std::vector<std::thread> threads;
    for (int t = 0; t < kThreads; ++t) {
        threads.emplace_back(worker);
    }
    for (auto& thread : threads) {
        thread.join();
    }
    workers_done = true;
    lookup.join();

    EXPECT_TRUE(failures.empty()) << failures.size() << " failure(s), first: "
                                  << (failures.empty() ? "" : failures.front());
    EXPECT_GT(busy_total.load(), 0) << "the lookups never contended with the workers";
    EXPECT_SQL_OK(Run(stmt_, "SELECT 1"), SQL_HANDLE_STMT, stmt_);
}
