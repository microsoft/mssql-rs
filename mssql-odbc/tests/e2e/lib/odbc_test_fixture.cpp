// Copyright (c) Microsoft Corporation. All rights reserved.
// odbc_test_fixture.cpp  –  ODBCTest base fixture implementation.

#include "odbc_test_fixture.h"
#include <iostream>
#include <iomanip>
#include <cstdlib>

// ---------------------------------------------------------------------------
// SetUp / TearDown
// ---------------------------------------------------------------------------

void ODBCTest::SetUp() {
    test_start_ = std::chrono::steady_clock::now();

    // Allocate environment handle
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &env_);
    ASSERT_TRUE(SQL_SUCCEEDED(rc))
        << "SQLAllocHandle(ENV) failed, rc=" << rc;

    // Request ODBC 3.x behavior
    rc = SQLSetEnvAttr(env_, SQL_ATTR_ODBC_VERSION,
                       reinterpret_cast<SQLPOINTER>(SQL_OV_ODBC3_80), 0);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);
}

void ODBCTest::TearDown() {
    // Free extra statements
    for (auto& h : extra_stmts_) {
        if (h != SQL_NULL_HSTMT) {
            SQLFreeHandle(SQL_HANDLE_STMT, h);
            h = SQL_NULL_HSTMT;
        }
    }
    extra_stmts_.clear();

    // Free primary statement
    if (stmt_ != SQL_NULL_HSTMT) {
        SQLFreeHandle(SQL_HANDLE_STMT, stmt_);
        stmt_ = SQL_NULL_HSTMT;
    }

    // Disconnect and free connection
    if (dbc_ != SQL_NULL_HDBC) {
        SQLDisconnect(dbc_);
        SQLFreeHandle(SQL_HANDLE_DBC, dbc_);
        dbc_ = SQL_NULL_HDBC;
    }

    // Free environment
    if (env_ != SQL_NULL_HENV) {
        SQLFreeHandle(SQL_HANDLE_ENV, env_);
        env_ = SQL_NULL_HENV;
    }
}

// ---------------------------------------------------------------------------
// Connect
// ---------------------------------------------------------------------------

void ODBCTest::Connect() {
    ASSERT_TRUE(env_ != SQL_NULL_HENV) << "Call SetUp() before Connect()";

    // Allocate connection handle
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_DBC, env_, &dbc_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_ENV, env_);

    // Connect
    SqlTString connstr = ODBCTestUtils::BuildConnectionString();
    SQLTCHAR outStr[1024] = {};
    SQLSMALLINT outLen = 0;

    rc = SQLDriverConnect(dbc_, nullptr,
                          const_cast<SQLTCHAR*>(connstr.c_str()),
                          static_cast<SQLSMALLINT>(connstr.size()),
                          outStr,
                          static_cast<SQLSMALLINT>(sizeof(outStr) / sizeof(SQLTCHAR)),
                          &outLen, SQL_DRIVER_NOPROMPT);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);

    // Allocate a default statement handle
    rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &stmt_);
    ASSERT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
}

// ---------------------------------------------------------------------------
// Statement helpers
// ---------------------------------------------------------------------------

SQLHSTMT ODBCTest::AllocStmt() {
    SQLHSTMT h = SQL_NULL_HSTMT;
    SQLRETURN rc = SQLAllocHandle(SQL_HANDLE_STMT, dbc_, &h);
    EXPECT_SQL_OK(rc, SQL_HANDLE_DBC, dbc_);
    if (h != SQL_NULL_HSTMT) {
        extra_stmts_.push_back(h);
    }
    return h;
}

void ODBCTest::FreeStmt(SQLHSTMT& hstmt) {
    if (hstmt != SQL_NULL_HSTMT) {
        SQLFreeHandle(SQL_HANDLE_STMT, hstmt);
        // Remove from tracking vector
        for (auto it = extra_stmts_.begin(); it != extra_stmts_.end(); ++it) {
            if (*it == hstmt) {
                extra_stmts_.erase(it);
                break;
            }
        }
        hstmt = SQL_NULL_HSTMT;
    }
}

void ODBCTest::ExecDirect(const std::string& sql) {
    SqlTString tsql = ODBCTestUtils::ToSqlTStr(sql);
    SQLRETURN rc = SQLExecDirect(stmt_,
                                 const_cast<SQLTCHAR*>(tsql.c_str()),
                                 SQL_NTS);
    SCOPED_TRACE("ExecDirect(\"" + sql + "\")");
    ASSERT_SQL_OK(rc, SQL_HANDLE_STMT, stmt_);
}

void ODBCTest::ExecDirectIgnoreError(const std::string& sql) {
    SqlTString tsql = ODBCTestUtils::ToSqlTStr(sql);
    SQLExecDirect(stmt_,
                  const_cast<SQLTCHAR*>(tsql.c_str()),
                  SQL_NTS);
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------
void ODBCTest::Log(const std::string& msg) {
    auto now = std::chrono::steady_clock::now();
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
        now - test_start_).count();
    std::cout << "    [" << std::setw(6) << ms << "ms] " << msg << std::endl;
}
