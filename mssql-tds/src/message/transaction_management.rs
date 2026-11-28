// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDS transaction management message types.
//!
//! Contains the wire-level request structures for `BEGIN`, `COMMIT`,
//! `ROLLBACK`, `SAVE`, and DTC operations sent via the TDS Transaction
//! Manager packet type.

use crate::connection::execution_context::ExecutionContext;
use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::message::headers::{TdsHeaders, TransactionDescriptorHeader, write_headers};
use crate::message::messages::PacketType::TransactionManager;
use crate::message::messages::{PacketType, Request};
use async_trait::async_trait;
use std::io::Error;

/// Parameters for creating a new transaction via `BEGIN TRANSACTION`.
#[derive(Debug, Clone)]
pub struct CreateTxnParams {
    /// Isolation level for the new transaction.
    pub level: TransactionIsolationLevel,
    /// Optional transaction name.
    pub name: Option<String>,
}

impl CreateTxnParams {
    async fn serialize(&self, writer: &mut PacketWriter<'_>) -> TdsResult<()> {
        writer.write_byte_async(self.level.clone() as u8).await?;
        match self.name.as_ref() {
            Some(name) => {
                writer.write_byte_async((name.len() * 2) as u8).await?;
                writer.write_string_unicode_async(name).await?;
                Ok(())
            }
            None => {
                writer.write_byte_async(0).await?;
                Ok(())
            }
        }
    }
}

/// TDS transaction manager request type.
///
/// Each variant maps to a TDS `TM_*` request ID sent in the
/// transaction manager payload.
#[repr(u16)]
pub enum TransactionManagementType {
    /// Retrieve the address of the Distributed Transaction Coordinator.
    GetDtcAddress = 0,
    /// Propagate a distributed transaction to another server.
    Propagate(String) = 1,
    /// Begin a new local transaction.
    Begin(CreateTxnParams) = 5,
    /// Promote a local transaction to a distributed transaction.
    Promote = 6,
    /// Commit the current transaction, optionally starting a new one.
    Commit {
        /// Name of the transaction to commit.
        name: Option<String>,
        /// If provided, a new transaction is started immediately after commit.
        create_txn_params: Option<CreateTxnParams>,
    } = 7,
    /// Roll back the current transaction (or to a savepoint), optionally starting a new one.
    Rollback {
        /// Savepoint or transaction name to roll back to.
        name: Option<String>,
        /// If provided, a new transaction is started immediately after rollback.
        create_txn_params: Option<CreateTxnParams>,
    } = 8,
    /// Create a savepoint within the current transaction.
    Save(String) = 9,
}

/// SQL Server transaction isolation level.
///
/// Maps to the TDS isolation-level byte in `BEGIN TRANSACTION` requests.
#[repr(u8)]
#[derive(Debug, Clone)]
pub enum TransactionIsolationLevel {
    /// Keep the current isolation level (server default).
    NoChange = 0x00,
    /// Allows dirty reads.
    ReadUncommitted = 0x01,
    /// Default level — prevents dirty reads.
    ReadCommitted = 0x02,
    /// Prevents dirty and non-repeatable reads.
    RepeatableRead = 0x03,
    /// Full isolation — prevents phantom reads.
    Serializable = 0x04,
    /// Row-versioning-based isolation.
    Snapshot = 0x05,
}

/// `fBeginXact`, bit 0 of a commit/rollback request's `XACT_FLAGS` (MS-TDS
/// 2.2.6.9); msodbcsql's `TM_BEGIN_NEW_XACT`. SQL Server 2022 treats any
/// non-zero byte as a begin request, but the other bits are reserved.
const TM_BEGIN_NEW_XACT: u8 = 0x01;

pub(crate) struct TransactionManagementRequest {
    pub(crate) headers: Vec<TdsHeaders>,
    pub(crate) transaction_params: TransactionManagementType,
}

impl TransactionManagementRequest {
    pub fn new(
        transaction_params: TransactionManagementType,
        execution_context: &ExecutionContext,
    ) -> Self {
        let transaction_descriptor_header = TransactionDescriptorHeader::new(
            execution_context.get_transaction_descriptor(),
            execution_context.get_outstanding_requests(),
        );
        Self {
            headers: Vec::from([transaction_descriptor_header.into()]),
            transaction_params,
        }
    }
}

impl From<&TransactionManagementType> for u16 {
    fn from(value: &TransactionManagementType) -> Self {
        use super::transaction_management::TransactionManagementType::{
            Begin, Commit, GetDtcAddress, Promote, Propagate, Rollback, Save,
        };

        match value {
            GetDtcAddress => 0,
            Propagate(..) => 1,
            Begin(..) => 5,
            Promote => 6,
            Commit { .. } => 7,
            Rollback { .. } => 8,
            Save(..) => 9,
        }
    }
}

#[async_trait]
impl Request for TransactionManagementRequest {
    fn packet_type(&self) -> PacketType {
        TransactionManager
    }

