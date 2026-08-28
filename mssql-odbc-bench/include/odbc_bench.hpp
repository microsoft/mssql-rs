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

    static Config from_environment();
    std::string connection_string() const;
};

struct WorkloadSpec {
    const char* benchmark_name;
    const char* scenario;
    const char* table_name;
    std::uint64_t row_count;
    std::size_t pattern_repetitions;

    std::size_t column_count() const;
};

const std::array<WorkloadSpec, 2>& workloads();

class OdbcSession {
public:
    explicit OdbcSession(const Config& config);
    ~OdbcSession();

    OdbcSession(const OdbcSession&) = delete;
    OdbcSession& operator=(const OdbcSession&) = delete;

    SQLHSTMT statement() const;
    void execute_non_query(const std::string& sql);
    std::uint64_t query_count(const std::string& qualified_table);

private:
    void release() noexcept;

    SQLHENV env_ = SQL_NULL_HENV;
    SQLHDBC dbc_ = SQL_NULL_HDBC;
    SQLHSTMT stmt_ = SQL_NULL_HSTMT;
};

void setup_benchmark_tables(OdbcSession& session);
void cleanup_benchmark_tables(OdbcSession& session);

struct RetrievalMetrics {
    std::uint64_t rows = 0;
    std::uint64_t cells = 0;
    std::uint64_t logical_bytes = 0;
    double total_seconds = 0.0;
    double execute_seconds = 0.0;
    double metadata_bind_seconds = 0.0;
    double fetch_seconds = 0.0;
};

class WorkloadRunner {
public:
    WorkloadRunner(OdbcSession& session, const WorkloadSpec& spec);
    ~WorkloadRunner();

    WorkloadRunner(const WorkloadRunner&) = delete;
    WorkloadRunner& operator=(const WorkloadRunner&) = delete;

    const WorkloadSpec& spec() const;
    std::uint64_t logical_bytes_per_row() const;
    void preflight();
    RetrievalMetrics retrieve();

private:
    class Impl;
    std::unique_ptr<Impl> impl_;
};

}  // namespace mssql::odbc::bench
