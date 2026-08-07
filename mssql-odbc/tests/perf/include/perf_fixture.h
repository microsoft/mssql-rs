// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// perf_fixture.h  –  Shared plumbing for the ODBC performance benchmarks.
//
// The benchmarks drive the driver through the ODBC Driver Manager, so the exact
// same binary measures mssql-odbc and msodbcsql18. Which driver is exercised is
// decided purely by the `Driver={...}` name in the connection string
// (ODBC_TEST_DRIVER), so a comparison run never has to touch the registry.

#pragma once

#ifdef _WIN32
#include <windows.h>
#endif

#include <sql.h>
#include <sqlext.h>

#include <benchmark/benchmark.h>

#include <cstdint>
#include <string>

#ifndef SQL_OV_ODBC3_80
#define SQL_OV_ODBC3_80 380UL
#endif

/// SQLTCHAR-based string type for ODBC API calls.
using SqlTString = std::basic_string<SQLTCHAR>;

namespace perf {

SqlTString ToSqlTStr(const std::string& s);
std::string ToNarrow(const SqlTString& s);

/// Concatenated SQLSTATE + message text for every diagnostic record on |handle|.
std::string DiagText(SQLSMALLINT handle_type, SQLHANDLE handle);

// ---------------------------------------------------------------------------
// Config  –  connection info from environment variables
// ---------------------------------------------------------------------------
// Shares the ODBC_TEST_* contract with tests/e2e so one environment drives both
// suites. ODBC_TEST_DRIVER is what selects the driver under measurement.
//
//   ODBC_TEST_SERVER     – server hostname       (required unless CONNSTR/DSN)
//   ODBC_TEST_DATABASE   – database name         (default: tempdb)
//   ODBC_TEST_UID/PWD    – SQL login             (omit for integrated auth)
//   ODBC_TEST_DRIVER     – driver name           (default: ODBC Driver 18 for SQL Server)
//   ODBC_TEST_DSN        – DSN name              (overrides driver/server)
//   ODBC_TEST_CONNSTR    – full connection string (overrides everything above)
//   ODBC_TEST_TRUST_CERT – TrustServerCertificate (default: Yes)
//   ODBC_TEST_ENCRYPT    – Encrypt value          (default: driver default)
// ---------------------------------------------------------------------------
class Config {
public:
    static const Config& Instance();

    const std::string& Driver() const { return driver_; }
    bool HasConnection() const {
        return !connstr_.empty() || !dsn_.empty() || !server_.empty();
    }

    /// Connection string assembled from the environment.
    SqlTString ConnectionString() const;

private:
    Config();
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
// RAII handle wrappers
// ---------------------------------------------------------------------------

/// ODBC environment handle set to ODBC 3.80.
class Env {
public:
    Env();
    ~Env();
    Env(const Env&) = delete;
    Env& operator=(const Env&) = delete;

    SQLHENV get() const { return env_; }
    bool ok() const { return env_ != SQL_NULL_HENV; }

private:
    SQLHENV env_ = SQL_NULL_HENV;
};

/// A connected HDBC plus one HSTMT, both released on destruction.
class Conn {
public:
    explicit Conn(const Env& env);
    ~Conn();
    Conn(const Conn&) = delete;
    Conn& operator=(const Conn&) = delete;

    SQLHDBC dbc() const { return dbc_; }
    SQLHSTMT stmt() const { return stmt_; }
    bool ok() const { return connected_ && stmt_ != SQL_NULL_HSTMT; }

    /// Diagnostic text explaining why ok() is false, or why the last Exec failed.
    const std::string& error() const { return error_; }

    /// Run a statement, draining every row and result set it produces.
    bool Exec(const std::string& sql);

private:
    SQLHDBC dbc_ = SQL_NULL_HDBC;
    SQLHSTMT stmt_ = SQL_NULL_HSTMT;
    bool connected_ = false;
    std::string error_;
};

/// Read every row/column of the current result set via SQLGetData and return the
/// number of rows consumed, or -1 on error.
///
/// SQLGetData is used rather than SQLBindCol because mssql-odbc does not export
/// SQLBindCol yet; keeping both drivers on the same retrieval path is what makes
/// the comparison meaningful.
int64_t DrainRows(SQLHSTMT stmt, std::string* error);

/// Advances the cursor to exhaustion without retrieving any column data.
///
/// Isolates the per-row cost of the cursor and token-stream path from the
/// per-column `SQLGetData` conversion cost measured by `DrainRows`.
int64_t DrainRowsNoGetData(SQLHSTMT stmt, std::string* error);

/// Close the cursor so the statement can be reused for the next iteration.
void CloseCursor(SQLHSTMT stmt);

/// Bail out of a benchmark with a diagnostic instead of reporting bogus timings.
#define PERF_REQUIRE(cond, state, msg)  \
    do {                                \
        if (!(cond)) {                  \
            (state).SkipWithError(msg); \
            return;                     \
        }                               \
    } while (0)

}  // namespace perf
