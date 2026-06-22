// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDS client performance benchmarks.
//!
//! Targets:
//! - `connect_only`            — connect + immediate disconnect; isolates the
//!   TLS handshake / login cost (no query).
//! - `fetch_large_encrypted`   — single long-lived connection fetching a large
//!   (~160 MB by default) payload; isolates the encrypt/decrypt + row-parse
//!   path with handshake cost excluded.
//! - `iter_rows`               — many small mixed-type rows over a fresh
//!   connection each iteration.
//! - `connect_fetch_multiple_packets` — connect + multi-result-set fetch.
//!
//! Environment knobs:
//! - `DB_HOST`, `DB_PORT`, `DB_USERNAME`, `SQL_PASSWORD` — connection target.
//! - `TRUST_SERVER_CERTIFICATE` (`true`/`false`), `CERT_HOST_NAME` — TLS validation.
//! - `BENCH_ENCRYPT` (`strict`|`on`|`off`, default `on`) — encryption setting.
//! - `BENCH_SECS` (default `30`) — Criterion measurement time, seconds.
//! - `BENCH_SAMPLES` (default `10`) — Criterion sample size.
//! - `BENCH_FETCH_ROWS` (default `20000`) — rows for `fetch_large_encrypted`
//!   (each row ≈ 8 KB, so total bytes ≈ rows × 8 KB).

use std::env;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::{
    connection::{
        client_context::ClientContext,
        tds_client::{ResultSet, ResultSetClient, TdsClient},
    },
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
};

static QUERY_TO_BENCHMARK: &str = "SELECT * FROM sys.databases; select * from sys.columns";

const ROW_COUNT: u64 = 10_000;

/// Cross-join sys.columns to produce ROW_COUNT rows with 20 columns (mixed types).
fn iter_rows_query() -> String {
    format!(
        r#"SELECT TOP ({ROW_COUNT})
    c1.object_id, c1.name, c1.column_id, c1.system_type_id,
    c1.user_type_id, c1.max_length, c1.precision, c1.scale,
    c1.collation_name, c1.is_nullable, c1.is_ansi_padded, c1.is_rowguidcol,
    c1.is_identity, c1.is_computed, c1.is_filestream, c1.is_replicated,
    c1.is_non_sql_subscribed, c1.is_merge_published, c1.is_dts_replicated, c1.is_xml_document
FROM sys.columns c1
CROSS JOIN sys.columns c2
ORDER BY c1.object_id, c1.column_id"#
    )
}

