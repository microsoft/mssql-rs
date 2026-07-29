// Copyright (c) Microsoft Corporation. All rights reserved.
// get_data_test.cpp  –  E2E tests for column-wise SQLGetData (msodbcsql style).
//
// SQLFetch positions on a row without materializing any column; each SQLGetData
// decodes exactly the requested column, draining the columns in between. PLP
// (VARCHAR(MAX)/NVARCHAR(MAX)/VARBINARY(MAX)) columns are streamed across
// repeated SQLGetData calls.
//
// Tests that require a live SQL Server are gated by ODBCTestConfig::HasConnection().

#include "odbc_test_fixture.h"

#include <string>
#include <vector>

// ===================================================================
// Tests that don't need a server connection
// ===================================================================

TEST(GetDataTest, NullHandle) {
    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(SQL_NULL_HSTMT, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_INVALID_HANDLE, rc);
}

// ===================================================================
// Tests that require a live SQL Server
// ===================================================================

class GetDataLiveTest : public ODBCTest {
protected:
    void SetUp() override {
        ODBCTest::SetUp();
        if (!ODBCTestConfig::Instance().HasConnection()) {
            GTEST_SKIP() << "No connection configured – set ODBC_TEST_SERVER or ODBC_TEST_CONNSTR";
        }
        Connect();
    }

    SQLRETURN ExecDirect(const std::string& sql) {
        SqlTString s = ODBCTestUtils::ToSqlTStr(sql);
        return SQLExecDirect(stmt_, const_cast<SQLTCHAR*>(s.c_str()), SQL_NTS);
    }

    // Read one column as a narrow string via a single SQLGetData call.
    std::string GetChar(SQLUSMALLINT col, SQLRETURN* rc_out = nullptr,
                        SQLLEN* ind_out = nullptr) {
        SQLCHAR buf[512] = {0};
        SQLLEN ind = 0;
        SQLRETURN rc = SQLGetData(stmt_, col, SQL_C_CHAR, buf, sizeof(buf), &ind);
        if (rc_out) {
            *rc_out = rc;
        }
        if (ind_out) {
            *ind_out = ind;
        }
        if (ind == SQL_NULL_DATA) {
            return std::string();
        }
        return std::string(reinterpret_cast<const char*>(buf));
    }
};

// SQLGetData without a positioned row (no SQLFetch yet) fails with 24000.
TEST_F(GetDataLiveTest, NoCurrentRow) {
    ASSERT_SQL_OK(ExecDirect("SELECT 1 AS c1"), SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "24000");

    SQLCloseCursor(stmt_);
}

// Column-wise retrieval: request columns in ascending order; intervening
// columns are drained transparently.
TEST_F(GetDataLiveTest, ColumnWiseAscending) {
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT CAST(1 AS INT) AS c1, "
                      "CAST('two' AS VARCHAR(10)) AS c2, "
                      "CAST(3 AS INT) AS c3, "
                      "CAST('four' AS VARCHAR(10)) AS c4, "
                      "CAST(5 AS INT) AS c5"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc;
    EXPECT_EQ("two", GetChar(2, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ("four", GetChar(4, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ("5", GetChar(5, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// Re-requesting a column strictly earlier than the last one retrieved is
// backward retrieval, which this driver rejects (SQLSTATE 07009). Re-requesting
// the column just retrieved reports end-of-data (SQL_NO_DATA).
TEST_F(GetDataLiveTest, BackwardColumnRejectedRereadIsNoData) {
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT CAST(10 AS INT) AS c1, "
                      "CAST(20 AS INT) AS c2, "
                      "CAST(30 AS INT) AS c3"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLRETURN rc;
    EXPECT_EQ("20", GetChar(2, &rc));
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);

    // Column 1 was drained while reaching column 2; requesting it now is a
    // backward access and returns SQL_ERROR with SQLSTATE 07009.
    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_ERROR, rc);
    EXPECT_SQLSTATE(SQL_HANDLE_STMT, stmt_, "07009");

    // Re-requesting the just-retrieved column 2 returns SQL_NO_DATA.
    rc = SQLGetData(stmt_, 2, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_NO_DATA, rc);

    SQLCloseCursor(stmt_);
}

// PLP streaming: a large VARCHAR(MAX) column is delivered across repeated
// SQLGetData calls. Each partial call returns SQL_SUCCESS_WITH_INFO (01004);
// the final call returns SQL_SUCCESS.
TEST_F(GetDataLiveTest, PlpVarcharMaxStreamed) {
    const int kTotal = 9000;
    ASSERT_SQL_OK(ExecDirect(
                      "SELECT REPLICATE(CAST('A' AS VARCHAR(MAX)), 9000) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    std::string assembled;
    SQLCHAR buf[1024];
    SQLLEN ind = 0;
    SQLRETURN rc;
    int guard = 0;
    do {
        rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
        ASSERT_TRUE(rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO)
            << "unexpected rc=" << rc;
        // Bytes copied this call = min(buffer-1, remaining). The driver always
        // NUL-terminates, so read up to the embedded NUL.
        assembled += std::string(reinterpret_cast<const char*>(buf));
        ASSERT_LT(++guard, 1000) << "PLP stream did not terminate";
    } while (rc == SQL_SUCCESS_WITH_INFO);

    EXPECT_EQ(SQL_SUCCESS, rc);
    EXPECT_EQ(static_cast<size_t>(kTotal), assembled.size());
    EXPECT_EQ(std::string(kTotal, 'A'), assembled);

    // Stream exhausted: a further call for the same column yields SQL_NO_DATA.
    rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_EQ(SQL_NO_DATA, rc);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}

// A NULL value reports SQL_NULL_DATA in the indicator with SQL_SUCCESS.
TEST_F(GetDataLiveTest, NullColumn) {
    ASSERT_SQL_OK(ExecDirect("SELECT CAST(NULL AS VARCHAR(10)) AS c1"),
                  SQL_HANDLE_STMT, stmt_);

    ASSERT_SQL_OK(SQLFetch(stmt_), SQL_HANDLE_STMT, stmt_);

    SQLCHAR buf[16] = {0};
    SQLLEN ind = 0;
    SQLRETURN rc = SQLGetData(stmt_, 1, SQL_C_CHAR, buf, sizeof(buf), &ind);
    EXPECT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
    EXPECT_EQ(SQL_NULL_DATA, ind);

    EXPECT_EQ(SQL_NO_DATA, SQLFetch(stmt_));
    SQLCloseCursor(stmt_);
}
