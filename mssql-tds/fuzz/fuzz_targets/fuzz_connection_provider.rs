// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz test for TdsConnectionProvider
//!
//! This fuzzer tests the actual TdsConnectionProvider::create_client_with_transport() API
//! by injecting a mock transport with fuzzed server responses.
//!
//! What it tests:
//! - Connection establishment with fuzzed prelogin responses
//! - Login handshake with fuzzed tokens
//! - Feature negotiation with malformed data
//! - Error handling during connection setup
//! - Timeout and cancellation behavior
//! - Redirection handling with fuzzed routing tokens
//!
//! Run with: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_connection_provider

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::fuzz_support::{FuzzPacketReader, MockTransport, TdsConnectionProvider};

fuzz_target!(|data: &[u8]| {
    // Need at least some data to work with
    if data.is_empty() {
        return;
    }

    // Run the fuzzing
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        fuzz_connection_provider(data).await;
    });
});

async fn fuzz_connection_provider(data: &[u8]) {
    // Create a fuzz reader with the input data
    let reader = FuzzPacketReader::from_data(data);
    let packet_size = 4096;

    // Create a mock transport with fuzzed data
    let transport = MockTransport::new(reader, packet_size);

    // Create a minimal client context for testing
    let context = ClientContext::default();

    // Try to create a client with the fuzzed transport
    // This exercises the entire connection flow including prelogin, login,
    // feature negotiation, and error handling
    let result = TdsConnectionProvider::create_client_with_transport(context, transport).await;

    // We don't care about the result, just that it doesn't panic
    // The fuzzer is looking for panics, not successful connections
    let _ = result;
}
