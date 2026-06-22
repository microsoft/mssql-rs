// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Token parsers for TDS protocol.
//!
//! This module contains parsers for each TDS token type. Each parser implements
//! the `TokenParser` trait and is responsible for reading the binary representation
//! of a specific token type from the packet stream.
//!
//! # Organization
//!
//! Each token type has its own parser module:
//!
//! - `common` - Common types and the `TokenParser` trait (internal)
//! - `envchange_parser` - ENVCHANGE token (environment changes)
//! - `loginack_parser` - LOGINACK token (login acknowledgment)
//! - `done_parser` - DONE family tokens (statement completion)
//! - `info_parser` - INFO token (informational messages)
//! - `error_parser` - ERROR token (error messages)
//! - `fedauth_parser` - FEDAUTHINFO token (federated authentication)
//! - `featureext_parser` - FEATUREEXTACK token (feature extension acknowledgment)
//! - `colmetadata_parser` - COLMETADATA token (column metadata)
//! - `row_parser` - ROW token (data row)
//! - `nbcrow_parser` - NBCROW token (null-bitmap compressed row)
//! - `order_parser` - ORDER token (ordering information)
//! - `returnstatus_parser` - RETURNSTATUS token (return status)
//! - `returnvalue_parser` - RETURNVALUE token (return value)
//!
//! # Architecture
//!
//! The parser architecture follows these principles:
//!
//! 1. **Single Responsibility**: Each parser handles exactly one token type
//! 2. **Trait-Based**: All parsers implement the `TokenParser` trait
//! 3. **Async**: Parsers use async/await for non-blocking I/O
//! 4. **Type Safety**: Strong typing prevents token confusion
//!
//! # Example
//!
//! ```rust,ignore
//! use mssql_tds::token::parsers::{TokenParser, DoneTokenParser};
//!
//! let parser = DoneTokenParser::default();
//! let token = parser.parse(&mut reader, &context).await?;
//! ```

// Common module with TokenParser trait
pub(crate) mod common;

// Individual parser modules
pub(crate) mod col_info_parser;
pub(crate) mod colmetadata_parser;
pub(crate) mod done_parser;
pub(crate) mod envchange_parser;
pub(crate) mod error_parser;
pub(crate) mod featureext_parser;
pub(crate) mod fedauth_parser;
pub(crate) mod info_parser;
pub(crate) mod loginack_parser;
pub(crate) mod nbcrow_parser;
pub(crate) mod order_parser;
pub(crate) mod returnstatus_parser;
pub(crate) mod returnvalue_parser;
pub(crate) mod row_parser;
pub(crate) mod session_state_parser;
pub(crate) mod sspi_parser;
pub(crate) mod tab_name_parser;

// Re-export TokenParser trait
pub(crate) use common::TokenParser;

// Re-export all parser types
pub(crate) use col_info_parser::ColInfoTokenParser;
pub(crate) use colmetadata_parser::ColMetadataTokenParser;
pub(crate) use done_parser::{DoneInProcTokenParser, DoneProcTokenParser, DoneTokenParser};
pub(crate) use envchange_parser::EnvChangeTokenParser;
pub(crate) use error_parser::ErrorTokenParser;
pub(crate) use featureext_parser::FeatureExtAckTokenParser;
pub(crate) use fedauth_parser::FedAuthInfoTokenParser;
pub(crate) use info_parser::InfoTokenParser;
pub(crate) use loginack_parser::LoginAckTokenParser;
pub(crate) use nbcrow_parser::NbcRowTokenParser;
pub(crate) use order_parser::OrderTokenParser;
pub(crate) use returnstatus_parser::ReturnStatusTokenParser;
pub(crate) use returnvalue_parser::ReturnValueTokenParser;
pub(crate) use row_parser::RowTokenParser;
pub(crate) use session_state_parser::SessionStateTokenParser;
pub(crate) use sspi_parser::SspiTokenParser;
pub(crate) use tab_name_parser::TabNameTokenParser;

// Fuzzing exports
#[cfg(fuzzing)]
pub use done_parser::DoneTokenParser as FuzzDoneTokenParser;
#[cfg(fuzzing)]
pub use envchange_parser::EnvChangeTokenParser as FuzzEnvChangeTokenParser;

// Tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_module_structure() {
        // Ensure all parsers can be instantiated
        let _ = DoneTokenParser::default();
        let _ = DoneInProcTokenParser::default();
        let _ = DoneProcTokenParser::default();
        let _ = EnvChangeTokenParser::default();
        let _ = ErrorTokenParser::default();
        let _ = FedAuthInfoTokenParser::default();
        let _ = FeatureExtAckTokenParser::default();
        let _ = InfoTokenParser::default();
        let _ = LoginAckTokenParser::default();
        let _ = OrderTokenParser::default();
        let _ = ReturnStatusTokenParser::default();
    }
}
