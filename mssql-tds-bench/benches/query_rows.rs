// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Row-returning / insert query benchmarks over a single long-lived connection
//! (handshake cost excluded). Split out of the former monolithic `query` bench
//! binary so the perf-lab's per-binary candidate/baseline interleaving keeps
//! each benchmark's two measurements close together in time.
//!
//! - `simple_select_one_row` — `SELECT 1`.
//! - `select_n_rows`         — N mixed-primitive rows (N = 100 / 1k / 10k).
//! - `single_insert`         — one INSERT into a session temp table.

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds_bench::{
    bench_env, connect, create_mixed_rows_table, criterion_config, drain, runtime, try_connect,
};

fn simple_select_one_row(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "simple_select_one_row").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    c.bench_function("simple_select_one_row", |b| {
        b.iter(|| {
            rt.block_on(async {
                client
                    .execute("SELECT 1".to_string(), ())
                    .await
                    .expect("execute failed");
                drain(&mut client).await;
            });
        });
    });
}

fn select_n_rows(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "select_n_rows").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    // Pre-populate a deterministic heap once (un-measured). Reading it back with
    // `SELECT TOP (n)` is a plain scan with fixed content, so the benchmark
    // times driver row decoding rather than server-side query execution.
    rt.block_on(create_mixed_rows_table(&mut client, "#bench_rows", 10_000));

    let mut group = c.benchmark_group("select_n_rows");
    for n in [100u64, 1_000, 10_000] {
        let query = format!(
            "SELECT TOP ({n}) c_int, c_bigint, c_bit, c_tinyint, c_smallint, \
             c_nvarchar, c_float, c_datetime2 FROM #bench_rows"
        );
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &query, |b, query| {
            b.iter(|| {
                rt.block_on(async {
                    client
                        .execute(query.clone(), ())
                        .await
                        .expect("execute failed");
                    drain(&mut client).await;
                });
            });
        });
    }
    group.finish();
}

// The single connection is shared between Criterion's un-measured reset closure
// (truncate) and the measured insert closure via a RefCell; each closure borrows
// it in turn and the borrow is held across the await. That is safe here
// (single-threaded bench, the closures never run concurrently), so the RefCell
// lint does not apply.
#[allow(clippy::await_holding_refcell_ref)]
fn single_insert(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "single_insert").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    // RefCell so the un-measured reset closure and the measured insert closure
    // can each borrow the single connection in turn (they never run at once).
    let client = std::cell::RefCell::new(rt.block_on(connect(&env)));

    // Session temp table — dropped automatically when the connection closes.
    rt.block_on(async {
        let mut conn = client.borrow_mut();
        conn.execute(
            "CREATE TABLE #bench_insert (id INT NOT NULL, name NVARCHAR(100) NOT NULL)".to_string(),
            (),
        )
        .await
        .expect("create temp table failed");
        conn.close_query().await.expect("close_query failed");
    });

    c.bench_function("single_insert", |b| {
        b.iter_batched(
            || {
                // Un-measured: truncate so rows do not accumulate across
                // iterations (which would otherwise grow the heap and tempdb
                // and drift the measurement upward).
                rt.block_on(async {
                    let mut conn = client.borrow_mut();
                    conn.execute("TRUNCATE TABLE #bench_insert".to_string(), ())
                        .await
                        .expect("truncate failed");
                    conn.close_query().await.expect("close_query failed");
                });
            },
            |_| {
                rt.block_on(async {
                    let mut conn = client.borrow_mut();
                    conn.execute(
                        "INSERT INTO #bench_insert (id, name) VALUES (1, N'benchmark')".to_string(),
                        (),
                    )
                    .await
                    .expect("insert failed");
                    conn.close_query().await.expect("close_query failed");
                });
            },
            BatchSize::PerIteration,
        );
    });
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets =
        simple_select_one_row,
        select_n_rows,
        single_insert
}
criterion_main!(benches);
