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

use criterion::{Criterion, criterion_group, criterion_main};
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use mssql_tds_bench::{
    bench_env, connect, criterion_config, drain, drain_capture_handle, runtime, try_connect,
};

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
                    .execute_sp_prepexec("SELECT @id AS v".to_string(), params, None, None, None)
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
        prepared_execute,
        prepared_prepexec,
        batched_statements
}
criterion_main!(benches);
