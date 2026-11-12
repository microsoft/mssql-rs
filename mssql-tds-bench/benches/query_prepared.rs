// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Prepared-statement and batched-statement benchmarks over a single long-lived
//! connection (handshake cost excluded). Split out of the former monolithic
//! `query` bench binary so the perf-lab's per-binary candidate/baseline
//! interleaving keeps each benchmark's two measurements close together in time.
//!
//! - `prepared_execute`  — prepare once, execute many via `sp_execute`.
//! - `prepared_prepexec` — one-shot prepare+execute via `sp_prepexec`.
//! - `batched_statements` — three statements in one round-trip.
//! - `prepared_params/*` — repeat executes of one prepared statement with 1000
//!   bound values (`sp_prepexec` once, then `sp_execute` with the values bound
//!   by ordinal), as an ODBC application's `SQLExecute` loop sends them.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use mssql_tds::connection::tds_client::{PreparedStatement, ResultSet, TdsClient};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use mssql_tds_bench::{bench_env, connect, criterion_config, drain, runtime, try_connect};

fn prepared_execute(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "prepared_execute").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    // Prepare once (un-measured) and reuse the handle for every measured
    // execution — the prepare-once/execute-many pattern that pooled clients and
    // ORMs use. Reusing one handle keeps server state constant so the
    // measurement isolates the `sp_execute` round-trip and row decode.
    let statement = rt.block_on(async {
        let decls = vec![RpcParameter::new(
            Some("@id".to_string()),
            StatusFlags::NONE,
            SqlType::Int(None),
        )];
        client
            .execute_sp_prepare_for_test("SELECT @id AS v".to_string(), decls, ())
            .await
            .expect("sp_prepare failed")
    });

    c.bench_function("prepared_execute", |b| {
        b.iter(|| {
            rt.block_on(async {
                let params = vec![RpcParameter::new(
                    Some("@id".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(42)),
                )];
                client
                    .execute_sp_execute_for_test(statement, None, Some(params), ())
                    .await
                    .expect("sp_execute failed");
                drain(&mut client).await;
            });
        });
    });

    // Release the handle (un-measured).
    rt.block_on(async {
        client
            .execute_sp_unprepare_for_test(statement, ())
            .await
            .expect("sp_unprepare failed");
    });
}

fn prepared_prepexec(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "prepared_prepexec").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    // Non-cached one-shot: each iteration prepares and executes in a single
    // round-trip via `sp_prepexec`, then releases the returned handle so server
    // state does not accumulate (unbounded prepared handles would drift the
    // measurement). This is the counterpart to `prepared_execute` — the extra
    // unprepare round-trip is the cost a client pays when it cannot cache.
    c.bench_function("prepared_prepexec", |b| {
        b.iter(|| {
            rt.block_on(async {
                let params = vec![RpcParameter::new(
                    Some("@id".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(42)),
                )];
                let mut orphan = None;
                let (statement_id, _) = client
                    .execute_sp_prepexec_for_test(
                        "SELECT @id AS v".to_string(),
                        params,
                        &mut orphan,
                        (),
                    )
                    .await
                    .expect("sp_prepexec failed");
                drain(&mut client).await;
                client
                    .execute_sp_unprepare_for_test(statement_id, ())
                    .await
                    .expect("sp_unprepare failed");
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
                    .execute("SELECT 1; SELECT 2; SELECT 3".to_string(), ())
                    .await
                    .expect("batch failed");
                drain(&mut client).await;
            });
        });
    });
}

/// One `prepared_params` workload: `count` values of one type, checked
/// through an aggregate that returns one INT.
struct ParamWorkload {
    name: &'static str,
    count: usize,
    aggregate: &'static str,
    value: fn(usize) -> SqlType,
    expected: i32,
}

const PARAM_WORKLOADS: [ParamWorkload; 3] = [
    ParamWorkload {
        name: "int_1000",
        count: 1000,
        aggregate: "SUM(v)",
        value: |index| SqlType::Int(Some(index as i32 + 1)),
        expected: 500_500,
    },
    ParamWorkload {
        name: "nvarchar_1000",
        count: 1000,
        aggregate: "SUM(LEN(v))",
        value: |index| {
            SqlType::NVarchar(
                Some(SqlString::from_utf8_string(format!("value-{index:014}"))),
                20,
            )
        },
        expected: 20_000,
    },
    ParamWorkload {
        name: "float_1000",
        count: 1000,
        aggregate: "CAST(SUM(v) AS INT)",
        value: |index| SqlType::Float(Some(index as f64 + 1.0)),
        expected: 500_500,
    },
];

async fn execute_params(
    client: &mut TdsClient,
    statement: &mut PreparedStatement,
    params: Vec<RpcParameter>,
) {
    let mut orphaned = None;
    client
        .execute_prepared(statement, params, &mut orphaned, ())
        .await
        .expect("execute_prepared failed");
}

fn prepared_params(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "prepared_params").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    let mut group = c.benchmark_group("prepared_params");
    for workload in &PARAM_WORKLOADS {
        let markers: Vec<String> = (1..=workload.count).map(|i| format!("(@P{i})")).collect();
        let mut statement = PreparedStatement::new(format!(
            "SELECT {} FROM (VALUES {}) AS t(v)",
            workload.aggregate,
            markers.join(", ")
        ));
        let params: Vec<RpcParameter> = (0..workload.count)
            .map(|index| {
                RpcParameter::new(
                    Some(format!("@P{}", index + 1)),
                    StatusFlags::NONE,
                    (workload.value)(index),
                )
            })
            .collect();

        // Prepare (`sp_prepexec`) and check the result once, un-measured, so
        // every measured call is an `sp_execute`.
        rt.block_on(async {
            execute_params(&mut client, &mut statement, params.clone()).await;
            let row = client.next_row().await.expect("next_row failed");
            assert!(
                matches!(row.as_deref(), Some([ColumnValues::Int(v)]) if *v == workload.expected),
                "{}: {row:?}",
                workload.name
            );
            drain(&mut client).await;
        });

        // Building the parameter list is the caller's cost, not the driver's.
        group.bench_function(workload.name, |b| {
            b.iter_batched(
                || params.clone(),
                |params| {
                    rt.block_on(async {
                        execute_params(&mut client, &mut statement, params).await;
                        drain(&mut client).await;
                    });
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets =
        prepared_execute,
        prepared_prepexec,
        batched_statements,
        prepared_params
}
criterion_main!(benches);
