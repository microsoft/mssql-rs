// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared support for the `mssql-tds-bench` Criterion harness.
//!
//! This module centralizes connection setup, environment configuration, and
//! Criterion tuning so every benchmark file stays small and identical in
//! behavior. Keeping this logic in one place is what lets a baseline build and
//! a candidate build be compared with `mssql-tds` as the only variable.
//!
//! ## Environment contract (shared with `mssql-tds/benches/perf.rs`)
//! - `DB_HOST`, `DB_PORT`, `DB_USERNAME`, `SQL_PASSWORD` — connection target.
//!   `SQL_PASSWORD` falls back to the contents of `/tmp/password`.
//! - `TRUST_SERVER_CERTIFICATE` (`true`/`false`), `CERT_HOST_NAME` — TLS validation.
//! - `BENCH_ENCRYPT` (`strict`|`on`|`off`, default `on`) — encryption setting.
//!
//! ## Criterion tuning knobs
//! - `BENCH_WARMUP_SECS` (default `5`) — per-benchmark warm-up time, seconds.
//!   Longer than Criterion's 3s default so the SQL plan cache, buffer pool, and
//!   tempdb reach steady state before measurement on a colocated server.
//! - `BENCH_SECS` (default `20`) — per-benchmark measurement time, seconds.
//! - `BENCH_SAMPLES` (default `20`) — sample size. More samples tighten the
//!   confidence interval so small real deltas separate from run noise.
//! - `BENCH_SIGNIFICANCE` (default `0.05`) — significance level.
//! - `BENCH_NOISE` (default `0.05`) — noise threshold. These end-to-end,
//!   network-bound benchmarks are noisier than CPU microbenchmarks, so the
//!   defaults are deliberately relaxed to avoid false regression alarms.

use std::env;
use std::time::Duration;

use criterion::Criterion;
use mssql_tds::{
    connection::{
        client_context::ClientContext,
        tds_client::{ResultSet, ResultSetClient, TdsClient},
    },
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
    datatypes::column_values::ColumnValues,
};

/// Connection target resolved from the environment.
#[derive(Clone)]
pub struct BenchEnv {
    pub host: String,
    pub port: u16,
}

impl BenchEnv {
    /// TDS datasource string, e.g. `tcp:localhost,1433`.
    pub fn datasource(&self) -> String {
        format!("tcp:{},{}", self.host, self.port)
    }
}

/// Resolve the connection target, returning `None` when the required
/// environment variables or credentials are missing. Lets benchmarks skip
/// gracefully when Criterion runs their closures once as a test (e.g. in CI
/// with no server) instead of panicking.
pub fn bench_env() -> Option<BenchEnv> {
    dotenv::dotenv().ok();
    let host = env::var("DB_HOST").ok()?;
    let port = env::var("DB_PORT").ok()?.parse::<u16>().ok()?;
    env::var("DB_USERNAME").ok()?;
    if env::var("SQL_PASSWORD").is_err() && std::fs::read_to_string("/tmp/password").is_err() {
        return None;
    }
    Some(BenchEnv { host, port })
}

/// Build a [`ClientContext`] from the environment.
///
/// Mirrors `create_context()` in `mssql-tds/benches/perf.rs` so the two
/// harnesses connect identically.
pub fn create_context() -> ClientContext {
    dotenv::dotenv().ok();
    let mut context = ClientContext::default();
    context.user_name = env::var("DB_USERNAME").expect("DB_USERNAME environment variable not set");
    context.password = env::var("SQL_PASSWORD")
        .or_else(|_| {
            std::fs::read_to_string("/tmp/password")
                .map(|s| s.trim().to_string())
                .map_err(|_| std::env::VarError::NotPresent)
        })
        .expect("SQL_PASSWORD environment variable not set and /tmp/password could not be read");
    context.encryption_options = EncryptionOptions {
        mode: env::var("BENCH_ENCRYPT")
            .ok()
            .and_then(|v| match v.to_ascii_lowercase().as_str() {
                "strict" => Some(EncryptionSetting::Strict),
                "on" => Some(EncryptionSetting::On),
                "off" => Some(EncryptionSetting::PreferOff),
                _ => None,
            })
            .unwrap_or(EncryptionSetting::On),
        trust_server_certificate: env::var("TRUST_SERVER_CERTIFICATE")
            .map(|v| v.parse().unwrap_or(false))
            .unwrap_or(false),
        host_name_in_cert: env::var("CERT_HOST_NAME").ok(),
        server_certificate: None,
    };
    context
}

/// Build a single-threaded-capable multi-thread tokio runtime for the harness.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("failed to build tokio runtime")
}

/// Connect a fresh [`TdsClient`] against the configured datasource.
pub async fn connect(env: &BenchEnv) -> TdsClient {
    let provider = TdsConnectionProvider {};
    provider
        .create_client(create_context(), &env.datasource(), None)
        .await
        .expect("failed to connect to SQL Server")
}

