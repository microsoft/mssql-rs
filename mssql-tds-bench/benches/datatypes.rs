// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data-type decode benchmarks over a single long-lived connection.
//!
//! - `primitives`   — BIT / TINYINT / SMALLINT / INT / BIGINT / DECIMAL / FLOAT,
//!   decoded over many rows from a pre-populated heap.
//! - `strings`      — VARCHAR and NVARCHAR (4000 chars each).
//! - `temporal`     — DATETIME2 / DATE / TIME / DATETIMEOFFSET, decoded over many
//!   rows from a pre-populated heap.
//! - `lob`          — VARCHAR(MAX) payloads (1 MB / 20 MB), `Throughput::Bytes`.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds_bench::{bench_env, connect, criterion_config, drain, runtime, try_connect};

/// Rows scanned by the multi-row decode benches (`primitives`, `temporal`).
/// Enough rows that per-row decode dominates round-trip latency.
const DECODE_ROWS: u64 = 1_000;

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

/// Pre-populate a typed heap with `DECODE_ROWS` rows (un-measured), then time
/// scanning it back. Because the rows are materialized once and the scan has no
/// sort or filter, the measurement reflects driver decode of `name`'s types
/// rather than round-trip latency or server query cost. `create_sql` is the
/// `CREATE TABLE`, `fill_sql` an `INSERT ... SELECT ... FROM GENERATE_SERIES`,
/// and `select_sql` the plain scan.
fn bench_decode_rows(
    c: &mut Criterion,
    name: &str,
    create_sql: &str,
    fill_sql: &str,
    select_sql: &str,
) {
    let rt = runtime();
    if try_connect(&rt, name).is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    rt.block_on(async {
        client
            .execute(create_sql.to_string(), None, None)
            .await
            .expect("create table failed");
        client.close_query().await.expect("close_query failed");
        client
            .execute(fill_sql.to_string(), None, None)
            .await
            .expect("fill table failed");
        client.close_query().await.expect("close_query failed");
    });

    let query = select_sql.to_string();
    let mut group = c.benchmark_group(name);
    group.throughput(Throughput::Elements(DECODE_ROWS));
    group.bench_function("decode", |b| {
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

fn primitives(c: &mut Criterion) {
    bench_decode_rows(
        c,
        "primitives",
        "CREATE TABLE #prim (\
            c_bit BIT NOT NULL, \
            c_tinyint TINYINT NOT NULL, \
            c_smallint SMALLINT NOT NULL, \
            c_int INT NOT NULL, \
            c_bigint BIGINT NOT NULL, \
            c_decimal DECIMAL(10,2) NOT NULL, \
            c_float FLOAT NOT NULL)",
        &format!(
            "INSERT INTO #prim \
                (c_bit, c_tinyint, c_smallint, c_int, c_bigint, c_decimal, c_float) \
             SELECT CAST(value % 2 AS BIT), \
                    CAST(value % 256 AS TINYINT), \
                    CAST(value % 32768 AS SMALLINT), \
                    CAST(value AS INT), \
                    CAST(value AS BIGINT), \
                    CAST(value AS DECIMAL(10,2)), \
                    CAST(value AS FLOAT) * 1.5 \
             FROM GENERATE_SERIES(CAST(1 AS BIGINT), CAST({DECODE_ROWS} AS BIGINT))"
        ),
        "SELECT c_bit, c_tinyint, c_smallint, c_int, c_bigint, c_decimal, c_float FROM #prim",
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
    bench_decode_rows(
        c,
        "temporal",
        "CREATE TABLE #temporal (\
            c_datetime2 DATETIME2 NOT NULL, \
            c_date DATE NOT NULL, \
            c_time TIME NOT NULL, \
            c_dto DATETIMEOFFSET NOT NULL)",
        &format!(
            "INSERT INTO #temporal (c_datetime2, c_date, c_time, c_dto) \
             SELECT DATEADD(SECOND, value % 86400, CAST('2020-01-01T00:00:00' AS DATETIME2)), \
                    DATEADD(DAY, value % 3650, CAST('2020-01-01' AS DATE)), \
                    CAST(DATEADD(SECOND, value % 86400, CAST('2020-01-01T00:00:00' AS DATETIME2)) AS TIME), \
                    TODATETIMEOFFSET(DATEADD(SECOND, value % 86400, CAST('2020-01-01T00:00:00' AS DATETIME2)), 0) \
             FROM GENERATE_SERIES(CAST(1 AS BIGINT), CAST({DECODE_ROWS} AS BIGINT))"
        ),
        "SELECT c_datetime2, c_date, c_time, c_dto FROM #temporal",
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
        // Pre-materialize the payload once (un-measured) into a session temp
        // table so the benchmark times transfer + driver decode, not the
        // server-side REPLICATE that would otherwise regenerate the LOB (and its
        // memory grant) on every iteration.
        let table = format!("#lob_{bytes}");
        rt.block_on(async {
            client
                .execute(
                    format!("CREATE TABLE {table} (payload VARCHAR(MAX) NOT NULL)"),
                    None,
                    None,
                )
                .await
                .expect("create lob table failed");
            client.close_query().await.expect("close_query failed");
            client
                .execute(
                    format!(
                        "INSERT INTO {table} (payload) \
                         SELECT REPLICATE(CAST('X' AS VARCHAR(MAX)), {bytes})"
                    ),
                    None,
                    None,
                )
                .await
                .expect("fill lob table failed");
            client.close_query().await.expect("close_query failed");
        });

        let query = format!("SELECT payload FROM {table}");
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
