// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz test for TdsConnectionProvider with network/server response variations
//!
//! This fuzzer tests how TdsConnectionProvider handles various server responses
//! with a fixed ClientContext. The focus is on testing:
//! - Malformed prelogin responses
//! - Invalid login tokens
//! - Corrupted feature negotiation data
//! - Unexpected token sequences
//! - Partial/truncated responses
//! - Invalid TDS packet headers
//!
//! Run with: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_connection_provider_network

#![no_main]

use libfuzzer_sys::fuzz_target;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::fuzz_support::{FuzzPacketReader, MockTransport, TdsConnectionProvider};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        fuzz_network_response(data).await;
    });
});

async fn fuzz_network_response(data: &[u8]) {
    let reader = FuzzPacketReader::from_data(data);
    let packet_size = 4096;
    let transport = MockTransport::new(reader, packet_size);
    
    // Use default ClientContext to isolate network response testing
    let context = ClientContext::default();

    let result = TdsConnectionProvider::create_client_with_transport(context, transport).await;
    let _ = result;
}
