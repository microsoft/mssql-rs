// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use tracing::debug;

use crate::{
    connection::execution_context::ExecutionContext,
    core::TdsResult,
    datatypes::encoder::GenericEncoder,
    error::Error,
    io::packet_writer::{PacketWriter, TdsPacketWriter},
    message::messages::PacketType,
    token::tokens::SqlCollation,
};

use super::{
    headers::{TdsHeaders, TransactionDescriptorHeader},
    messages::Request,
    parameters::rpc_parameters::RpcParameter,
};
use crate::message::headers::write_headers;

pub(crate) const PROC_ID_SWITCH: u16 = 0xffff;
// [MS-TDS] 2.2.6.6 limits ProcName to 1046 bytes, encoded as US_VARCHAR.
const MAX_PROC_NAME_UTF16_UNITS: u16 = 523;
/// TDS 7.2+ delimiter between RPC requests carried in one RPC message.
pub(crate) const RPC_BATCH_DELIMITER: u8 = 0xff;

#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProcOptions {
    None = 0x00,
    #[allow(dead_code)]
    // This option is not implemented yet, but may be used in the future for forcing metadata recompile on the server.
    WithRecompile = 0x01,
    #[allow(dead_code)]
    // This option is not implemented yet, but may be used in the future for RPCs that do not require metadata to be sent.
    NoMetadata = 0x02,
    #[allow(dead_code)]
    // This option is not implemented yet, but may be used in the future for RPCs that can reuse metadata from a previous RPC.
    ReuseMetadata = 0x04,
}

/// Enum representing the different types of RPCs
/// that can be sent to the server.
pub(crate) enum RpcType {
    Named(String),
    ProcId(RpcProcs),
}

impl<'a> SqlRpc<'a> {
    pub fn new(
        rpc_type: RpcType,
        positional_parameters: Option<Vec<RpcParameter>>,
        named_parameters: Option<Vec<RpcParameter>>,
        db_collation: &'a SqlCollation,
        execution_context: &ExecutionContext,
    ) -> Self {
        let transaction_descriptor_header: TransactionDescriptorHeader = execution_context.into();

        Self {
            rpc_type,
            headers: Vec::from([transaction_descriptor_header.into()]),
            positional_parameters,
            named_parameters,
            db_collation,
            proc_options: ProcOptions::None,
        }
    }

    pub(crate) fn new_batch_command(
        rpc_type: RpcType,
        positional_parameters: Option<Vec<RpcParameter>>,
        named_parameters: Option<Vec<RpcParameter>>,
        db_collation: &'a SqlCollation,
        execution_context: &ExecutionContext,
        first: bool,
    ) -> Self {
        let headers = if first {
            Vec::from([TransactionDescriptorHeader::from(execution_context).into()])
        } else {
            Vec::new()
        };
        Self {
            rpc_type,
            headers,
            positional_parameters,
            named_parameters,
            db_collation,
            proc_options: ProcOptions::None,
        }
    }

    async fn write_positional_parameters(
        &self,
        packet_writer: &mut PacketWriter<'_>,
    ) -> TdsResult<()> {
        // Implement the logic for writing positional parameters
        // Example: Write a placeholder implementation
        if let Some(positional_parameters) = &self.positional_parameters {
            let encoder = GenericEncoder::new();
            for parameter in positional_parameters {
                parameter
                    .serialize(packet_writer, self.db_collation, true, &encoder)
                    .await?;
            }
        } else {
            debug!("Positional parameters are None, skipping serialization.");
        }
        Ok(())
    }

    async fn write_named_parameters(&self, packet_writer: &mut PacketWriter<'_>) -> TdsResult<()> {
        // Implement the logic for writing parameters
        // Example: Write a placeholder implementation
        if let Some(parameters) = &self.named_parameters {
            let encoder = GenericEncoder::new();
            for parameter in parameters {
                parameter
                    .serialize(packet_writer, self.db_collation, false, &encoder)
                    .await?;
            }
        }
        Ok(())
    }