fn fetch_large_encrypted(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let fetch_rows: u64 = env::var("BENCH_FETCH_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    // Each row carries an ~8 KB payload, so total bytes through the decrypt
    // path ≈ fetch_rows * 8 KB. Default ≈ 160 MB.
    let payload_len: u64 = 8000;
    let query = format!(
        "SELECT TOP ({fetch_rows}) REPLICATE(CAST('X' AS VARCHAR(MAX)), {payload_len}) AS payload \
         FROM sys.all_columns a CROSS JOIN sys.all_columns b"
    );

    // Connect ONCE, outside the timing loop, so we measure only the encrypted
    // fetch + decrypt path, not handshake/login.
    let mut client = match try_connect(&rt) {
        Some(c) => c,
        None => {
            eprintln!(
                "fetch_large_encrypted: skipped — DB unreachable or connection env not set \
                 (expected when running benches without a server)"
            );
            return;
        }
    };

    let mut group = c.benchmark_group("fetch_large_encrypted");
    group.throughput(Throughput::Bytes(fetch_rows * payload_len));
    group.bench_function("fetch_large_encrypted", |b| {
        b.iter(|| {
            rt.block_on(async {
                client.execute(query.clone(), None, None).await.unwrap();
                let mut _row_count = 0u64;
                loop {
                    while client.next_row().await.unwrap().is_some() {
                        _row_count += 1;
                    }
                    if !client.move_to_next().await.unwrap() {
                        break;
                    }
                }
            });
        })
    });
    group.finish();
}

fn connect_only(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Criterion runs each benchmark's closure once when the binary is invoked
    // as a test (e.g. `nextest` in CI, where no DB is reachable). Probe first
    // and skip gracefully instead of panicking inside the timing loop.
    if try_connect(&rt).is_none() {
        eprintln!(
            "connect_only: skipped — DB unreachable or connection env not set \
             (expected when running benches without a server)"
        );
        return;
    }

    c.bench_function("connect_only", |b| {
        b.iter(|| {
            let context = create_context();
            rt.block_on(async move {
                let provider = TdsConnectionProvider {};
                let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
                let port = env::var("DB_PORT")
                    .expect("DB_PORT environment variable not set")
                    .parse::<u16>()
                    .expect("DB_PORT must be a valid u16");
                let datasource = format!("tcp:{},{}", host, port);
                let client = provider
                    .create_client(context, &datasource, None)
                    .await
                    .unwrap();
                drop(client);
            });
        })
    });
}

fn iter_rows(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("row_iteration");
    group.throughput(Throughput::Elements(ROW_COUNT));
    group.bench_function("iter_rows", |b| {
        b.iter(|| {
            let context = create_context();
            rt.block_on(async move {
                let provider = TdsConnectionProvider {};
                let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
                let port = env::var("DB_PORT")
                    .expect("DB_PORT environment variable not set")
                    .parse::<u16>()
                    .expect("DB_PORT must be a valid u16");
                let datasource = format!("tcp:{},{}", host, port);
                let mut client = provider
                    .create_client(context, &datasource, None)
                    .await
                    .unwrap();

                client.execute(iter_rows_query(), None, None).await.unwrap();

                let mut _row_count = 0u64;
                loop {
                    while client.next_row().await.unwrap().is_some() {
                        _row_count += 1;
                    }
                    if !client.move_to_next().await.unwrap() {
                        break;
                    }
                }
            });
        })
    });
    group.finish();
}

fn connect_fetch_multiple_packets(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    c.bench_function("connect_fetch_multiple_packets", |b| {
        b.iter(|| {
            let context = create_context();
            rt.block_on(async move {
                let provider = TdsConnectionProvider {};
                let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
                let port = env::var("DB_PORT")
                    .expect("DB_PORT environment variable not set")
                    .parse::<u16>()
                    .expect("DB_PORT must be a valid u16");
                let datasource = format!("tcp:{},{}", host, port);
                let mut client = provider
                    .create_client(context, &datasource, None)
                    .await
                    .unwrap();

                let mut _row_count = 0;
                client
                    .execute(QUERY_TO_BENCHMARK.to_string(), None, None)
                    .await
                    .unwrap();
                // let mut row_count = 0;
                loop {
                    while client.next_row().await.unwrap().is_some() {
                        _row_count += 1;
                    }

                    if !client.move_to_next().await.unwrap() {
                        break;
                    }
                }
            });
        })
    });
}

/// Probe whether a benchmark DB is reachable, returning the connected client on
/// success. Returns `None` when connection env/credentials are missing or the
/// connect fails. Lets DB-dependent benches skip gracefully when Criterion runs
/// their closures once as a test (e.g. `nextest` in CI with no server) instead
/// of panicking.
fn try_connect(rt: &tokio::runtime::Runtime) -> Option<TdsClient> {
    rt.block_on(async {
        dotenv::dotenv().ok();
        let host = env::var("DB_HOST").ok()?;
        let port = env::var("DB_PORT").ok()?.parse::<u16>().ok()?;
        env::var("DB_USERNAME").ok()?;
        if env::var("SQL_PASSWORD").is_err() && std::fs::read_to_string("/tmp/password").is_err() {
            return None;
        }
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let datasource = format!("tcp:{host},{port}");
        provider
            .create_client(context, &datasource, None)
            .await
            .ok()
    })
}

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

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(std::time::Duration::from_secs(
            std::env::var("BENCH_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        ))
        .sample_size(
            std::env::var("BENCH_SAMPLES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
        );
    targets = fetch_large_encrypted, connect_only, iter_rows, connect_fetch_multiple_packets
}
criterion_main!(benches);
