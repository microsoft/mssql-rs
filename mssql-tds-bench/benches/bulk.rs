// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bulk-insert benchmark via the `BulkCopy` builder.
//!
//! Inserts `BENCH_BULK_ROWS` rows (default 10,000) into a session temp table
//! across batch sizes 500 / 5,000. The table is truncated in un-measured
//! setup before each measured insert, so it does not grow across iterations and
//! the truncate round-trip is excluded from the timing.

use std::env;

use async_trait::async_trait;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;
use mssql_tds_bench::{bench_env, connect, criterion_config, runtime, try_connect};

/// A two-column bulk row (`id BIGINT`, `value INT`).
#[derive(Clone)]
struct BenchRow {
    id: i64,
    value: i32,
}

#[async_trait]
impl BulkLoadRow for BenchRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer
            .write_column_value(*column_index, &ColumnValues::BigInt(self.id))
            .await?;
        *column_index += 1;
        writer
            .write_column_value(*column_index, &ColumnValues::Int(self.value))
            .await?;
        *column_index += 1;
        Ok(())
    }
}

#[async_trait]
impl BulkLoadRow for &BenchRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        (*self).write_to_packet(writer, column_index).await
    }
}

fn bulk_rows() -> u64 {
    env::var("BENCH_BULK_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

// The single connection is shared between Criterion's un-measured setup closure
// (truncate) and the measured closure (bulk copy) via a RefCell; each closure
// borrows it in turn and the borrow is held across the await. That is safe here
// (single-threaded bench, the closures never run concurrently), so the RefCell
// lint does not apply.
#[allow(clippy::await_holding_refcell_ref)]
fn bulk_insert(c: &mut Criterion) {
    let rt = runtime();
    if try_connect(&rt, "bulk_insert").is_none() {
        return;
    }
    let env = bench_env().expect("env present after successful probe");
    // RefCell so the un-measured truncate closure and the measured bulk-copy
    // closure can each borrow the single connection in turn.
    let client = std::cell::RefCell::new(rt.block_on(connect(&env)));

    rt.block_on(async {
        let mut conn = client.borrow_mut();
        conn.execute(
            "CREATE TABLE #bulk_target (id BIGINT NOT NULL, value INT NOT NULL)".to_string(),
            None,
            None,
        )
        .await
        .expect("create temp table failed");
        conn.close_query().await.expect("close_query failed");
    });

    let rows_count = bulk_rows();
    let rows: Vec<BenchRow> = (0..rows_count)
        .map(|i| BenchRow {
            id: i as i64,
            value: (i % i32::MAX as u64) as i32,
        })
        .collect();

    let mut group = c.benchmark_group("bulk_insert");
    group.throughput(Throughput::Elements(rows_count));
    for batch_size in [500usize, 5_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            &batch_size,
            |b, &batch_size| {
                b.iter_batched(
                    || {
                        // Un-measured: reset the target so it doesn't grow.
                        rt.block_on(async {
                            let mut conn = client.borrow_mut();
                            conn.execute("TRUNCATE TABLE #bulk_target".to_string(), None, None)
                                .await
                                .expect("truncate failed");
                            conn.close_query().await.expect("close_query failed");
                        });
                    },
                    |_| {
                        rt.block_on(async {
                            let mut conn = client.borrow_mut();
                            let bulk_copy = BulkCopy::new(&mut conn, "#bulk_target");
                            bulk_copy
                                .batch_size(batch_size)
                                .write_to_server_zerocopy(&rows)
                                .await
                                .expect("bulk copy failed");
                        });
                    },
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = bulk_insert
}
criterion_main!(benches);
