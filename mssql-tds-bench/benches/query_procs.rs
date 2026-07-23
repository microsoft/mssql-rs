// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stored-procedure / parameterized RPC benchmarks over a single long-lived
//! connection (handshake cost excluded). Split out of the former monolithic
//! `query` bench binary so the perf-lab's per-binary candidate/baseline
//! interleaving keeps each benchmark's two measurements close together in time.
//!
//! - `parameterized_sp_executesql` — parameterized query via `sp_executesql`.
//! - `stored_procedure`            — a temp stored procedure via RPC.
//! - `stored_procedure_output`     — a temp proc with an OUTPUT parameter.

use criterion::{Criterion, criterion_group, criterion_main};
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use mssql_tds_bench::{bench_env, connect, criterion_config, drain, runtime, try_connect};

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
                    .execute_sp_executesql("SELECT @id AS v".to_string(), params, ())
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
                (),
            )
            .await
            .expect("create temp proc failed");
        client.close_query().await.expect("close_query failed");
    });

    c.bench_function("stored_procedure", |b| {
        b.iter(|| {
            rt.block_on(async {
                client
                    .execute_stored_procedure("#bench_proc".to_string(), None, None, ())
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
                (),
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
                    .execute_stored_procedure("#bench_proc_out".to_string(), None, Some(params), ())
                    .await
                    .expect("exec proc failed");
                // Read the OUTPUT parameter, then reset for the next iteration.
                let _out = client.get_return_values();
                client.close_query().await.expect("close_query failed");
            });
        });
    });
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets =
        parameterized_sp_executesql,
        stored_procedure,
        stored_procedure_output
}
criterion_main!(benches);
