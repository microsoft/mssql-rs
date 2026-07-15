// Copyright (c) Microsoft Corporation. All rights reserved.
// odbc_test_fixture.h  –  Base fixture for ODBC Google Tests
//
// Provides automatic HENV / HDBC / HSTMT lifetime management,
// connection from environment variables, and assertion helpers.

#pragma once

#include <gtest/gtest.h>

#ifdef _WIN32
#include <windows.h>
#endif

#include <sql.h>
#include <sqlext.h>

// SQL_OV_ODBC3_80 was added in ODBC 3.80.  Older unixODBC headers may lack it.
#ifndef SQL_OV_ODBC3_80
#define SQL_OV_ODBC3_80 380UL
#endif

#include <string>
#include <vector>
#include <chrono>

/// SQLTCHAR-based string type for ODBC API calls.
using SqlTString = std::basic_string<SQLTCHAR>;

// ---------------------------------------------------------------------------
// Assertion helper macros
// ---------------------------------------------------------------------------

// Succeed on SQL_SUCCESS or SQL_SUCCESS_WITH_INFO
#define ASSERT_SQL_OK(rc, handle_type, handle)                                 \
    do {                                                                       \
        SQLRETURN _rc = (rc);                                                  \
        ASSERT_TRUE(SQL_SUCCEEDED(_rc))        \
            << "ODBC call failed with rc=" << _rc                              \
            << "  error=" << ODBCTestUtils::GetDiagMessage(                    \
                   handle_type, handle);                                       \
    } while (0)

#define EXPECT_SQL_OK(rc, handle_type, handle)                                 \
    do {                                                                       \
        SQLRETURN _rc = (rc);                                                  \
        EXPECT_TRUE(SQL_SUCCEEDED(_rc))        \
            << "ODBC call failed with rc=" << _rc                              \
            << "  error=" << ODBCTestUtils::GetDiagMessage(                    \
                   handle_type, handle);                                       \
    } while (0)

#define ASSERT_SQL_ERROR(expr)                                                 \
    do {                                                                       \
        SQLRETURN _rc = (expr);                                                \
        ASSERT_EQ(SQL_ERROR, _rc) << #expr " expected SQL_ERROR, got " << _rc; \
    } while (0)

#define EXPECT_SQL_ERROR(expr)                                                 \
    do {                                                                       \
        SQLRETURN _rc = (expr);                                                \
        EXPECT_EQ(SQL_ERROR, _rc) << #expr " expected SQL_ERROR, got " << _rc; \
    } while (0)

// Verify the 5-char SQLSTATE on the most recent diagnostic record.
// |handle_type| is SQL_HANDLE_ENV / SQL_HANDLE_DBC / SQL_HANDLE_STMT.
#define EXPECT_SQLSTATE(handle_type, handle, expected_state)                    \
    EXPECT_EQ(std::string(expected_state),                                     \
              ODBCTestUtils::GetDiagState(handle_type, handle))

// ---------------------------------------------------------------------------
// Forward declarations
// ---------------------------------------------------------------------------
class ODBCTestConfig;

// ---------------------------------------------------------------------------
// ODBCTestUtils  –  free helper functions
// ---------------------------------------------------------------------------
class ODBCTestUtils {
public:
    /// Retrieve the SQLSTATE string from the most recent diagnostic record.
    static std::string GetDiagState(SQLSMALLINT handleType, SQLHANDLE handle);

    /// Return true if |state| appears on ANY diagnostic record for the handle.
    ///
    /// Prefer this over GetDiagState when checking for a connection-string
    /// parse warning (01S00): on a successful connect the server posts its own
    /// 01000 "changed database context/language" info messages, and driver
    /// implementations differ on ordering. msodbcsql 18 appends the 01S00 parse
    /// warning AFTER the server messages (record #3), while mssql-odbc posts it
    /// first (record #1). GetDiagState only reads record #1, so it cannot see a
    /// warning that the server messages push further down the list.
    static bool HasDiagState(SQLSMALLINT handleType, SQLHANDLE handle,
                             const std::string& state);

    /// Retrieve the full diagnostic message text.
    static std::string GetDiagMessage(SQLSMALLINT handleType, SQLHANDLE handle);

    /// Build a connection string from ODBCTestConfig.
    static SqlTString BuildConnectionString();

    /// Convert narrow string to SQLTCHAR string (for ODBC API calls).
    static SqlTString ToSqlTStr(const std::string& s);

