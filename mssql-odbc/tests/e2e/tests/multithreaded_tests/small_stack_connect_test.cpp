// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#include "odbc_test_fixture.h"

#include <future>
#include <pthread.h>

// Keep this in its own executable so the first connection also exercises cold
// TLS initialization. Unlike a direct Rust call, unixODBC consumes stack too.
class SmallStackConnectTest : public ODBCTest {};

// Benefits-from-mock-tds: a TLS mock could verify the handshake and login packets;
// this regression measures the complete application/Driver Manager/driver stack.
TEST_F(SmallStackConnectTest, ConnectAndQueryOnSmallStack) {
    ASSERT_TRUE(ODBCTestConfig::Instance().HasConnection())
        << "Set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";

#ifdef ODBC_E2E_RELEASE_DRIVER
    constexpr size_t stackBytes = 128 * 1024;
#else
    constexpr size_t stackBytes = 512 * 1024;
#endif
    std::packaged_task<void()> operation([this] {
        ASSERT_NO_FATAL_FAILURE(Connect());
        ASSERT_NO_FATAL_FAILURE(ExecDirect("SELECT 1"));
        ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);
        SQLINTEGER value = 0;
        SQLLEN length = 0;
        ASSERT_SQL_OK(SQLGetData(stmt_, 1, SQL_C_LONG, &value, sizeof(value), &length),
                      SQL_HANDLE_STMT, stmt_);
        EXPECT_EQ(1, value);
        EXPECT_EQ(static_cast<SQLLEN>(sizeof(value)), length);
        EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    });
    auto result = operation.get_future();
    pthread_attr_t attributes;
    ASSERT_EQ(0, pthread_attr_init(&attributes));
    const int configured = pthread_attr_setstacksize(&attributes, stackBytes);
    pthread_t worker;
    const int started = configured == 0
        ? pthread_create(&worker, &attributes, [](void* argument) -> void* {
              (*static_cast<std::packaged_task<void()>*>(argument))();
              return nullptr;
          }, &operation)
        : configured;
    EXPECT_EQ(0, pthread_attr_destroy(&attributes));
    ASSERT_EQ(0, started);
    ASSERT_EQ(0, pthread_join(worker, nullptr));
    result.get();
}
