// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection benchmarks.
//!
//! - `connect_close`        — single non-pooled connect + immediate close;
//!   isolates the TLS handshake / login cost.
//! - `concurrent_connects`  — N simultaneous connects (N = 50, 100).
//!
//! Pooled connections are intentionally excluded — `mssql-tds` has no pool.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds_bench::{bench_env, connect, criterion_config, runtime, try_connect};

fn connect_close(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "connect_close").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");

    c.bench_function("connect_close", |b| {
        b.iter(|| {
            rt.block_on(async {
                let client = connect(&env).await;
                drop(client);
            });
        });
    });
}

fn concurrent_connects(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "concurrent_connects").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");

    let mut group = c.benchmark_group("concurrent_connects");
    for n in [50u64, 100] {
        group.throughput(Throughput::Elements(n));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                rt.block_on(async {
                    let mut set = tokio::task::JoinSet::new();
                    for _ in 0..n {
                        let env = env.clone();
                        set.spawn(async move {
                            let client = connect(&env).await;
                            drop(client);
                        });
                    }
                    while let Some(res) = set.join_next().await {
                        res.expect("connect task panicked");
                    }
                });
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = connect_close, concurrent_connects
}
criterion_main!(benches);
