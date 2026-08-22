// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Live-server coverage for reading very large `varbinary(max)` values.
//!
//! These complement the unit tests in `datatypes::decoder`: they prove the
//! chunk framing works against a real SQL Server, which emits `varbinary(max)`
//! in 8000-byte PLP chunks. A 2 GB value therefore arrives as ~268 000 chunks,
//! which the removed `MAX_PLP_CHUNKS` cap of 100 000 rejected outright.
//!
//! They are `#[ignore]`d because they move gigabytes and take minutes. Run them
//! explicitly against a configured server:
//!
//! ```text
//! cargo nextest run --release -p mssql-tds --test test_large_value_read --run-ignored all
//! ```

mod common;

use std::time::Instant;

use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, TdsClient};
use mssql_tds::datatypes::column_values::ColumnValues;

/// T-SQL producing a single `varbinary(max)` value of exactly `bytes` bytes.
fn large_varbinary_query(bytes: usize) -> String {
    format!("SELECT CONVERT(varbinary(max), REPLICATE(CONVERT(varchar(max), 'a'), {bytes}))")
}

/// Reads the single-column, single-row result and returns its byte length.
async fn read_single_large_value(client: &mut TdsClient) -> Result<usize, String> {
    let mut length = None;
    while let Some(row) = client.next_row().await.map_err(|e| e.to_string())? {
        match row.first() {
            Some(ColumnValues::Bytes(bytes)) => length = Some(bytes.len()),
            Some(other) => return Err(format!("unexpected column value: {other:?}")),
            None => return Err("row had no columns".to_string()),
        }
    }
    client.close_query().await.map_err(|e| e.to_string())?;
    length.ok_or_else(|| "result set produced no rows".to_string())
}

/// Connects, runs one sized query, and returns the byte count read.
async fn probe_size(bytes: usize) -> Result<usize, String> {
    let mut client = common::create_client(&common::build_tcp_datasource())
        .await
        .map_err(|e| e.to_string())?;

    client
        .execute(
            large_varbinary_query(bytes),
            ExecuteOptions::new().timeout_secs(0),
        )
        .await
        .map_err(|e| e.to_string())?;

    read_single_large_value(&mut client).await
}

/// Walks `varbinary(max)` sizes upward. Every size below SQL Server's own 2 GB
/// limit must be readable — the driver must not impose a lower ceiling.
///
/// Before the chunk-count cap was removed this failed at 768 MB with
/// "Too many PLP chunks: 100001 (max 100000)".
#[tokio::test]
#[ignore = "moves ~2.7 GB against a live server"]
async fn large_varbinary_sizes_are_readable_up_to_the_server_limit() {
    common::init_tracing();

    const MB: usize = 1024 * 1024;
    let sizes = [
        MB,
        16 * MB,
        64 * MB,
        256 * MB,
        512 * MB,
        768 * MB,
        1024 * MB,
    ];

    let mut first_failure = None;
    for size in sizes {
        let started = Instant::now();
        match probe_size(size).await {
            Ok(read) => {
                println!(
                    "{:>6} MB: read {read} bytes in {:?}",
                    size / MB,
                    started.elapsed()
                );
                assert_eq!(read, size, "short read at {} MB", size / MB);
            }
            Err(error) => {
                println!(
                    "{:>6} MB: FAILED after {:?}: {error}",
                    size / MB,
                    started.elapsed()
                );
                first_failure.get_or_insert((size, error));
            }
        }
    }

    if let Some((size, error)) = first_failure {
        panic!(
            "a {} MB varbinary(max) is well within SQL Server's 2 GB limit but the driver \
             refused it: {error}",
            size / MB
        );
    }
}

/// The 2 GB case from the original hang report. `varbinary(max)` tops out at
/// 2 147 483 647 bytes on the server, so this is the largest single value TDS
/// can carry.
#[tokio::test]
#[ignore = "moves 2 GB against a live server and buffers it client-side"]
async fn two_gigabyte_varbinary_is_readable() {
    common::init_tracing();

    let started = Instant::now();
    let read = probe_size(i32::MAX as usize)
        .await
        .unwrap_or_else(|error| panic!("2 GB varbinary(max) read failed: {error}"));

    println!("2 GB: read {read} bytes in {:?}", started.elapsed());
    assert_eq!(read, i32::MAX as usize);
}
