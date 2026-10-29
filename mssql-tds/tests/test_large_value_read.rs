// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Live-server reproductions for the large-value (`varbinary(max)` / LOB) read
//! path, driving a real SQL Server rather than a hand-built packet stream.
//!
//! These are `#[ignore]`d: they move hundreds of megabytes and take minutes.
//! Run them explicitly:
//!
//! ```text
//! cargo nextest run -p mssql-tds --test test_large_value_read --run-ignored all
//! ```

mod common;

use std::time::{Duration, Instant};

use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, TdsClient};
use mssql_tds::datatypes::column_values::ColumnValues;

/// T-SQL that produces a single `varbinary(max)` value of exactly `bytes`
/// bytes, without materializing it in the client's parameter buffer.
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

/// Runs one size and reports either the byte count read or the failure.
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

/// SQL Server emits PLP chunks of exactly 8000 bytes for a `varbinary(max)`
/// value. The removed `MAX_PLP_CHUNKS` cap of 100 000 therefore translated to a
/// hard ceiling of 800 000 000 bytes on the wire, regardless of the advertised
/// `MAX_PLP_SIZE` of 2 GB. Sizes either side of it are the interesting cases.
const REMOVED_CHUNK_CAP_CEILING: usize = 100_000 * 8_000;

/// Walks `varbinary(max)` sizes upward across the ceiling the removed
/// chunk-count cap used to impose. Everything here is below SQL Server's own
/// 2 GB limit for the type, so the driver must read all of it — before the fix
/// anything past ~763 MB failed with "Too many PLP chunks".
#[tokio::test]
#[ignore = "moves hundreds of MB against a live server"]
async fn large_varbinary_sizes_are_readable_up_to_the_server_limit() {
    common::init_tracing();

    const MB: usize = 1024 * 1024;
    let sizes = [
        MB,
        64 * MB,
        256 * MB,
        REMOVED_CHUNK_CAP_CEILING - MB, // just under the old ceiling
        REMOVED_CHUNK_CAP_CEILING + MB, // just over it — used to fail
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

/// The 2 GB case from the original report, isolated. `varbinary(max)` tops out
/// at 2 147 483 647 bytes on the server, so this is the largest single value
/// TDS can carry.
#[tokio::test]
#[ignore = "moves 2 GB against a live server"]
async fn two_gigabyte_varbinary_is_readable() {
    common::init_tracing();

    let started = Instant::now();
    let read = probe_size(i32::MAX as usize)
        .await
        .unwrap_or_else(|error| panic!("2 GB varbinary(max) read failed: {error}"));

    println!("2 GB: read {read} bytes in {:?}", started.elapsed());
    assert_eq!(read, i32::MAX as usize);
}

/// A command timeout that fires while the server is still streaming a large
/// result must return to the caller promptly. The driver answers a timeout by
/// sending ATTENTION and then waiting for the acknowledgement *without a
/// deadline*, so a server still unwinding a large result set can hold the
/// caller well past the timeout it asked for.
///
/// The assertion is deliberately generous: a 5-second command is allowed 30
/// seconds of total wall time. Exceeding that is not slowness, it is an
/// unbounded wait.
#[tokio::test]
#[ignore = "moves hundreds of MB against a live server"]
async fn command_timeout_during_a_large_read_returns_promptly() {
    common::init_tracing();

    const TIMEOUT_SECS: u32 = 5;
    const GENEROUS_BOUND: Duration = Duration::from_secs(30);

    let mut client = common::create_client(&common::build_tcp_datasource())
        .await
        .unwrap();

    let started = Instant::now();
    let outcome = async {
        client
            .execute(
                // Many large rows, so the server is still producing when the
                // timeout fires and has a real backlog to unwind.
                "SELECT CONVERT(varbinary(max), REPLICATE(CONVERT(varchar(max), 'a'), 8000000)) \
                 FROM sys.all_objects a CROSS JOIN sys.all_objects b"
                    .to_string(),
                ExecuteOptions::new().timeout_secs(TIMEOUT_SECS),
            )
            .await?;

        while client.next_row().await?.is_some() {}
        Ok::<(), mssql_tds::error::Error>(())
    }
    .await;

    let elapsed = started.elapsed();
    println!("timeout outcome after {elapsed:?}: {outcome:?}");

    assert!(
        outcome.is_err(),
        "the command must not complete inside its {TIMEOUT_SECS}s budget"
    );
    assert!(
        elapsed < GENEROUS_BOUND,
        "a {TIMEOUT_SECS}s command timeout took {elapsed:?} to return: the post-timeout \
         attention-ACK wait is unbounded"
    );
}
