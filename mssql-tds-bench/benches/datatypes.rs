// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data-type decode benchmarks over a single long-lived connection.
//!
//! - `primitives`   — BIT / TINYINT / SMALLINT / INT / BIGINT / DECIMAL / FLOAT.
//! - `strings`      — VARCHAR and NVARCHAR (4000 chars each).
//! - `temporal`     — DATETIME2 / DATE / TIME / DATETIMEOFFSET.
//! - `lob`          — VARCHAR(MAX) payloads (1 MB / 20 MB), `Throughput::Bytes`.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds_bench::{bench_env, connect, criterion_config, drain, runtime, try_connect};

/// Run a single-statement query and drain it, on a persistent connection.
fn bench_query(c: &mut Criterion, name: &str, query: &str) {
    let rt = runtime();
    if try_connect(&rt, name).is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));
    let query = query.to_string();

    c.bench_function(name, |b| {
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
}

fn primitives(c: &mut Criterion) {
    bench_query(
        c,
        "primitives",
        "SELECT CAST(1 AS BIT) AS c_bit, \
                CAST(2 AS TINYINT) AS c_tinyint, \
                CAST(3 AS SMALLINT) AS c_smallint, \
                CAST(4 AS INT) AS c_int, \
                CAST(5 AS BIGINT) AS c_bigint, \
                CAST(123.45 AS DECIMAL(10,2)) AS c_decimal, \
                CAST(1.5 AS FLOAT) AS c_float",
    );
}

fn strings(c: &mut Criterion) {
    bench_query(
        c,
        "strings",
        "SELECT CAST(REPLICATE('a', 4000) AS VARCHAR(4000)) AS c_varchar, \
                CAST(REPLICATE(N'n', 4000) AS NVARCHAR(4000)) AS c_nvarchar",
    );
}

fn temporal(c: &mut Criterion) {
    bench_query(
        c,
        "temporal",
        "SELECT CAST(SYSDATETIME() AS DATETIME2) AS c_datetime2, \
                CAST(SYSDATETIME() AS DATE) AS c_date, \
                CAST(SYSDATETIME() AS TIME) AS c_time, \
                SYSDATETIMEOFFSET() AS c_datetimeoffset",
    );
}

fn lob(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "lob").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    let mut group = c.benchmark_group("lob");
    for bytes in [1_048_576u64, 20_971_520] {
        let query = format!(
            "SELECT REPLICATE(CAST('X' AS VARCHAR(MAX)), {bytes}) AS payload"
        );
        group.throughput(Throughput::Bytes(bytes));
        group.bench_with_input(BenchmarkId::from_parameter(bytes), &query, |b, query| {
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
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = primitives, strings, temporal, lob
}
criterion_main!(benches);
