// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Benchmarks specific to `mssql-tds` internals.
//!
//! - `packet_size_sensitivity` — the same large read at requested packet sizes
//!   4096 / 8192 / 32768, measuring TDS reassembly overhead.
//! - `row_iteration_throughput` — zero-copy `next_row` iteration over a large
//!   result set, measuring per-row decode throughput.

use std::env;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds_bench::{
    bench_env, connect, connect_with_packet_size, create_mixed_rows_table, criterion_config, drain,
    runtime, try_connect,
};

/// Approximate payload size (bytes) for the packet-size read.
const READ_BYTES: u64 = 16 * 1024 * 1024;

fn packet_size_sensitivity(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "packet_size_sensitivity").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let query =
        format!("SELECT REPLICATE(CAST('X' AS VARCHAR(MAX)), {READ_BYTES}) AS payload");

    let mut group = c.benchmark_group("packet_size_sensitivity");
    group.throughput(Throughput::Bytes(READ_BYTES));
    for packet_size in [4096u16, 8192, 32768] {
        let mut client = rt.block_on(connect_with_packet_size(&env, packet_size));
        group.bench_with_input(
            BenchmarkId::from_parameter(packet_size),
            &query,
            |b, query| {
                b.iter(|| {
                    rt.block_on(async {
                        client
                            .execute(query.clone(), None, None)
                            .await
                            .expect("execute failed");
                        drain(&mut client).await;
                    });
                });
            },
        );
    }
    group.finish();
}

fn row_iteration_throughput(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "row_iteration_throughput").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    let rows: u64 = env::var("BENCH_ITER_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000);

    // Pre-populate a deterministic heap once (un-measured), then scan it back so
    // the benchmark times per-row decode throughput rather than the server-side
    // cost of generating the rows.
    rt.block_on(create_mixed_rows_table(&mut client, "#iter_rows", rows));
    let query = "SELECT c_int, c_bigint, c_nvarchar, c_bit FROM #iter_rows".to_string();

    let mut group = c.benchmark_group("row_iteration_throughput");
    group.throughput(Throughput::Elements(rows));
    group.bench_function("next_row", |b| {
        b.iter(|| {
            rt.block_on(async {
                client
                    .execute(query.clone(), None, None)
                    .await
                    .expect("execute failed");
                drain(&mut client).await;
            });
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = packet_size_sensitivity, row_iteration_throughput
}
criterion_main!(benches);
