// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Benchmark: streaming row throughput comparing next_row vs next_row_into paths.
//! Connects using the shared live-test environment (`tests/common/mod.rs`).

#[path = "../tests/common/mod.rs"]
pub mod common;

use criterion::{Criterion, criterion_group, criterion_main};
use futures_util::StreamExt;
use mssql_rs::{Client, Config};
use std::time::Instant;
use tokio::runtime::Runtime;

fn test_config() -> Config {
    common::live_config().expect("set SQL_PASSWORD for the row streaming benchmark")
}

const CREATE_SQL: &str = "CREATE TABLE #bench_rows (
    id INT,
    name NVARCHAR(100),
    value FLOAT,
    created_at DATETIME2
);";

const INSERT_SQL: &str = "
INSERT INTO #bench_rows (id, name, value, created_at)
SELECT TOP 100000
    ROW_NUMBER() OVER (ORDER BY (SELECT NULL)),
    CONCAT(N'row_name_', ROW_NUMBER() OVER (ORDER BY (SELECT NULL))),
    ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) * 1.5,
    GETDATE()
FROM sys.all_objects a CROSS JOIN sys.all_objects b;
";

async fn client_with_rows() -> Client {
    let cfg = test_config();
    let mut client = Client::connect(&cfg).await.expect("connect failed");
    client
        .simple_query(CREATE_SQL)
        .await
        .expect("create failed");
    client
        .simple_query(INSERT_SQL)
        .await
        .expect("insert failed");
    client
}

fn bench_streaming_rows(c: &mut Criterion) {
    let rt = Runtime::new().expect("benchmark runtime should start");
    let mut client = rt.block_on(client_with_rows());

    c.bench_function("stream_100k_rows", |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let mut stream = client.simple_query_streamed(
                        "SELECT id, name, value, created_at FROM #bench_rows",
                    );
                    let mut count = 0u64;
                    while let Some(row) = stream.next().await {
                        row.expect("streamed benchmark row failed");
                        count += 1;
                    }
                    total += start.elapsed();
                    assert_eq!(count, 100000);
                }
                total
            })
        });
    });
}

fn bench_buffered_rows(c: &mut Criterion) {
    let rt = Runtime::new().expect("benchmark runtime should start");
    let mut client = rt.block_on(client_with_rows());

    c.bench_function("buffered_100k_rows", |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let result = client
                        .simple_query("SELECT id, name, value, created_at FROM #bench_rows")
                        .await
                        .expect("buffered benchmark query failed");
                    let rows = result.into_first_result();
                    total += start.elapsed();
                    assert_eq!(rows.len(), 100000);
                }
                total
            })
        });
    });
}

fn bench_into_row_stream(c: &mut Criterion) {
    let rt = Runtime::new().expect("benchmark runtime should start");
    let mut client = rt.block_on(client_with_rows());

    c.bench_function("into_row_stream_100k_rows", |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    let result = client
                        .simple_query("SELECT id, name, value, created_at FROM #bench_rows")
                        .await
                        .expect("benchmark query failed");
                    let mut stream = result.into_row_stream();
                    let mut count = 0u64;
                    while let Some(row) = stream.next().await {
                        row.expect("buffered benchmark row failed");
                        count += 1;
                    }
                    total += start.elapsed();
                    assert_eq!(count, 100000);
                }
                total
            })
        });
    });
}

criterion_group!(
    benches,
    bench_streaming_rows,
    bench_buffered_rows,
    bench_into_row_stream
);
criterion_main!(benches);
