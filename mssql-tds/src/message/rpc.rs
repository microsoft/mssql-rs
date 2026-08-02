// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use tracing::debug;

use crate::{
    connection::execution_context::ExecutionContext,
    core::TdsResult,
    datatypes::encoder::GenericEncoder,
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

    async fn write_proc(&self, packet_writer: &mut PacketWriter<'_>) -> TdsResult<()> {
        match &self.rpc_type {
            RpcType::Named(stored_proc_name) => {
                // Write the procedure name to the packet writer
                packet_writer
                    .write_i16_async((stored_proc_name.len() as u8).into())
                    .await?;
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
