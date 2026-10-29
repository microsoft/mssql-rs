// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;

use crate::{
    connection::execution_context::ExecutionContext,
    core::TdsResult,
    io::packet_writer::{PacketWriter, TdsPacketWriter},
};

pub(crate) enum TdsHeaders {
    TransactionDescriptor(TransactionDescriptorHeader),
    #[allow(dead_code)]
    // This variant is not used currently, but may be used in the future for generating trace activity headers for requests.
    TraceActivity(TraceActivityHeader),
    #[allow(dead_code)]
    // This variant is not used currently, but may be used in the future for generating query notifications headers for requests.
    QueryNotifications(QueryNotificationsHeader),
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
        // MS-TDS requires the outstanding request count to be 1 when the
        // connection is operating in autocommit mode (section 2.2.5.3.2).
        Self::new(0, 1)
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

// QueryNotificationsHeader struct
pub(crate) struct QueryNotificationsHeader {
    notification_data: Vec<u8>,
}

impl QueryNotificationsHeader {
    #[allow(dead_code)]
    // This constructor is not used currently, but may be used in the future for generating query notifications headers for requests.
    pub fn new(notification_data: Vec<u8>) -> Self {
        Self { notification_data }
    }
}

#[async_trait]
impl TdsHeader for QueryNotificationsHeader {
    fn header_type(&self) -> u16 {
        0x0001 // HeaderType for QueryNotificationsHeader
    }

    fn calculate_length(&self) -> i32 {
        // Total length = HeaderLength (4 bytes) + HeaderType (2 bytes) + NotificationData length
        (6 + self.notification_data.len()) as i32
    }

    async fn write_async(&self, _writer: &mut PacketWriter) -> TdsResult<()> {
        let _length = self.calculate_length();
        unimplemented!("QueryNotificationsHeader::write_async");
        //     writer.write_int32_async(header_length).await; // Write HeaderLength
        //     writer.write_uint16_async(self.header_type()).await; // Write HeaderType
        //     writer.write_bytes_async(&self.notification_data).await; // Write NotificationData
    }
}

pub(crate) struct TraceActivityHeader {
    pub id: uuid::Uuid,
    pub sequence_number: i32,
}

impl TraceActivityHeader {
    #[allow(dead_code)]
    // This constructor is not used currently, but may be used in the future for generating trace activity headers for requests.
    pub fn new(id: uuid::Uuid) -> Self {
        // Interlocked.Increment(ref sequenceNumber);
        static SEQUENCE_NUMBER: AtomicU32 = AtomicU32::new(0);
        let sequence_number = SEQUENCE_NUMBER.fetch_add(1, Ordering::SeqCst) as i32;
        Self {
            id,
            sequence_number,
        }
    }
}

#[async_trait]
impl TdsHeader for TraceActivityHeader {
    fn header_type(&self) -> u16 {
        0x0003
    }

    fn calculate_length(&self) -> i32 {
        // Total length of header = HeaderLength (4 bytes) + HeaderType (2 bytes) + ActivityId (16 bytes) + Sequence Number (4)
        6 + 16 + 4
    }

    async fn write_async(&self, writer: &mut PacketWriter) -> TdsResult<()> {
        let header_len = self.calculate_length();
        writer.write_i32_async(header_len).await?;
        writer.write_u16_async(self.header_type()).await?;
        writer.write_async(self.id.as_bytes()).await?;
        writer.write_i32_async(self.sequence_number).await?;
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
    for header in headers {
        match header {
            TdsHeaders::TransactionDescriptor(header) => {
                header_len += header.calculate_length();
            }
            TdsHeaders::TraceActivity(header) => {
                header_len += header.calculate_length();
            }
            TdsHeaders::QueryNotifications(header) => {
                header_len += header.calculate_length();
            }
        }
    }

    packet_writer.write_i32_async(header_len).await?;
    for header in headers {
        match header {
            TdsHeaders::TransactionDescriptor(header) => {
                header.write_async(packet_writer).await?;
            }
            TdsHeaders::TraceActivity(header) => {
                header.write_async(packet_writer).await?;
            }
            TdsHeaders::QueryNotifications(header) => {
                header.write_async(packet_writer).await?;
            }
        }
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
    fn test_non_transaction_headers_use_autocommit_values() {
        let header1 = TransactionDescriptorHeader::create_non_transaction_header();
        let header2 = TransactionDescriptorHeader::create_non_transaction_header();
        assert_eq!(header1.transaction_descriptor, 0);
        assert_eq!(header2.transaction_descriptor, 0);
        assert_eq!(header1.outstanding_request_count, 1);
        assert_eq!(header2.outstanding_request_count, 1);
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
    fn test_query_notifications_header_new() {
        let data = vec![1, 2, 3, 4, 5];
        let header = QueryNotificationsHeader::new(data.clone());
        assert_eq!(header.notification_data, data);
    }

    #[test]
    fn test_query_notifications_header_type() {
        let header = QueryNotificationsHeader::new(vec![]);
        assert_eq!(header.header_type(), 0x0001);
    }

    #[test]
    fn test_query_notifications_calculate_length() {
        let header = QueryNotificationsHeader::new(vec![1, 2, 3]);
        assert_eq!(header.calculate_length(), 9);
    }

    #[test]
    fn test_query_notifications_calculate_length_empty() {
        let header = QueryNotificationsHeader::new(vec![]);
        assert_eq!(header.calculate_length(), 6);
    }

    #[test]
    fn test_trace_activity_header_new() {
        let uuid = uuid::Uuid::new_v4();
        let header = TraceActivityHeader::new(uuid);
        assert_eq!(header.id, uuid);
        assert!(header.sequence_number >= 0);
    }

    #[test]
    fn test_trace_activity_header_type() {
        let header = TraceActivityHeader::new(uuid::Uuid::new_v4());
        assert_eq!(header.header_type(), 0x0003);
    }

    #[test]
    fn test_trace_activity_calculate_length() {
        let header = TraceActivityHeader::new(uuid::Uuid::new_v4());
        assert_eq!(header.calculate_length(), 26);
    }

    #[test]
    fn test_trace_activity_sequence_numbers() {
        let header1 = TraceActivityHeader::new(uuid::Uuid::new_v4());
        let header2 = TraceActivityHeader::new(uuid::Uuid::new_v4());
        assert!(header2.sequence_number > header1.sequence_number);
    }

    #[test]
    fn test_tds_headers_from_transaction_descriptor() {
        let header = TransactionDescriptorHeader::new(123, 1);
        let tds_header = TdsHeaders::from(header);
        match tds_header {
            TdsHeaders::TransactionDescriptor(_) => {}
            _ => panic!("Expected TransactionDescriptor"),
        }
    }
}
