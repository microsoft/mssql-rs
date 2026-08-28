// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#pragma once

#ifdef _WIN32
#include <windows.h>
#endif

#include <sql.h>
#include <sqlext.h>

#include <array>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>

namespace mssql::odbc::bench {

inline constexpr std::size_t kRowArraySize = 1024;

/// Connection and workload selection shared by the admin and timed executables.
///
/// Keeping this environment-driven lets the same binaries compare three drivers
/// without recompilation while the runner controls every connection attribute.
struct Config {
    std::string driver;
    std::string server;
    std::string database;
    std::string uid;
    std::string pwd;
    std::string trust_certificate;
    std::string encrypt;
    std::string packet_size;
    std::string packet_size_keyword;
    std::string scenario;

    /// Read and validate the cross-platform perf-lab environment contract.
    static Config from_environment();

    /// Build a braced ODBC string so credentials and names remain single values.
    std::string connection_string() const;
};

/// Defines one stable SQL shape whose name is also the comparison key.
///
/// The pattern repetition count changes result width without changing the type mix.
struct WorkloadSpec {
    const char* benchmark_name;
    const char* scenario;
    const char* table_name;
    std::uint64_t row_count;
    std::size_t pattern_repetitions;

    /// Return the exact projected width used to size buffers and verify metadata.
    std::size_t column_count() const;
};

/// Return the fixed workload catalog used by setup, validation, and measurement.
const std::array<WorkloadSpec, 2>& workloads();

/// Owns one ODBC handle chain so every workload uses the same connection lifecycle.
class OdbcSession {
public:
    /// Connect using ODBC 3.8 and allocate the reusable statement handle.
    explicit OdbcSession(const Config& config);
    ~OdbcSession();

    OdbcSession(const OdbcSession&) = delete;
    OdbcSession& operator=(const OdbcSession&) = delete;

    /// Expose the statement only to the runner that controls its complete lifecycle.
    SQLHSTMT statement() const;

    /// Execute setup/cleanup SQL and drain every result before reusing the statement.
    void execute_non_query(const std::string& sql);

    /// Verify setup row counts through the same driver path used by the benchmark.
    std::uint64_t query_count(const std::string& qualified_table);

private:
    /// Release child-before-parent and remain safe during partial construction.
    void release() noexcept;

    SQLHENV env_ = SQL_NULL_HENV;
    SQLHDBC dbc_ = SQL_NULL_HDBC;
    SQLHSTMT stmt_ = SQL_NULL_HSTMT;
};

/// Recreate deterministic tables and verify their row counts before timing begins.
void setup_benchmark_tables(OdbcSession& session);

/// Remove benchmark-owned tables so shared perf databases do not accumulate state.
void cleanup_benchmark_tables(OdbcSession& session);

/// Separates the end-to-end timing boundary from diagnostic phase timings.
///
/// Regression decisions use total_seconds; phase values help explain a change.
struct RetrievalMetrics {
    std::uint64_t rows = 0;
    std::uint64_t cells = 0;
    std::uint64_t logical_bytes = 0;
    double total_seconds = 0.0;
    double execute_seconds = 0.0;
    double metadata_bind_seconds = 0.0;
    double fetch_seconds = 0.0;
};

/// Binds one workload once per retrieval and validates that timed work stays correct.
class WorkloadRunner {
public:
    /// Materialize the workload's column descriptors and row-array buffers.
    WorkloadRunner(OdbcSession& session, const WorkloadSpec& spec);
    ~WorkloadRunner();

    WorkloadRunner(const WorkloadRunner&) = delete;
    WorkloadRunner& operator=(const WorkloadRunner&) = delete;

    /// Return the catalog entry used as the stable Google Benchmark identity.
    const WorkloadSpec& spec() const;

    /// Report payload volume independent of driver-specific physical representation.
    std::uint64_t logical_bytes_per_row() const;

    /// Exercise a full untimed fetch and validate rowsets, indicators, and values.
    void preflight();

    /// Measure execution through complete result consumption with validation disabled.
    RetrievalMetrics retrieve();

private:
    /// Hide ODBC headers and mutable fetch state from benchmark registration code.
    class Impl;
    std::unique_ptr<Impl> impl_;
};

}  // namespace mssql::odbc::bench