/// Connect with an explicit requested TDS packet size (512–32768).
pub async fn connect_with_packet_size(env: &BenchEnv, packet_size: u16) -> TdsClient {
    let mut context = create_context();
    context.packet_size = packet_size;
    let provider = TdsConnectionProvider {};
    provider
        .create_client(context, &env.datasource(), None)
        .await
        .expect("failed to connect to SQL Server")
}

/// Probe whether the benchmark DB is reachable, returning a connected client on
/// success and `None` otherwise. Use at the top of a benchmark to skip
/// gracefully when no server is available.
pub fn try_connect(rt: &tokio::runtime::Runtime, bench_name: &str) -> Option<TdsClient> {
    let env = bench_env()?;
    let client = rt.block_on(async {
        let provider = TdsConnectionProvider {};
        provider
            .create_client(create_context(), &env.datasource(), None)
            .await
            .ok()
    });
    if client.is_none() {
        eprintln!(
            "{bench_name}: skipped — DB unreachable or connection env not set \
             (expected when running benches without a server)"
        );
    }
    client
}

/// Run a statement and drain every row of every result set, then close the
/// batch so the connection can be reused. Returns the total row count.
pub async fn drain(client: &mut TdsClient) -> u64 {
    let mut rows = 0u64;
    loop {
        while client.next_row().await.expect("next_row failed").is_some() {
            rows += 1;
        }
        if !client.move_to_next().await.expect("move_to_next failed") {
            break;
        }
    }
    client.close_query().await.expect("close_query failed");
    rows
}

/// Like [`drain`], but captures the prepared-statement handle — the first
/// integer return value carried by the response — before
/// [`close_query`](TdsClient::close_query) clears the return-value buffer.
///
/// Used by the `sp_prepexec` benchmark to release the handle it just created so
/// server-side prepared state does not accumulate across iterations (which would
/// drift the measurement upward).
pub async fn drain_capture_handle(client: &mut TdsClient) -> i32 {
    loop {
        while client.next_row().await.expect("next_row failed").is_some() {}
        if !client.move_to_next().await.expect("move_to_next failed") {
            break;
        }
    }
    let handle = client
        .get_return_values()
        .into_iter()
        .find_map(|rv| match rv.value {
            ColumnValues::Int(h) => Some(h),
            _ => None,
        })
        .expect("prepared handle not found in return values");
    client.close_query().await.expect("close_query failed");
    handle
}

/// Create a session temp table `table` filled with `rows` deterministic rows of
/// eight mixed-type columns, using a single set-based `GENERATE_SERIES` insert.
///
/// Population is a one-time, un-measured cost. The table is a heap (no indexes,
/// no sort), so a later `SELECT ... FROM table` is a plain scan whose content is
/// fixed and whose server-side cost is trivial — isolating driver decode from
/// database query execution. The columns are:
/// `c_int, c_bigint, c_bit, c_tinyint, c_smallint, c_nvarchar, c_float, c_datetime2`.
///
/// Requires SQL Server 2022+ (database compatibility level 160) for
/// `GENERATE_SERIES`.
pub async fn create_mixed_rows_table(client: &mut TdsClient, table: &str, rows: u64) {
    let ddl = format!(
        "CREATE TABLE {table} (\
            c_int INT NOT NULL, \
            c_bigint BIGINT NOT NULL, \
            c_bit BIT NOT NULL, \
            c_tinyint TINYINT NOT NULL, \
            c_smallint SMALLINT NOT NULL, \
            c_nvarchar NVARCHAR(128) NOT NULL, \
            c_float FLOAT NOT NULL, \
            c_datetime2 DATETIME2 NOT NULL)"
    );
    client
        .execute(ddl, None, None)
        .await
        .expect("create rows table failed");
    client.close_query().await.expect("close_query failed");

    let fill = format!(
        "INSERT INTO {table} \
            (c_int, c_bigint, c_bit, c_tinyint, c_smallint, c_nvarchar, c_float, c_datetime2) \
         SELECT CAST(value AS INT), \
                CAST(value AS BIGINT), \
                CAST(value % 2 AS BIT), \
                CAST(value % 256 AS TINYINT), \
                CAST(value % 32768 AS SMALLINT), \
                CONCAT(N'row_', value), \
                CAST(value AS FLOAT) * 1.5, \
                DATEADD(SECOND, value % 86400, CAST('2020-01-01T00:00:00' AS DATETIME2)) \
         FROM GENERATE_SERIES(CAST(1 AS BIGINT), CAST({rows} AS BIGINT))"
    );
    client
        .execute(fill, None, None)
        .await
        .expect("fill rows table failed");
    client.close_query().await.expect("close_query failed");
}

/// Build a Criterion instance tuned for network-bound, end-to-end benchmarks.
pub fn criterion_config() -> Criterion {
    fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
        env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    Criterion::default()
        .warm_up_time(Duration::from_secs(env_parse("BENCH_WARMUP_SECS", 5)))
        .measurement_time(Duration::from_secs(env_parse("BENCH_SECS", 20)))
        .sample_size(env_parse("BENCH_SAMPLES", 20usize))
        .significance_level(env_parse("BENCH_SIGNIFICANCE", 0.05))
        .noise_threshold(env_parse("BENCH_NOISE", 0.05))
}
