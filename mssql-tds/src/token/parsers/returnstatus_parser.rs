// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # RETURNSTATUS Token Parser
//!
//! Parses RETURNSTATUS tokens (0x79) which contain the return value from
//! a stored procedure or function executed via RPC (Remote Procedure Call).
//!
//! ## Token Byte Layout
//!
//! ```text
//! ┌──────────────────────────────┐
//! │  RETURNSTATUS Token (4 bytes)│
//! ├──────────────────────────────┤
//! │         Return Value         │
//! │          (4 bytes)           │
//! │           INT32              │
//! └──────────────────────────────┘
//!             0-3
//!
//! Return Value:
//!   Signed 32-bit integer containing the RETURN value
//!   from a stored procedure.
//!
//!   Common conventions:
//!     0    = Success (by convention)
//!     -1   = General failure
//!     -2   = Specific error condition
//!     > 0  = Application-specific success codes
//!     < 0  = Application-specific error codes
//! ```
//!
//! ## When This Token Appears
//!
//! RETURNSTATUS tokens are sent:
//! - After executing a stored procedure via SqlRpc
//! - Before DONEPROC token
//! - After all result sets and output parameters
//! - Only if the procedure has a RETURN statement
//!
//! ## Token Flow Example
//!
//! ## Example
//!
//! ```text
//! // Execute: EXEC spGetUser @userId = 123
//! // Server sends (in order):
//! //   1. ColMetadata (if procedure returns result set)
//! //   2. Row tokens (result set data)
//! //   3. DONE (result set completion)
//! //   4. RETURNSTATUS token (Value = 0 for success)
//! //   5. DONEPROC (procedure completion)
//! ```
//!
//! ## Example Stored Procedure
//!
//! ```sql
//! CREATE PROCEDURE spCheckUser @userId INT
//! AS
//! BEGIN
//!     IF EXISTS (SELECT 1 FROM Users WHERE Id = @userId)
//!         RETURN 0;  -- Success
//!     ELSE
//!         RETURN -1; -- User not found
//! END
//! ```
//!
//! The RETURNSTATUS token will contain 0 or -1 depending on the result.

use async_trait::async_trait;

use super::super::tokens::{ReturnStatusToken, Tokens};
use super::common::TokenParser;
use crate::io::token_stream::ParserContext;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};

/// Parser for RETURNSTATUS token (0x79) - stored procedure return value
#[derive(Default)]
pub(crate) struct ReturnStatusTokenParser {}

#[async_trait]
impl<T> TokenParser<T> for ReturnStatusTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        // Read the return value (4 bytes) - signed 32-bit integer
        // This is the value from a stored procedure's RETURN statement
        let value = reader.read_int32().await?;

        Ok(Tokens::from(ReturnStatusToken { value }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::test_utils::MockReader;
    use super::*;

    #[tokio::test]
    async fn test_parse_returnstatus_success() {
        let mut reader = MockReader::from_i32(0);
        let parser = ReturnStatusTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ReturnStatus(token) => {
                assert_eq!(token.value, 0);
            }
            _ => panic!("Expected ReturnStatus token"),
        }
    }

    #[tokio::test]
    async fn test_parse_returnstatus_error() {
        let mut reader = MockReader::from_i32(-1);
        let parser = ReturnStatusTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ReturnStatus(token) => {
                assert_eq!(token.value, -1);
            }
            _ => panic!("Expected ReturnStatus token"),
        }
    }

    #[tokio::test]
    async fn test_parse_returnstatus_positive() {
        let mut reader = MockReader::from_i32(42);
        let parser = ReturnStatusTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ReturnStatus(token) => {
                assert_eq!(token.value, 42);
            }
            _ => panic!("Expected ReturnStatus token"),
        }
    }

    #[tokio::test]
    async fn test_parse_returnstatus_max_int() {
        let mut reader = MockReader::from_i32(i32::MAX);
        let parser = ReturnStatusTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ReturnStatus(token) => {
                assert_eq!(token.value, i32::MAX);
            }
            _ => panic!("Expected ReturnStatus token"),
        }
    }

    #[tokio::test]
    async fn test_parse_returnstatus_min_int() {
        let mut reader = MockReader::from_i32(i32::MIN);
        let parser = ReturnStatusTokenParser::default();
        let context = ParserContext::default();
        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ReturnStatus(token) => {
                assert_eq!(token.value, i32::MIN);
            }
            _ => panic!("Expected ReturnStatus token"),
        }
    }
}