    async fn serialize<'a, 'b>(&'a self, packet_writer: &'a mut PacketWriter<'b>) -> TdsResult<()>
    where
        'b: 'a,
    {
        write_headers(&self.headers, packet_writer).await?;
        packet_writer
            .write_u16_async(u16::from(&self.transaction_params))
            .await?;
        match &self.transaction_params {
            TransactionManagementType::GetDtcAddress => {
                packet_writer.write_u16_async(0).await?;
            }
            TransactionManagementType::Propagate(payload) => {
                packet_writer
                    .write_u16_async((payload.len() * 2) as u16)
                    .await?;
                packet_writer.write_string_unicode_async(payload).await?;
            }
            TransactionManagementType::Begin(payload) => {
                payload.serialize(packet_writer).await?;
            }
            TransactionManagementType::Promote => (),
            TransactionManagementType::Commit {
                name,
                create_txn_params: new_transaction_metadata,
            }
            | TransactionManagementType::Rollback {
                name,
                create_txn_params: new_transaction_metadata,
            } => {
                if let Some(transaction_name) = &name {
                    packet_writer
                        .write_byte_async((transaction_name.len() * 2) as u8)
                        .await?;
                    packet_writer
                        .write_string_unicode_async(transaction_name)
                        .await?;
                } else {
                    packet_writer.write_byte_async(0).await?;
                }
                match new_transaction_metadata {
                    Some(new_transaction_metadata) => {
                        packet_writer.write_byte_async(TM_BEGIN_NEW_XACT).await?;
                        new_transaction_metadata.serialize(packet_writer).await?;
                    }
                    None => {
                        packet_writer.write_byte_async(0).await?;
                    }
                }
            }
            TransactionManagementType::Save(payload) => {
                // Savepoint must be non-empty.
                if payload.is_empty() {
                    Err(crate::error::Error::from(Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Saving a transaction without a savepoint name. Savepoint name must be non-empty.",
                    )))?
                }
                packet_writer
                    .write_byte_async((payload.len() * 2) as u8)
                    .await?;
                packet_writer.write_string_unicode_async(payload).await?;
            }
        }
        packet_writer.finalize().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use futures::executor::block_on;

    const PACKET_HEADER_LEN: usize = 8;
    const ALL_HEADERS_LEN: usize = 22;

    /// The request body after the packet header and ALL_HEADERS.
    fn request_body(transaction_params: TransactionManagementType) -> Vec<u8> {
        let mut mock = MockNetworkWriter::new(4096);
        let mut writer = PacketWriter::new(PacketType::TransactionManager, &mut mock, None, None);
        let request =
            TransactionManagementRequest::new(transaction_params, &ExecutionContext::new());
        block_on(request.serialize(&mut writer)).unwrap();
        drop(writer);
        let headers = &mock.data[PACKET_HEADER_LEN..];
        assert_eq!(&headers[..4], &(ALL_HEADERS_LEN as u32).to_le_bytes());
        headers[ALL_HEADERS_LEN..].to_vec()
    }

    fn begin_new(name: Option<&str>) -> Option<CreateTxnParams> {
        Some(CreateTxnParams {
            level: TransactionIsolationLevel::ReadCommitted,
            name: name.map(str::to_string),
        })
    }

    #[test]
    fn end_transaction_requests_set_begin_xact_as_bit_zero() {
        // XACT_NAME, XACT_FLAGS, then ISOLATION_LEVEL and BEGIN_XACT_NAME only
        // when fBeginXact is set.
        let cases: [(Option<&str>, Option<CreateTxnParams>, &[u8]); 4] = [
            (None, None, &[0x00, 0x00]),
            (Some("ab"), None, &[0x04, b'a', 0, b'b', 0, 0x00]),
            (None, begin_new(None), &[0x00, 0x01, 0x02, 0x00]),
            (
                Some("ab"),
                begin_new(Some("c")),
                &[0x04, b'a', 0, b'b', 0, 0x01, 0x02, 0x02, b'c', 0],
            ),
        ];
        for (name, create_txn_params, payload) in cases {
            for (request_type, params) in [
                (
                    7u16,
                    TransactionManagementType::Commit {
                        name: name.map(str::to_string),
                        create_txn_params: create_txn_params.clone(),
                    },
                ),
                (
                    8u16,
                    TransactionManagementType::Rollback {
                        name: name.map(str::to_string),
                        create_txn_params: create_txn_params.clone(),
                    },
                ),
            ] {
                let mut expected = request_type.to_le_bytes().to_vec();
                expected.extend_from_slice(payload);
                assert_eq!(
                    request_body(params),
                    expected,
                    "request {request_type}, name {name:?}, begin {create_txn_params:?}"
                );
            }
        }
    }

    #[test]
    fn begin_request_writes_isolation_then_name() {
        assert_eq!(
            request_body(TransactionManagementType::Begin(CreateTxnParams {
                level: TransactionIsolationLevel::Snapshot,
                name: Some("c".to_string()),
            })),
            [0x05, 0x00, 0x05, 0x02, b'c', 0]
        );
    }
}
