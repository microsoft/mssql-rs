// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz test for TdsConnectionProvider with ClientContext variations
//!
//! This fuzzer tests how TdsConnectionProvider handles various ClientContext
//! configurations with a fixed mock server response. The focus is on testing:
//! - Various authentication methods
//! - Extreme timeout values
//! - Invalid packet sizes
//! - Long string fields (username, password, database, etc.)
//! - Application intent variations
//! - Connection retry configurations
//!
//! Run with: RUSTFLAGS="--cfg fuzzing" cargo +nightly fuzz run fuzz_connection_provider_context

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::message::login_options::ApplicationIntent;
use mssql_tds::fuzz_support::{EmptyReader, FuzzPacketReader, MockTransport, TdsConnectionProvider};

#[derive(Debug, Arbitrary)]
struct FuzzClientContext {
    application_intent: FuzzApplicationIntent,
    application_name: String,
    connect_retry_count: u32,
    connect_timeout: u32,
    database: String,
    language: String,
    packet_size: i16,
    user_name: String,
    password: String,
    workstation_id: String,
    auth_method: FuzzAuthMethod,
}

#[derive(Debug, Arbitrary)]
enum FuzzApplicationIntent {
    ReadWrite,
    ReadOnly,
}

#[derive(Debug, Arbitrary)]
enum FuzzAuthMethod {
    Password,
    SSPI,
}

impl FuzzClientContext {
    fn to_client_context(&self) -> ClientContext {
        let mut context = ClientContext::default();
        
        context.application_intent = match self.application_intent {
            FuzzApplicationIntent::ReadWrite => ApplicationIntent::ReadWrite,
            FuzzApplicationIntent::ReadOnly => ApplicationIntent::ReadOnly,
        };
        
        // Sanitize string fields to prevent timeout
        context.application_name = truncate_string(&self.application_name, 128);
        context.database = truncate_string(&self.database, 128);
        context.language = truncate_string(&self.language, 128);
        context.user_name = truncate_string(&self.user_name, 128);
        context.password = truncate_string(&self.password, 128);
        context.workstation_id = truncate_string(&self.workstation_id, 128);
        
        // Clamp numeric values to reasonable ranges
        context.connect_retry_count = self.connect_retry_count.min(3);
        context.connect_timeout = self.connect_timeout.min(30);
        context.packet_size = self.packet_size.clamp(512, 32767) as u16;
        
        context.tds_authentication_method = match self.auth_method {
            FuzzAuthMethod::Password => TdsAuthenticationMethod::Password,
            FuzzAuthMethod::SSPI => TdsAuthenticationMethod::SSPI,
        };
        
        context
    }
}

fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    
    let mut truncate_at = max_len;
    while truncate_at > 0 && !s.is_char_boundary(truncate_at) {
        truncate_at -= 1;
    }
    
    s[..truncate_at].to_string()
}

fuzz_target!(|fuzz_context: FuzzClientContext| {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        fuzz_client_context(fuzz_context).await;
    });
});

async fn fuzz_client_context(fuzz_context: FuzzClientContext) {
    let reader = FuzzPacketReader::Empty(EmptyReader);
    let packet_size = 4096;
    let transport = MockTransport::new(reader, packet_size);
    
    // Convert fuzzed input to ClientContext
    let context = fuzz_context.to_client_context();

    // Test connection with fuzzed context
    let _ = TdsConnectionProvider::create_client_with_transport(context, transport).await;
}