    /// Serializes the RPC up to and including all fully-materialized
    /// parameters, but does **not** send the terminating `finalize` packet.
    ///
    /// Used by the incremental (streamed) PLP write path: the caller writes the
    /// header, proc, positional and materialized named parameters here, then
    /// appends one or more streamed parameters chunk-by-chunk before calling
    /// `finalize` itself. For the normal atomic send, use [`serialize`], which
    /// wraps this and then finalizes.
    pub(crate) async fn serialize_prefix<'s, 'b>(
        &'s self,
        packet_writer: &'s mut PacketWriter<'b>,
    ) -> TdsResult<()>
    where
        'b: 's,
    {
        write_headers(&self.headers, packet_writer).await?;
        self.write_proc(packet_writer).await?;
        self.write_positional_parameters(packet_writer).await?;
        self.write_named_parameters(packet_writer).await?;
        Ok(())
    }

    /// Serializes one command inside an RPC batch.
    ///
    /// The first command owns the message's `ALL_HEADERS`; later commands are
    /// introduced by the TDS 7.2+ `0xff` RPC delimiter and contain only their
    /// procedure and parameters. The caller finalizes the packet writer once
    /// after the last command.
    pub(crate) async fn serialize_batch_command(
        &self,
        packet_writer: &mut PacketWriter<'_>,
        first: bool,
    ) -> TdsResult<()> {
        if first {
            write_headers(&self.headers, packet_writer).await?;
        } else {
            packet_writer.write_byte_async(RPC_BATCH_DELIMITER).await?;
        }
        self.write_proc(packet_writer).await?;
        self.write_positional_parameters(packet_writer).await?;
        self.write_named_parameters(packet_writer).await
    }

    async fn write_proc(&self, packet_writer: &mut PacketWriter<'_>) -> TdsResult<()> {
        match &self.rpc_type {
            RpcType::Named(stored_proc_name) => {
                let name_len = u16::try_from(stored_proc_name.encode_utf16().count())
                    .ok()
                    .filter(|&len| len <= MAX_PROC_NAME_UTF16_UNITS)
                    .ok_or_else(|| {
                        Error::UsageError(format!(
                            "RPC procedure name exceeds {MAX_PROC_NAME_UTF16_UNITS} UTF-16 code units."
                        ))
                    })?;
                packet_writer.write_u16_async(name_len).await?;
                packet_writer
                    .write_string_unicode_async(stored_proc_name.as_str())
                    .await?;
            }
            RpcType::ProcId(proc) => {
                // Write the procedure ID to the packet writer
                packet_writer.write_u16_async(PROC_ID_SWITCH).await?;
                // Write the int32 value for the procedure ID
                packet_writer
                    .write_i16_async(proc.get_u8_value().into())
                    .await?;
            }
        }
        packet_writer
            .write_i16_async(self.proc_options as i16)
            .await?;
        Ok(())
    }
}

/// Well-known SQL Server system stored-procedure IDs used by the TDS RPC
/// message.
///
/// These correspond to the procedure-ID shortcut in the RPC request header,
/// avoiding the overhead of sending the procedure name as a string.
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum RpcProcs {
    Cursor = 1,
    CursorOpen = 2,
    CursorPrepare = 3,
    CursorExecute = 4,
    CursorPrepExec = 5,
    CursorUnprepare = 6,
    CursorFetch = 7,
    CursorOption = 8,
    CursorClose = 9,
    ExecuteSql = 10,
    Prepare = 11,
    Execute = 12,
    PrepExec = 13,
    PrepExecRpc = 14,
    Unprepare = 15,
}

impl RpcProcs {
    fn get_u8_value(&self) -> u8 {
        *self as u8
    }
}

pub(crate) struct SqlRpc<'param> {
    pub headers: Vec<TdsHeaders>,
    pub rpc_type: RpcType,
    pub positional_parameters: Option<Vec<RpcParameter>>,
    pub named_parameters: Option<Vec<RpcParameter>>,
    pub db_collation: &'param SqlCollation,
    pub proc_options: ProcOptions,
}

