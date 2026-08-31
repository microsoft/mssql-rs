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
#include <ostream>
#include <string>

namespace mssql::odbc::bench {

/// Row-array sizes the bound workloads exercise.
///
/// These are not round numbers picked for symmetry: each one is a cadence the
/// only supported consumer actually produces. `mssql-python`'s `Cursor.arraysize`
/// defaults to 1, so an unconfigured `fetchmany()` binds a one-row rowset;
/// applications that raise it land in the tens; and `fetchall()` caps its
/// computed batch at 1000 rows (`FetchAll_wrap` in `ddbc_bindings.cpp`). The
/// harness previously hard-coded 1024, which is a size no consumer asks for.
inline constexpr std::size_t kFetchManyDefaultRowset = 1;
inline constexpr std::size_t kFetchManyCadenceRowset = 64;
inline constexpr std::size_t kFetchAllRowset = 1000;

/// Largest rowset any workload binds, used to size the shared row buffers.
inline constexpr std::size_t kMaxRowsetSize = kFetchAllRowset;

/// Buffer size `mssql-python` hands to every `SQLGetData` LOB continuation call
/// (`DAE_CHUNK_SIZE`). Matching it is what makes the chunked workload measure
/// the same number of driver round trips the consumer would cause.
inline constexpr std::size_t kLobChunkBytes = 8192;

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

/// Column families a benchmark table can be built from.
///
/// The shape decides schema generation, the deterministic value generator, and
/// the preflight validator together, so a table can never be filled by one rule
/// and checked by another.
enum class TableShape {
    /// Repeated fixed-width `NOT NULL` pattern; every value is inline and
    /// bindable, so it isolates conversion and rowset cost from length handling.
    fixed_pattern,
    /// Nullable inline `VARCHAR`/`NVARCHAR` with realistic length variation.
    /// Deliberately holds no `MAX` column: inline variable width and PLP are
    /// different driver paths and are measured separately.
    variable_width,
    /// `NVARCHAR(MAX)`/`VARCHAR(MAX)` values large enough that one value cannot
    /// be delivered in a single 8192-byte `SQLGetData` call.
    lob_max,
    /// The fixed pattern plus one small `NVARCHAR(MAX)` column. One MAX column
    /// is enough to make `mssql-python` abandon bound fetching for the *whole*
    /// result, so this measures the dispatch, not the LOB payload.
    mixed_lob,
    /// `sql_variant` columns, which require the zero-length `SQL_C_BINARY` probe
    /// plus `SQLColAttribute(SQL_CA_SS_VARIANT_TYPE)` before any value is read.
    sql_variant,
};

/// How a workload consumes its result set.
enum class AccessMode {
    /// Bind once, then `SQLFetchScroll` until `SQL_NO_DATA`. This is the
    /// steady-state block-fetch path.
    bound_drain,
    /// Rebind, fetch one rowset, reset the row-array size, and unbind — once per
    /// rowset. This is exactly what `mssql-python`'s `fetchmany()` does on every
    /// call, and at `arraysize = 1` the bind/unbind pair dominates the cost.
    bound_bind_cycle,
    /// `SQLFetch` with a one-row rowset and `SQLGetData` per column, which is the
    /// path a LOB or `sql_variant` column forces the entire result onto.
    row_wise_get_data,
};

/// One deterministic table: the setup executable creates exactly this catalog.
struct TableSpec {
    const char* table_name;
    TableShape shape;
    std::uint64_t row_count;
    /// Repeats of the 15-column fixed pattern; ignored by the other shapes.
    std::size_t pattern_repetitions;

    /// Return the exact projected width used to size buffers and verify metadata.
    std::size_t column_count() const;

    /// Zero-based index of the `INT` column carrying the row identity, which the
    /// preflight coverage check reads.
    std::size_t row_id_column() const;

    /// Driver-independent payload volume for one full retrieval, derived from the
    /// same generator formulas that fill the table. Computing it up front keeps
    /// per-cell accounting out of the timed loop for the variable-length shapes.
    std::uint64_t logical_bytes_total() const;
};

/// Defines one stable measurement whose name is also the comparison key.
///
/// The name encodes table shape, row count, column count, and access shape, so a
/// benchmark id stays meaningful in a report that no longer has the catalog next
/// to it — and stays identical across the candidate, baseline, and Microsoft legs.
struct WorkloadSpec {
    const char* benchmark_name;
    const char* scenario;
    const TableSpec* table;
    AccessMode access;
    /// Row-array size for the bound modes; always 1 for `row_wise_get_data`.
    std::size_t rowset_size;
};

/// Number of distinct tables the admin executable creates.
inline constexpr std::size_t kTableCount = 8;

/// Number of measured workloads in the catalog.
inline constexpr std::size_t kWorkloadCount = 13;

/// Return the fixed table catalog used by setup, cleanup, and validation.
const std::array<TableSpec, kTableCount>& tables();

/// Return the fixed workload catalog used by validation and measurement.
const std::array<WorkloadSpec, kWorkloadCount>& workloads();

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

/// Print the exact DDL, generator, and projection SQL for the whole catalog.
///
/// This needs no connection, so the generated T-SQL can be reviewed or replayed
/// against a server without provisioning the perf lab first — which is the only
/// way to check the generators offline, since they are assembled in C++.
void print_benchmark_sql(std::ostream& output);

/// Separates the end-to-end timing boundary from diagnostic phase timings.
///
/// Regression decisions use total_seconds; phase values help explain a change.
struct RetrievalMetrics {
    std::uint64_t rows = 0;
    std::uint64_t cells = 0;
    std::uint64_t logical_bytes = 0;
    /// `SQLGetData` calls issued, including LOB continuations and variant probes.
    /// Zero for the bound modes, where the driver moves whole rowsets instead.
    std::uint64_t get_data_calls = 0;
    double total_seconds = 0.0;
    double execute_seconds = 0.0;
    /// Time between execute and the start of fetching. Negative when the workload
    /// has no one-time metadata phase (`bind_cycle_*` re-describes and re-binds
    /// inside the fetch loop), in which case no counter is emitted.
    double metadata_bind_seconds = 0.0;
    double fetch_seconds = 0.0;
};

/// Drives one workload and validates that the timed work stays correct.
class WorkloadRunner {
public:
    /// Materialize the workload's column descriptors and row-array buffers.
    WorkloadRunner(OdbcSession& session, const WorkloadSpec& spec);
    ~WorkloadRunner();

    WorkloadRunner(const WorkloadRunner&) = delete;
    WorkloadRunner& operator=(const WorkloadRunner&) = delete;

    /// Return the catalog entry used as the stable Google Benchmark identity.
    const WorkloadSpec& spec() const;

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
