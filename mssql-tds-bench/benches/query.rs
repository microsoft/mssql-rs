// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Query-execution benchmarks over a single long-lived connection (handshake
//! cost excluded). Each iteration runs one statement and drains its results.
//!
//! - `simple_select_one_row`     — `SELECT 1`.
//! - `select_n_rows`             — N mixed-primitive rows (N = 100 / 1k / 10k).
//! - `single_insert`             — one INSERT into a session temp table.
//! - `parameterized_sp_executesql` — parameterized query via `sp_executesql`.
//! - `stored_procedure`          — a temp stored procedure via RPC.
//! - `batched_statements`        — three statements in one round-trip.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use mssql_tds_bench::{bench_env, connect, criterion_config, drain, runtime, try_connect};

/// Produce a query returning `rows` rows with 8 mixed-type columns.
fn mixed_rows_query(rows: u64) -> String {
    format!(
        r#"SELECT TOP ({rows})
    CAST(c1.object_id AS INT)        AS c_int,
    CAST(c1.column_id AS BIGINT)     AS c_bigint,
    CAST(c1.is_nullable AS BIT)      AS c_bit,
    CAST(c1.precision AS TINYINT)    AS c_tinyint,
    CAST(c1.max_length AS SMALLINT)  AS c_smallint,
    CAST(c1.name AS NVARCHAR(128))   AS c_nvarchar,
    CAST(1.5 AS FLOAT)               AS c_float,
    CAST(SYSDATETIME() AS DATETIME2) AS c_datetime2
FROM sys.columns c1
CROSS JOIN sys.columns c2
ORDER BY c1.object_id, c1.column_id"#
    )
}

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
                    .execute("SELECT 1".to_string(), None, None)
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

    let mut group = c.benchmark_group("select_n_rows");
    for n in [100u64, 1_000, 10_000] {
        let query = mixed_rows_query(n);
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &query, |b, query| {
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

fn single_insert(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "single_insert").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    // Session temp table — dropped automatically when the connection closes.
    rt.block_on(async {
        client
            .execute(
                "CREATE TABLE #bench_insert (id INT NOT NULL, name NVARCHAR(100) NOT NULL)"
                    .to_string(),
                None,
                None,
            )
            .await
            .expect("create temp table failed");
        client.close_query().await.expect("close_query failed");
    });

    c.bench_function("single_insert", |b| {
        b.iter(|| {
            rt.block_on(async {
                client
                    .execute(
                        "INSERT INTO #bench_insert (id, name) VALUES (1, N'benchmark')"
                            .to_string(),
                        None,
                        None,
                    )
                    .await
                    .expect("insert failed");
                client.close_query().await.expect("close_query failed");
            });
        });
    });
}

fn parameterized_sp_executesql(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "parameterized_sp_executesql").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    c.bench_function("parameterized_sp_executesql", |b| {
        b.iter(|| {
            rt.block_on(async {
                let params = vec![RpcParameter::new(
                    Some("@id".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(42)),
                )];
                client
                    .execute_sp_executesql("SELECT @id AS v".to_string(), params, None, None)
                    .await
                    .expect("sp_executesql failed");
                drain(&mut client).await;
            });
        });
    });
}

fn stored_procedure(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "stored_procedure").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    // Temp stored procedure — scoped to the connection's session.
    rt.block_on(async {
        client
            .execute(
                "CREATE PROCEDURE #bench_proc AS BEGIN SET NOCOUNT ON; SELECT 1 AS v; END"
                    .to_string(),
                None,
                None,
            )
            .await
            .expect("create temp proc failed");
        client.close_query().await.expect("close_query failed");
    });

    c.bench_function("stored_procedure", |b| {
        b.iter(|| {
            rt.block_on(async {
                client
                    .execute_stored_procedure("#bench_proc".to_string(), None, None, None, None)
                    .await
                    .expect("exec proc failed");
                drain(&mut client).await;
            });
        });
    });
}

fn batched_statements(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "batched_statements").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    c.bench_function("batched_statements", |b| {
        b.iter(|| {
            rt.block_on(async {
                client
                    .execute("SELECT 1; SELECT 2; SELECT 3".to_string(), None, None)
                    .await
                    .expect("batch failed");
                drain(&mut client).await;
            });
        });
    });
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets =
        simple_select_one_row,
        select_n_rows,
        single_insert,
        parameterized_sp_executesql,
        stored_procedure,
        batched_statements
}
criterion_main!(benches);
