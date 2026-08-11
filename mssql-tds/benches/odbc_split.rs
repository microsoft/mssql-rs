// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Attribution benchmark: splits the mssql-odbc per-row cost between the TDS
//! stack and the ODBC glue.
//!
//! The C++ `datatype_bench` measures the full ODBC path (`SQLFetch` +
//! `SQLGetData`) and reports ~2.0 µs/row for `INT` against ~0.46 µs/row for
//! msodbcsql18. Decoding four bytes costs a handful of nanoseconds, so
//! essentially all of that is overhead — but the C++ harness cannot say whether
//! the overhead lives below the ODBC boundary (async machinery in `mssql-tds`)
//! or above it (handle mutexes, diagnostics, text rendering).
//!
//! These cases run the *same query* the ODBC `BM_Type_Int` case runs, straight
//! against `TdsClient`, with no ODBC layer:
//!
//! - `tds_cursor_column` — `next_row_cursor()` + `read_row_column(0)`, the exact
//!   pair `SQLFetch`/`SQLGetData` drive. Subtracting this from the ODBC number
//!   leaves the ODBC glue cost.
//! - `tds_next_row` — whole-row `next_row()`, for reference: the non-cursor path
//!   that does not pause per column.
//!
//! Environment knobs match `perf.rs` (`DB_HOST`, `DB_PORT`, `DB_USERNAME`,
//! `SQL_PASSWORD`, `BENCH_ENCRYPT`, `TRUST_SERVER_CERTIFICATE`). Set
//! `BENCH_ENCRYPT=off` to match the ODBC harness's `Encrypt=Optional`.

use std::env;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::{
    connection::{
        client_context::ClientContext,
        tds_client::{CursorColumn, ResultSet, TdsClient},
    },
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
};

/// Matches `kRows` in `mssql-odbc/tests/perf/benches/datatype_bench.cpp`.
const ROWS: u64 = 5000;

/// Byte-for-byte the query behind `BM_Type_Int`.
fn int_query() -> String {
    format!(
        "SELECT TOP ({ROWS}) CAST(a.object_id AS INT) AS v \
         FROM sys.all_objects a CROSS JOIN sys.all_objects b"
    )
}

fn create_context() -> ClientContext {
    dotenv::dotenv().ok();
    let mut context = ClientContext::default();
    context.user_name = env::var("DB_USERNAME").expect("DB_USERNAME environment variable not set");
    context.password = env::var("SQL_PASSWORD").expect("SQL_PASSWORD environment variable not set");
    context.encryption_options = EncryptionOptions {
        mode: env::var("BENCH_ENCRYPT")
            .ok()
            .and_then(|v| match v.to_ascii_lowercase().as_str() {
                "strict" => Some(EncryptionSetting::Strict),
                "on" => Some(EncryptionSetting::On),
                "off" => Some(EncryptionSetting::PreferOff),
                _ => None,
            })
            .unwrap_or(EncryptionSetting::PreferOff),
        trust_server_certificate: env::var("TRUST_SERVER_CERTIFICATE")
            .map(|v| v.parse().unwrap_or(true))
            .unwrap_or(true),
        host_name_in_cert: env::var("CERT_HOST_NAME").ok(),
        server_certificate: None,
    };
    context
}

fn try_connect(rt: &tokio::runtime::Runtime) -> Option<TdsClient> {
    rt.block_on(async {
        dotenv::dotenv().ok();
        let host = env::var("DB_HOST").ok()?;
        let port = env::var("DB_PORT").ok()?.parse::<u16>().ok()?;
        env::var("DB_USERNAME").ok()?;
        env::var("SQL_PASSWORD").ok()?;
        let provider = TdsConnectionProvider {};
        let datasource = format!("tcp:{host},{port}");
        provider
            .create_client(create_context(), &datasource, None)
            .await
            .ok()
    })
}

fn odbc_split(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Criterion runs each closure once when the binary is invoked as a test
    // (nextest in CI, where no DB is reachable), so probe and skip gracefully
    // rather than panicking inside the timing loop.
    let Some(mut client) = try_connect(&rt) else {
        eprintln!("odbc_split: skipped — DB unreachable or connection env not set");
        return;
    };

    let query = int_query();

    let mut group = c.benchmark_group("odbc_split");
    group.throughput(Throughput::Elements(ROWS));

    group.bench_function("tds_cursor_column", |b| {
        b.iter(|| {
            rt.block_on(async {
                client.execute(query.clone(), ()).await.unwrap();
                let mut rows = 0u64;
                loop {
                    while client.next_row_cursor().await.unwrap() {
                        match client.read_row_column(0).await.unwrap() {
                            CursorColumn::Value(_) => rows += 1,
                            other => panic!("unexpected cursor column: {other:?}"),
                        }
                    }
                    if !client.advance_to_rows().await.unwrap() {
                        break;
                    }
                }
                assert_eq!(rows, ROWS);
            })
        })
    });

    group.bench_function("tds_next_row", |b| {
        b.iter(|| {
            rt.block_on(async {
                client.execute(query.clone(), ()).await.unwrap();
                let mut rows = 0u64;
                loop {
                    while client.next_row().await.unwrap().is_some() {
                        rows += 1;
                    }
                    if !client.advance_to_rows().await.unwrap() {
                        break;
                    }
                }
                assert_eq!(rows, ROWS);
            })
        })
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(std::time::Duration::from_secs(
            std::env::var("BENCH_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
        ))
        .sample_size(
            std::env::var("BENCH_SAMPLES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
        );
    targets = odbc_split
}
criterion_main!(benches);
