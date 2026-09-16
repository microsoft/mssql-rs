// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Temporary attribution harness for the PR #564 handle-registry regression.
//! Not shipped: measures driver-side synchronization only, with no server.

use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use crate::api::odbc_types::{
    SQL_ATTR_ODBC_VERSION, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_NULL_HANDLE,
    SQL_OV_ODBC3_80, SQL_SUCCESS, SqlHandle,
};
use crate::handles::{Handle, StmtHandle, handle_from_raw};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Merge-base equivalent: pointer cast was free, so only the STMT lock.
    BaseSync,
    /// `get_handle!` — registry lookup plus ancestor reservation.
    Acquire,
    /// `claim_result_use` — DBC gate, STMT lock, binding leases.
    Claim,
    /// Everything `SQLGetData` now does before touching statement state.
    GetDataSync,
    /// Registry lookup only: RwLock read + HashMap + Arc clone.
    LookupOnly,
    /// Ancestor reservation only: the STMT/DBC/ENV activity counters.
    ReserveOnly,
}

impl Op {
    fn label(self) -> &'static str {
        match self {
            Self::BaseSync => "base_sync (STMT lock only)",
            Self::Acquire => "acquire   (registry + reservation)",
            Self::Claim => "claim     (result-use admission)",
            Self::GetDataSync => "getdata   (acquire + claim + lock)",
            Self::LookupOnly => "  lookup  (rwlock + hashmap + arc)",
            Self::ReserveOnly => "  reserve (ancestor counters)",
        }
    }
}

fn alloc(kind: i16, parent: SqlHandle) -> SqlHandle {
    let mut out: SqlHandle = SQL_NULL_HANDLE;
    assert_eq!(
        unsafe { crate::api::alloc_handle::sql_alloc_handle(kind, parent, &mut out) },
        SQL_SUCCESS
    );
    assert!(!out.is_null());
    out
}

fn new_env() -> SqlHandle {
    let env = alloc(SQL_HANDLE_ENV, SQL_NULL_HANDLE);
    assert_eq!(
        unsafe {
            crate::api::set_env_attr::sql_set_env_attr(
                env,
                SQL_ATTR_ODBC_VERSION,
                SQL_OV_ODBC3_80 as usize as *mut std::ffi::c_void,
                0,
            )
        },
        SQL_SUCCESS
    );
    env
}

/// One measured parallel region. Returns aggregate operations per second.
fn measure(env_addr: usize, threads: usize, iters: usize, op: Op) -> f64 {
    let barrier = Arc::new(Barrier::new(threads + 1));
    let elapsed = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    let env: SqlHandle = std::ptr::without_provenance_mut(env_addr);
                    let dbc = alloc(SQL_HANDLE_DBC, env);
                    let stmt_raw = alloc(SQL_HANDLE_STMT, dbc);
                    let stmt = handle_from_raw::<StmtHandle>(stmt_raw).unwrap().into_arc();
                    barrier.wait();
                    for _ in 0..iters {
                        match op {
                            Op::BaseSync => {
                                black_box(&*stmt.inner.lock().unwrap());
                            }
                            Op::Acquire => {
                                let h = handle_from_raw::<StmtHandle>(stmt_raw).unwrap();
                                black_box(&*h);
                            }
                            Op::Claim => {
                                let g = super::close_cursor::claim_result_use(&stmt).unwrap();
                                black_box(&g);
                            }
                            Op::LookupOnly => {
                                let v =
                                    crate::handles::probe_lookup::<StmtHandle>(stmt_raw).unwrap();
                                black_box(&*v);
                            }
                            Op::ReserveOnly => {
                                crate::handles::probe_reserve(stmt.activity());
                            }
                            Op::GetDataSync => {
                                let h = handle_from_raw::<StmtHandle>(stmt_raw).unwrap();
                                let g = super::close_cursor::claim_result_use(&h).unwrap();
                                black_box(&*h.inner.lock().unwrap());
                                black_box(&g);
                            }
                        }
                    }
                    // Handles intentionally leak: teardown is outside the measurement
                    // and the process exits at the end of the test.
                })
            })
            .collect();
        barrier.wait();
        let start = Instant::now();
        for worker in workers {
            worker.join().unwrap();
        }
        start.elapsed()
    });
    (threads * iters) as f64 / elapsed.as_secs_f64()
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

#[test]
#[ignore = "perf attribution probe; run explicitly with --ignored"]
fn perf_probe() {
    const ITERS: usize = 300_000;
    let reps: usize = std::env::var("PROBE_REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let thread_counts: Vec<usize> = std::env::var("PROBE_THREADS")
        .ok()
        .map(|v| v.split(',').map(|t| t.trim().parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1, 2, 4, 8]);

    let env_addr = new_env().addr();
    // Warm up allocation paths and the registry map before measuring.
    measure(env_addr, 2, 20_000, Op::GetDataSync);

    println!("\n{:<38} {:>8} {:>14} {:>12}", "op", "threads", "ops/sec", "ns/op");
    println!("{}", "-".repeat(76));
    for op in [
        Op::BaseSync,
        Op::LookupOnly,
        Op::ReserveOnly,
        Op::Acquire,
        Op::Claim,
        Op::GetDataSync,
    ] {
        for &threads in &thread_counts {
            let samples: Vec<f64> = (0..reps)
                .map(|_| {
                    let r = measure(env_addr, threads, ITERS, op);
                    std::thread::sleep(Duration::from_millis(20));
                    r
                })
                .collect();
            let spread = samples.iter().cloned().fold(f64::MIN, f64::max)
                / samples.iter().cloned().fold(f64::MAX, f64::min);
            let agg = median(samples);
            println!(
                "RESULT\t{}\t{}\t{:.0}\t{:.1}\t{:.2}",
                op.label().split_whitespace().next().unwrap(),
                threads,
                agg,
                1e9 / (agg / threads as f64),
                spread
            );
        }
    }
}
