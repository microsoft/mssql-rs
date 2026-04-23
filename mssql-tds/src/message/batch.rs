// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::headers::{TdsHeaders, TransactionDescriptorHeader, write_headers};
use super::messages::{PacketType, Request};
use crate::connection::execution_context::ExecutionContext;
use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use async_trait::async_trait;

pub(crate) struct SqlBatch {
    pub sql_command: String,
    pub headers: Vec<TdsHeaders>,
}

impl Default for SqlBatch {
    fn default() -> Self {
        let transaction_descriptor_header =
            TransactionDescriptorHeader::create_non_transaction_header();
        Self {
            sql_command: String::new(),
            headers: Vec::from([transaction_descriptor_header.into()]),
        }
    }
}

impl SqlBatch {
    pub fn new(sql_command: String, execution_context: &ExecutionContext) -> Self {
        let transaction_descriptor_header = match execution_context.get_transaction_descriptor() {
            0 => TransactionDescriptorHeader::create_non_transaction_header(),
            transaction_descriptor => TransactionDescriptorHeader::new(
                transaction_descriptor,
                execution_context.get_outstanding_requests(),
            ),
        };
        Self {
            sql_command,
            headers: Vec::from([transaction_descriptor_header.into()]),
        }
    }
}

#[async_trait]
impl Request for SqlBatch {
    fn packet_type(&self) -> PacketType {
        PacketType::SqlBatch
    }

    async fn serialize<'a, 'b>(&'a self, packet_writer: &'a mut PacketWriter<'b>) -> TdsResult<()>
    where
        'b: 'a,
    {
        write_headers(&self.headers, packet_writer).await?;
        packet_writer
            .write_string_unicode_async(&self.sql_command)
            .await?;
        packet_writer.finalize().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_has_empty_command() {
        let batch = SqlBatch::default();
        assert!(batch.sql_command.is_empty());
        assert_eq!(batch.headers.len(), 1);
    }

    #[test]
    fn test_new_with_non_transaction_context() {
        let ctx = ExecutionContext::new();
        let batch = SqlBatch::new("SELECT 1".to_string(), &ctx);
        assert_eq!(batch.sql_command, "SELECT 1");
        assert_eq!(batch.headers.len(), 1);
    }

    #[test]
    fn test_packet_type() {
        let batch = SqlBatch::default();
        assert_eq!(batch.packet_type() as u8, PacketType::SqlBatch as u8);
    }
}
