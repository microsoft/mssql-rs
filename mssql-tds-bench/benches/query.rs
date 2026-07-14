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
//! - `stored_procedure_output`   — a temp proc with an OUTPUT parameter.
//! - `prepared_execute`          — prepare once, execute many via `sp_execute`.
//! - `prepared_prepexec`         — one-shot prepare+execute via `sp_prepexec`.
//! - `batched_statements`        — three statements in one round-trip.

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use mssql_tds_bench::{
    bench_env, connect, create_mixed_rows_table, criterion_config, drain, drain_capture_handle,
    runtime, try_connect,
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
    // RefCell so the un-measured reset closure and the measured insert closure
    // can each borrow the single connection in turn (they never run at once).
    let client = std::cell::RefCell::new(rt.block_on(connect(&env)));

    // Session temp table — dropped automatically when the connection closes.
    rt.block_on(async {
        let mut conn = client.borrow_mut();
        conn.execute(
            "CREATE TABLE #bench_insert (id INT NOT NULL, name NVARCHAR(100) NOT NULL)".to_string(),
            None,
            None,
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
                    conn.execute("TRUNCATE TABLE #bench_insert".to_string(), None, None)
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
                        None,
                        None,
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

fn stored_procedure_output(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "stored_procedure_output").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    let mut client = rt.block_on(connect(&env));

    // Temp proc with an OUTPUT parameter — exercises BY_REF parameter
    // serialization on the write side and the ReturnValue decode path on the
    // read side, neither of which the plain `stored_procedure` bench (no output
    // params) touches. The proc returns no rows, isolating the parameter paths.
    rt.block_on(async {
        client
            .execute(
                "CREATE PROCEDURE #bench_proc_out @x INT, @y INT OUTPUT AS \
                 BEGIN SET NOCOUNT ON; SET @y = @x * 2; END"
                    .to_string(),
                None,
                None,
            )
            .await
            .expect("create temp proc failed");
        client.close_query().await.expect("close_query failed");
    });

    c.bench_function("stored_procedure_output", |b| {
        b.iter(|| {
            rt.block_on(async {
                let params = vec![
                    RpcParameter::new(
                        Some("@x".to_string()),
                        StatusFlags::NONE,
                        SqlType::Int(Some(21)),
                    ),
                    RpcParameter::new(
                        Some("@y".to_string()),
                        StatusFlags::BY_REF_VALUE,
                        SqlType::Int(None),
                    ),
                ];
                client
                    .execute_stored_procedure(
                        "#bench_proc_out".to_string(),
                        None,
                        Some(params),
                        None,
                        None,
                    )
                    .await
                    .expect("exec proc failed");
                // Read the OUTPUT parameter, then reset for the next iteration.
                let _out = client.get_return_values();
                client.close_query().await.expect("close_query failed");
            });
        });
    });
}

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
    let handle = rt.block_on(async {
        let decls = vec![RpcParameter::new(
            Some("@id".to_string()),
            StatusFlags::NONE,
            SqlType::Int(None),
        )];
        client
            .execute_sp_prepare("SELECT @id AS v".to_string(), decls, None, None)
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
                    .execute_sp_execute(handle, None, Some(params), None, None)
                    .await
                    .expect("sp_execute failed");
                drain(&mut client).await;
            });
        });
    });

    // Release the handle (un-measured).
    rt.block_on(async {
        client
            .execute_sp_unprepare(handle, None, None)
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
                client
                    .execute_sp_prepexec("SELECT @id AS v".to_string(), params, None, None)
                    .await
                    .expect("sp_prepexec failed");
                let handle = drain_capture_handle(&mut client).await;
                client
                    .execute_sp_unprepare(handle, None, None)
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
        stored_procedure_output,
        prepared_execute,
        prepared_prepexec,
        batched_statements
}
criterion_main!(benches);
