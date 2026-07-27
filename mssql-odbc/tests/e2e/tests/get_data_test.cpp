// Copyright (c) Microsoft Corporation. All rights reserved.
// get_data_test.cpp  –  E2E tests for SQLGetData continuation behavior.

#include "odbc_test_fixture.h"

#include <cstring>
#include <string>

namespace {

std::string ReadCharDataInChunks(SQLHSTMT stmt, SQLUSMALLINT col, SQLLEN* final_ind = nullptr) {
    std::string value;
    bool done = false;

    while (!done) {
        SQLCHAR buf[4] = {0};
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt, col, SQL_C_CHAR, buf, sizeof(buf), &ind);

        EXPECT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "SQLGetData failed rc=" << rc;

        size_t copied = 0;
        while (copied < sizeof(buf) && buf[copied] != 0) {
            copied++;
        }
        value.append(reinterpret_cast<const char*>(buf), copied);

        if (rc == SQL_SUCCESS) {
            if (final_ind != nullptr) {
                *final_ind = ind;
            }
            done = true;
        }
    }

    return value;
}

std::string RepeatToken(const std::string& token, size_t count) {
    std::string out;
    out.reserve(token.size() * count);
    for (size_t i = 0; i < count; ++i) {
        out += token;
    }
    return out;
}

}  // namespace

TEST(GetDataTest, NullHandleReturnsInvalidHandle) {
    SQLCHAR buf[8] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(SQL_NULL_HSTMT, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

class GetDataLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }
};

TEST_F(GetDataLiveTest, PlpSmallBufferRepeatedCalls) {
    const std::string expected = RepeatToken("abc", 200);
    ExecDirect("SELECT CAST(REPLICATE('abc', 200) AS VARCHAR(MAX))");

    SQLRETURN rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    bool saw_success_with_info = false;
    std::string observed;

    while (true) {
        SQLCHAR buf[4] = {0};
        SQLLEN ind = 0;
        rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "SQLGetData failed rc=" << rc;

        size_t copied = 0;
        while (copied < sizeof(buf) && buf[copied] != 0) {
            copied++;
        }
        observed.append(reinterpret_cast<const char*>(buf), copied);

        if (rc == SQL_SUCCESS_WITH_INFO) {
            saw_success_with_info = true;
            continue;
        }

        break;
    }

    EXPECT_TRUE(saw_success_with_info);
    EXPECT_EQ(expected, observed);

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

TEST_F(GetDataLiveTest, MixedPlpAndNonPlpColumns) {
    const std::string expected_plp = RepeatToken("x", 128);
    ExecDirect("SELECT CAST(42 AS INT), CAST(REPLICATE('x', 128) AS VARCHAR(MAX))");

    SQLRETURN rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    std::string plp = ReadCharDataInChunks(stmt_, 2);
    EXPECT_EQ(expected_plp, plp);

    SQLCHAR num_buf[16] = {0};
    SQLLEN num_ind = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, num_buf, sizeof(num_buf), &num_ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(2, num_ind);
    EXPECT_STREQ("42", reinterpret_cast<const char*>(num_buf));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

TEST_F(GetDataLiveTest, SparseColumnFetchWithPlpMiddleColumn) {
    ExecDirect("SELECT 'first', CAST(REPLICATE('y', 64) AS VARCHAR(MAX)), 'third'");

    SQLRETURN rc = SQLFetch(stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    SQLCHAR first_buf[16] = {0};
    SQLLEN first_ind = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, first_buf, sizeof(first_buf), &first_ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5, first_ind);
    EXPECT_STREQ("first", reinterpret_cast<const char*>(first_buf));

    SQLCHAR third_buf[16] = {0};
    SQLLEN third_ind = 0;
    rc = SQLGetData(stmt_, 3, SQL_C_CHAR, third_buf, sizeof(third_buf), &third_ind);
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(5, third_ind);
    EXPECT_STREQ("third", reinterpret_cast<const char*>(third_buf));

    rc = SQLCloseCursor(stmt_);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}
