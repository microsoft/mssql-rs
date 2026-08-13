// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz test for TdsClient
//!
//! This fuzzer tests the TdsClient with simulated server responses to find:
//! - Panics or crashes when processing various token sequences
//! - State management issues
//! - Edge cases in query execution and result processing
//!
//! The fuzzer simulates different scenarios:
//! - Execute batch queries
//! - Fetch rows from result sets
//! - Handle metadata
//! - Process various token sequences
//!
//! Run with: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_tds_client

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssql_tds::fuzz_support::{FuzzPacketReader, create_fuzz_tds_client};

fuzz_target!(|data: &[u8]| {
    // Need at least 2 bytes: 1 for scenario, 1+ for token data
    if data.len() < 2 {
        return;
    }

    // Limit input size to avoid excessive memory consumption and timeouts
    if data.len() > 2048 {
        return;
    }

    let scenario = data[0] % 4; // 4 different scenarios
    let token_data = &data[1..];

    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async {
        // Create TdsClient with mock transport using fuzzer data
        let packet_reader = FuzzPacketReader::from_data(token_data);
        let mut client = create_fuzz_tds_client(packet_reader, 4096);

        // Execute scenario based on fuzzer input
        // We only test public APIs here
        match scenario {
            0 => {
                // Fuzz execute() - simulates sending a query and receiving response
                let _ = client.execute("SELECT 1".to_string(), ()).await;
            }
            1 => {
                // Fuzz execute() followed by close_query()
                // This tests the full query -> response -> close flow
                if client
                    .execute("SELECT 1".to_string(), ())
                    .await
                    .is_ok()
                {
                    let _ = client.close_query().await;
                }
            }
            2 => {
                // Fuzz execute_sp_executesql()
                // This tests parameterized query execution
                let _ = client
                    .execute_sp_executesql("SELECT @p1".to_string(), vec![], ())
                    .await;
            }
            3 => {
                // Fuzz execute_stored_procedure()
                // This tests stored procedure execution
                let _ = client
                    .execute_stored_procedure("sp_test".to_string(), None, None, ())
                    .await;
            }
            _ => {}
        }
    });
});