    /// Convert SQLTCHAR string to narrow string (for logging).
    static std::string ToNarrow(const SqlTString& s);
};

// ---------------------------------------------------------------------------
// ODBCTestConfig  –  connection info from environment variables
// ---------------------------------------------------------------------------
//   ODBC_TEST_DSN          – DSN name           (if set, connects via SQLConnect/DSN=)
//   ODBC_TEST_SERVER       – server hostname     (required when no DSN/CONNSTR)
//   ODBC_TEST_DATABASE     – database name       (default: tempdb)
//   ODBC_TEST_UID          – login               (optional – omit for integrated auth)
//   ODBC_TEST_PWD          – password            (optional)
//   ODBC_TEST_DRIVER       – driver name         (default: ODBC Driver 18 for SQL Server)
//   ODBC_TEST_CONNSTR      – full override conn string (if set, other vars ignored)
//   ODBC_TEST_TRUST_CERT   – TrustServerCertificate (default: Yes)
//   ODBC_TEST_ENCRYPT      – Encrypt value (e.g. Optional, Mandatory, Strict; default: empty = driver default)
// ---------------------------------------------------------------------------
class ODBCTestConfig {
public:
    static ODBCTestConfig& Instance();

    const std::string& DSN()         const { return dsn_; }
    const std::string& Server()      const { return server_; }
    const std::string& Database()    const { return database_; }
    const std::string& Uid()         const { return uid_; }
    const std::string& Pwd()         const { return pwd_; }
    const std::string& Driver()      const { return driver_; }
    const std::string& ConnStr()     const { return connstr_; }
    const std::string& TrustCert()   const { return trust_cert_; }
    const std::string& Encrypt()     const { return encrypt_; }

    bool HasDSN()         const { return !dsn_.empty(); }
    bool HasCredentials() const { return !uid_.empty(); }
    bool HasConnStr()     const { return !connstr_.empty(); }

    /// Returns true if ANY connection method is configured.
    bool HasConnection()  const { return HasConnStr() || HasDSN() || !server_.empty(); }

private:
    ODBCTestConfig();
    static std::string GetEnv(const char* name, const char* fallback = "");

    std::string dsn_;
    std::string server_;
    std::string database_;
    std::string uid_;
    std::string pwd_;
    std::string driver_;
    std::string connstr_;
    std::string trust_cert_;
    std::string encrypt_;
};

// ---------------------------------------------------------------------------
// ODBCTest  –  base test fixture
// ---------------------------------------------------------------------------
// Allocates HENV in SetUp().  Call Connect() to get HDBC + HSTMT.
// TearDown() cleans up everything.
// ---------------------------------------------------------------------------
class ODBCTest : public ::testing::Test {
protected:
    // --- Handles (available after the corresponding Alloc/Connect call) ---
    SQLHENV  env_  = SQL_NULL_HENV;
    SQLHDBC  dbc_  = SQL_NULL_HDBC;
    SQLHSTMT stmt_ = SQL_NULL_HSTMT;

    // --- Lifecycle ----------------------------------------------------------
    void SetUp() override;
    void TearDown() override;

    /// Allocate HDBC + connect using env-var config.  Also allocates one HSTMT.
    void Connect();

    /// Allocate an additional HSTMT on the current connection.
    SQLHSTMT AllocStmt();

    /// Free a specific HSTMT (stmt_ is freed automatically in TearDown).
    void FreeStmt(SQLHSTMT& hstmt);

    /// Execute a SQL statement on stmt_ and assert success.
    void ExecDirect(const std::string& sql);

    /// Execute a SQL statement, ignoring errors (useful for DROP IF EXISTS).
    void ExecDirectIgnoreError(const std::string& sql);

    // --- Logging ------------------------------------------------------------
    /// Print a timestamped, indented log message to stdout.
    void Log(const std::string& msg);

    // --- Helpers available to derived fixtures --------------------------------
    /// Shorthand to get diag state for stmt_.
    std::string StmtDiagState() {
        return ODBCTestUtils::GetDiagState(SQL_HANDLE_STMT, stmt_);
    }
    std::string DbcDiagState() {
        return ODBCTestUtils::GetDiagState(SQL_HANDLE_DBC, dbc_);
    }

private:
    std::vector<SQLHSTMT> extra_stmts_;
    std::chrono::steady_clock::time_point test_start_;
};