#[async_trait]
impl Request for SqlRpc<'_> {
    fn packet_type(&self) -> PacketType {
        PacketType::RpcRequest
    }

    async fn serialize<'a, 'b>(&'a self, packet_writer: &'a mut PacketWriter<'b>) -> TdsResult<()>
    where
        'b: 'a,
    {
        self.serialize_prefix(packet_writer).await?;
        packet_writer.finalize().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use futures::executor::block_on;

    #[test]
    fn named_proc_serializes_utf16_length_and_options() {
        for name in [
            String::new(),
            "dbo.proc".into(),
            "[dbo].[路由]".into(),
            "[dbo].[𐐀proc]".into(),
            "a".repeat(255),
            "a".repeat(256),
            "路".repeat(100),
            "a".repeat(usize::from(MAX_PROC_NAME_UTF16_UNITS)),
            "𐐀".repeat(261) + "a",
        ] {
            let mut mock = MockNetworkWriter::new(4096);
            let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
            let collation = SqlCollation::default();
            let rpc = SqlRpc::new(
                RpcType::Named(name.clone()),
                None,
                None,
                &collation,
                &ExecutionContext::new(),
            );
            block_on(rpc.write_proc(&mut writer)).unwrap();
            let mut expected = u16::try_from(name.encode_utf16().count())
                .unwrap()
                .to_le_bytes()
                .to_vec();
            expected.extend(name.encode_utf16().flat_map(u16::to_le_bytes));
            expected.extend([0, 0]);
            assert_eq!(&writer.get_payload().into_inner()[8..], expected);
        }
    }

    #[test]
    fn named_proc_rejects_oversized_names_before_writing() {
        for name in [
            "a".repeat(usize::from(MAX_PROC_NAME_UTF16_UNITS) + 1),
            "𐐀".repeat(262),
            "a".repeat(usize::from(PROC_ID_SWITCH)),
            "a".repeat(usize::from(u16::MAX) + 1),
        ] {
            let mut mock = MockNetworkWriter::new(4096);
            let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
            let collation = SqlCollation::default();
            let rpc = SqlRpc::new(
                RpcType::Named(name),
                None,
                None,
                &collation,
                &ExecutionContext::new(),
            );
            let before = writer.get_payload().into_inner();
            assert!(matches!(
                block_on(rpc.write_proc(&mut writer)),
                Err(Error::UsageError(_))
            ));
            assert_eq!(writer.get_payload().into_inner(), before);
            drop(writer);
            assert!(mock.data.is_empty());
        }
    }

    #[test]
    fn named_proc_encoding_is_shared_by_all_serializers() {
        for name in ["[dbo].[路由]".to_owned(), "a".repeat(256)] {
            let collation = SqlCollation::default();
            let mut context = ExecutionContext::new();
            context.set_transaction_descriptor(1);
            let rpc = SqlRpc::new(
                RpcType::Named(name.clone()),
                None,
                None,
                &collation,
                &context,
            );
            let mut expected = Vec::new();
            for route in 0..4 {
                let mut mock = MockNetworkWriter::new(4096);
                let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
                block_on(async {
                    match route {
                        0 => rpc.serialize(&mut writer).await.unwrap(),
                        1 => {
                            rpc.serialize_prefix(&mut writer).await.unwrap();
                            writer.finalize().await.unwrap();
                        }
                        _ => {
                            let first = route == 2;
                            let rpc = SqlRpc::new_batch_command(
                                RpcType::Named(name.clone()),
                                None,
                                None,
                                &collation,
                                &context,
                                first,
                            );
                            rpc.serialize_batch_command(&mut writer, first)
                                .await
                                .unwrap();
                            writer.finalize().await.unwrap();
                        }
                    }
                });
                drop(writer);
                let payload = &mock.data[8..];
                if route == 0 {
                    expected = payload.to_vec();
                } else if route == 3 {
                    assert_eq!(payload[0], RPC_BATCH_DELIMITER);
                    let header_len =
                        usize::try_from(u32::from_le_bytes(expected[..4].try_into().unwrap()))
                            .unwrap();
                    assert_eq!(&payload[1..], &expected[header_len..]);
                } else {
                    assert_eq!(payload, expected);
                }
            }
        }
    }

    #[test]
    fn proc_id_serialization_is_unchanged() {
        let mut mock = MockNetworkWriter::new(4096);
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let collation = SqlCollation::default();
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::ExecuteSql),
            None,
            None,
            &collation,
            &ExecutionContext::new(),
        );
        block_on(rpc.write_proc(&mut writer)).unwrap();
        assert_eq!(
            &writer.get_payload().into_inner()[8..],
            &[0xff, 0xff, 10, 0, 0, 0]
        );
    }
}
