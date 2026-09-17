// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;

use crate::{
    connection::execution_context::ExecutionContext,
    core::TdsResult,
    io::packet_writer::{PacketWriter, TdsPacketWriter},
};

// Static counter for non-transaction request count
static NON_TRANSACTION_REQUEST_COUNT: AtomicU32 = AtomicU32::new(0);

pub(crate) enum TdsHeaders {
    TransactionDescriptor(TransactionDescriptorHeader),
}

impl From<TransactionDescriptorHeader> for TdsHeaders {
    fn from(header: TransactionDescriptorHeader) -> Self {
        TdsHeaders::TransactionDescriptor(header)
    }
}

// Trait representing the abstract TdsHeader
#[async_trait]
pub(crate) trait TdsHeader {
    fn header_type(&self) -> u16;
    fn calculate_length(&self) -> i32;
    async fn write_async(&self, writer: &mut PacketWriter) -> TdsResult<()>;
}

// Struct for TransactionDescriptorHeader
pub(crate) struct TransactionDescriptorHeader {
    transaction_descriptor: u64,
    outstanding_request_count: u32,
}

impl TransactionDescriptorHeader {
    pub fn new(transaction_descriptor: u64, outstanding_request_count: u32) -> Self {
        Self {
            transaction_descriptor,
            outstanding_request_count,
        }
    }

    pub fn create_non_transaction_header() -> Self {
        let count = NON_TRANSACTION_REQUEST_COUNT.fetch_add(1, Ordering::SeqCst);
        Self::new(0, count + 1)
    }
}

impl From<&ExecutionContext> for TransactionDescriptorHeader {
    fn from(execution_context: &ExecutionContext) -> Self {
        match execution_context.get_transaction_descriptor() {
            0 => Self::create_non_transaction_header(),
            transaction_descriptor => Self::new(
                transaction_descriptor,
                execution_context.get_outstanding_requests(),
            ),
        }
    }
}

#[async_trait]
impl TdsHeader for TransactionDescriptorHeader {
    fn header_type(&self) -> u16 {
        0x0002
    }

    fn calculate_length(&self) -> i32 {
        18 // 4 (HeaderLength) + 2 (HeaderType) + 8 (TransactionDescriptor) + 4 (OutstandingRequestCount)
    }

    async fn write_async(&self, writer: &mut PacketWriter) -> TdsResult<()> {
        let header_length = self.calculate_length();
        writer.write_i32_async(header_length).await?; // HeaderLength
        writer.write_u16_async(self.header_type()).await?; // HeaderType
        writer.write_u64_async(self.transaction_descriptor).await?; // TransactionDescriptor
        writer
            .write_u32_async(self.outstanding_request_count)
            .await?; // OutstandingRequestCount
        Ok(())
    }
}

/// Writes the set of headers to the packet writer.
pub(crate) async fn write_headers(
    headers: &Vec<TdsHeaders>,
    packet_writer: &mut PacketWriter<'_>,
) -> TdsResult<()> {
    let _ = packet_writer;

    // Start with the length field size.
    let mut header_len = 4;
    for TdsHeaders::TransactionDescriptor(header) in headers {
        header_len += header.calculate_length();
    }

    packet_writer.write_i32_async(header_len).await?;
    for TdsHeaders::TransactionDescriptor(header) in headers {
        header.write_async(packet_writer).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_descriptor_header_new() {
        let header = TransactionDescriptorHeader::new(12345, 1);
        assert_eq!(header.transaction_descriptor, 12345);
        assert_eq!(header.outstanding_request_count, 1);
    }

    #[test]
    fn test_transaction_descriptor_header_create_non_transaction() {
        let header1 = TransactionDescriptorHeader::create_non_transaction_header();
        let header2 = TransactionDescriptorHeader::create_non_transaction_header();
        assert_eq!(header1.transaction_descriptor, 0);
        assert_eq!(header2.transaction_descriptor, 0);
        assert!(header2.outstanding_request_count > header1.outstanding_request_count);
    }

    #[test]
    fn test_transaction_descriptor_header_type() {
        let header = TransactionDescriptorHeader::new(0, 1);
        assert_eq!(header.header_type(), 0x0002);
    }

    #[test]
    fn test_transaction_descriptor_calculate_length() {
        let header = TransactionDescriptorHeader::new(0, 1);
        assert_eq!(header.calculate_length(), 18);
    }

    #[test]
    fn test_tds_headers_from_transaction_descriptor() {
        let header = TransactionDescriptorHeader::new(123, 1);
        let tds_header = TdsHeaders::from(header);
        let TdsHeaders::TransactionDescriptor(header) = tds_header;
        assert_eq!(header.transaction_descriptor, 123);
        assert_eq!(header.outstanding_request_count, 1);
    }
}
