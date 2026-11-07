// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use tracing::debug;

use crate::{
    connection::execution_context::ExecutionContext,
    core::TdsResult,
    datatypes::encoder::GenericEncoder,
    error::{Error, TvpParameterRef},
    io::packet_writer::{PacketWriter, TdsPacketWriter, TdsPacketWriterUnchecked},
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
// PROC_ID_SWITCH, the procedure id and the option flags.
const PROC_ID_LEN: usize = 6;
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

/// How an RPC sends the values in its `named_parameters`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ValueBinding {
    /// With their names; the server matches each one to its declaration.
    #[default]
    ByName,
    /// With zero-length names, so the server binds them by ordinal.
    ///
    /// Valid only when the values follow the order of the `@params`
    /// declaration. That holds for any `sp_executesql` or `sp_prepexec` whose
    /// declaration is built from the same list. SQL Server matches named
    /// values to the declaration in time quadratic in the parameter count;
    /// msodbcsql sends these values unnamed (`AddRPCUserParameters`).
    ByOrdinal,
}

/// The RPC ordinal and declared name of each output value among `values`,
/// whose first element has RPC ordinal `first_ordinal`.
///
/// SQL Server returns an output value bound by ordinal with an empty name
/// and this ordinal, which counts every parameter of the RPC.
pub(crate) fn output_names_by_ordinal<'p>(
    first_ordinal: usize,
    values: impl IntoIterator<Item = &'p RpcParameter>,
) -> Vec<(u16, String)> {
    values
        .into_iter()
        .enumerate()
        .filter(|(_, value)| value.is_output())
        .filter_map(|(index, value)| {
            Some((
                u16::try_from(first_ordinal + index).ok()?,
                value.name.clone()?,
            ))
        })
        .collect()
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
            value_binding: ValueBinding::ByName,
        }
    }

    pub(crate) fn with_value_binding(mut self, binding: ValueBinding) -> Self {
        self.value_binding = binding;
        self
    }

    pub(crate) fn value_binding(&self) -> ValueBinding {
        self.value_binding
    }

    /// See [`output_names_by_ordinal`]. `trailing` are values appended after
    /// this message's own, such as streamed parameters.
    pub(crate) fn output_names_by_ordinal<'p>(
        &'p self,
        trailing: impl IntoIterator<Item = &'p RpcParameter>,
    ) -> Vec<(u16, String)> {
        if self.value_binding == ValueBinding::ByName {
            return Vec::new();
        }
        output_names_by_ordinal(
            self.positional_parameters.as_ref().map_or(0, Vec::len),
            self.named_parameters.iter().flatten().chain(trailing),
        )
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
            value_binding: ValueBinding::ByName,
        }
    }

    /// Validates every parameter before the message writes anything.
    ///
    /// `PacketWriter` sends on overflow, so once serialization starts a later
    /// parameter's invalid value can only be reported after earlier bytes have
    /// already left. Running the checks first keeps locally-invalid input a
    /// local failure instead of a half-sent RPC needing cancel-and-drain.
    ///
    /// SCOPE: this covers one RPC message. In a *batched* prepared execution
    /// each row is its own command, appended to a shared writer by
    /// `serialize_batch_command`, so this runs per command and a later row's
    /// invalid parameter is still reported after earlier rows have flushed -
    /// `finish_send` retracts the request in that case. Closing that would
    /// mean validating every row before the first is written, which the
    /// streaming row iterator does not allow without materializing the whole
    /// batch. The batch path already aborts mid-batch the same way for
    /// `reject_data_at_exec` and the ForceColumnEncryption check
    /// (`tds_client.rs`), so this shares an existing property rather than
    /// adding one; tracked in AB#48248.
    fn validate_parameters(&self) -> TdsResult<()> {
        for parameter in self.positional_parameters.iter().flatten() {
            parameter.validate_before_send(self.db_collation)?;
        }
        for parameter in self.named_parameters.iter().flatten() {
            // An unnamed value here would be missing from the `@params`
            // declaration, so binding by ordinal would shift every later value.
            if parameter.name.is_none() {
                return Err(Error::UsageError(
                    "Named RPC parameters must have a name.".to_string(),
                ));
            }
            parameter.validate_named_before_send(self.db_collation)?;
        }
        Ok(())
    }

    async fn write_positional_parameters(
        &self,
        packet_writer: &mut PacketWriter<'_>,
    ) -> TdsResult<()> {
        // Implement the logic for writing positional parameters
        // Example: Write a placeholder implementation
        if let Some(positional_parameters) = &self.positional_parameters {
            let encoder = GenericEncoder::new();
            for (index, parameter) in positional_parameters.iter().enumerate() {
                if let Err(error) = parameter
                    .serialize(packet_writer, self.db_collation, true, &encoder)
                    .await
                {
                    return Err(error.with_tvp_parameter_context(
                        parameter.tvp_parameter_ref(TvpParameterRef::RpcPosition(index + 1)),
                    ));
                }
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
                if let Err(error) = parameter
                    .serialize(
                        packet_writer,
                        self.db_collation,
                        self.value_binding == ValueBinding::ByOrdinal,
                        &encoder,
                    )
                    .await
                {
                    return Err(
                        error.with_tvp_parameter_context(parameter.tvp_parameter_ref(
                            TvpParameterRef::Name(
                                parameter.name.as_deref().unwrap_or("unnamed").to_string(),
                            ),
                        )),
                    );
                }
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
        self.validate_parameters()?;
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
        self.validate_parameters()?;
        if first {
            write_headers(&self.headers, packet_writer).await?;
        } else {
            packet_writer.write_byte_async(RPC_BATCH_DELIMITER).await?;
        }
        self.write_proc(packet_writer).await?;
        self.write_positional_parameters(packet_writer).await?;
        self.write_named_parameters(packet_writer).await
    }

    /// Writes a non-first batch command without awaiting when its exact
    /// encoded length ends strictly before the packet boundary; otherwise
    /// returns `false` having written nothing.
    ///
    /// Below the boundary the async path cannot flush, so it changes only
    /// buffer bytes. The bound is strict because an exact fill is flushed at
    /// different points by different async primitives, which decides whether
    /// `finalize` sends a header-only EOM.
    pub(crate) fn try_write_batch_command_within_packet(
        &self,
        packet_writer: &mut PacketWriter<'_>,
        first: bool,
    ) -> TdsResult<bool> {
        let RpcType::ProcId(proc) = self.rpc_type else {
            return Ok(false);
        };
        let Some(parameters) = &self.positional_parameters else {
            return Ok(false);
        };
        if first
            || self
                .named_parameters
                .as_ref()
                .is_some_and(|parameters| !parameters.is_empty())
        {
            return Ok(false);
        }
        let mut length = 1 + PROC_ID_LEN;
        for parameter in parameters {
            let Some(parameter_length) = parameter.positional_inline_len() else {
                return Ok(false);
            };
            length += parameter_length;
        }
        if !packet_writer.has_space(length + 1) {
            return Ok(false);
        }
        self.validate_parameters()?;

        let start = packet_writer.position();
        packet_writer.write_bytes_within_packet(&[RPC_BATCH_DELIMITER]);
        packet_writer.write_bytes_within_packet(&self.proc_id_bytes(proc));
        for parameter in parameters {
            parameter.write_positional_inline(packet_writer, self.db_collation);
        }
        debug_assert_eq!((packet_writer.position() - start) as usize, length);
        debug_assert!(packet_writer.has_space(1));
        Ok(true)
    }

    fn proc_id_bytes(&self, proc: RpcProcs) -> [u8; PROC_ID_LEN] {
        let switch = PROC_ID_SWITCH.to_le_bytes();
        let id = i16::from(proc.get_u8_value()).to_le_bytes();
        let options = (self.proc_options as i16).to_le_bytes();
        [switch[0], switch[1], id[0], id[1], options[0], options[1]]
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
                return packet_writer
                    .write_fixed_bytes(&self.proc_id_bytes(*proc))
                    .await;
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
    value_binding: ValueBinding,
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
    use crate::datatypes::{sql_string::SqlString, sqltypes::SqlType};
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use crate::message::parameters::rpc_parameters::StatusFlags;
    use futures::executor::block_on;

    fn insert_parameters(row: i32) -> Vec<RpcParameter> {
        [
            SqlType::Int(Some(42)),
            SqlType::Int(Some(row)),
            SqlType::NVarchar(Some(SqlString::from_utf8_string("row-你好-𐐀".into())), 15),
            SqlType::BigInt(Some(i64::from(row) * 1000)),
        ]
        .into_iter()
        .map(|value| RpcParameter::new(None, StatusFlags::NONE, value))
        .collect()
    }

    fn serialize_insert_batch(packet_size: u32) -> Vec<u8> {
        let mut mock = MockNetworkWriter::new(packet_size);
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let collation = SqlCollation::default();
        let mut context = ExecutionContext::new();
        context.set_transaction_descriptor(1);
        block_on(async {
            for row in 0..2000 {
                let rpc = SqlRpc::new_batch_command(
                    RpcType::ProcId(RpcProcs::Execute),
                    Some(insert_parameters(row)),
                    None,
                    &collation,
                    &context,
                    row == 0,
                );
                rpc.serialize_batch_command(&mut writer, row == 0)
                    .await
                    .unwrap();
            }
            writer.finalize().await.unwrap();
        });
        drop(writer);
        mock.data
    }

    #[test]
    fn tvp_serialization_error_uses_the_original_odbc_ordinal() {
        let collation = SqlCollation::default();
        let table = SqlType::Table(
            crate::datatypes::sql_tvp::TvpTypeName::new(
                Some("dbo".to_string()),
                "ExampleType".to_string(),
            ),
            Some(crate::datatypes::sql_tvp::TvpTableData::new(
                vec![crate::datatypes::sql_tvp::TvpColumnDef::new(
                    SqlType::Varchar(None, 2),
                )],
                vec![vec![SqlType::Varchar(
                    Some(SqlString::from_utf8_string("too long".to_string())),
                    2,
                )]],
            )),
        );
        let parameter = RpcParameter::new(None, StatusFlags::NONE, table).with_odbc_ordinal(2);
        let context = ExecutionContext::new();
        let rpc = SqlRpc::new(
            RpcType::Named("proc".to_string()),
            Some(vec![parameter]),
            None,
            &collation,
            &context,
        );
        let mut mock = MockNetworkWriter::new(4096);
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);

        let error = block_on(rpc.serialize_prefix(&mut writer)).unwrap_err();
        assert!(matches!(
            &error,
            Error::TvpCellSerialization {
                parameter,
                row: 1,
                column: 1,
                ..
            } if parameter == &TvpParameterRef::OdbcOrdinal(2)
        ));
        assert!(
            error
                .to_string()
                .contains("TVP parameter ODBC ordinal 2, row 1, column 1")
        );
    }

    #[test]
    fn prepared_insert_batch_preserves_parameters_across_packet_boundaries() {
        let collation = SqlCollation::default();
        let mut expected = Vec::new();
        for row in 0i32..2000 {
            if row != 0 {
                expected.push(RPC_BATCH_DELIMITER);
            }
            expected.extend([0xff, 0xff, 12, 0, 0, 0]);
            for value in [42, row] {
                expected.extend([0, 0, 0x26, 4, 4]);
                expected.extend(value.to_le_bytes());
            }
            expected.extend([0, 0, 0xe7, 30, 0]);
            expected.extend(collation.info.to_le_bytes());
            expected.push(collation.sort_id);
            let text: Vec<u8> = "row-你好-𐐀"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            expected.extend(u16::try_from(text.len()).unwrap().to_le_bytes());
            expected.extend(text);
            expected.extend([0, 0, 0x26, 8, 8]);
            expected.extend((i64::from(row) * 1000).to_le_bytes());
        }
        for packet_size in [512, 4096, 8000, 16192] {
            let wire = serialize_insert_batch(packet_size);
            let mut remaining = wire.as_slice();
            let mut payload = Vec::new();
            let mut packet_id = 1u8;
            while !remaining.is_empty() {
                assert_eq!(remaining[0], 3);
                assert_eq!(remaining[6], packet_id);
                packet_id = packet_id.wrapping_add(1);
                let length = usize::from(u16::from_be_bytes([remaining[2], remaining[3]]));
                assert!(length <= usize::try_from(packet_size).unwrap());
                assert_eq!(remaining[1], u8::from(length == remaining.len()));
                payload.extend_from_slice(&remaining[8..length]);
                remaining = &remaining[length..];
            }
            let header_len =
                usize::try_from(u32::from_le_bytes(payload[..4].try_into().unwrap())).unwrap();
            assert_eq!(header_len, 22);
            assert_eq!(&payload[10..18], &1u64.to_le_bytes());
            assert_eq!(&payload[header_len..], expected);
        }
    }

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
    fn output_names_by_ordinal_count_leading_and_trailing_parameters() {
        let collation = SqlCollation::default();
        let named = |name: &str, flags| {
            RpcParameter::new(Some(name.to_string()), flags, SqlType::Int(None))
        };
        let build = |binding| {
            SqlRpc::new(
                RpcType::ProcId(RpcProcs::Execute),
                Some(vec![param(SqlType::Int(Some(55)))]),
                Some(vec![
                    named("@P1", StatusFlags::NONE),
                    named("@P2", StatusFlags::BY_REF_VALUE),
                ]),
                &collation,
                &ExecutionContext::new(),
            )
            .with_value_binding(binding)
        };
        let trailing = [named("@P3", StatusFlags::BY_REF_VALUE)];
        assert!(
            build(ValueBinding::ByName)
                .output_names_by_ordinal(&trailing)
                .is_empty()
        );
        assert_eq!(
            build(ValueBinding::ByOrdinal).output_names_by_ordinal(&trailing),
            vec![(2, "@P2".to_string()), (3, "@P3".to_string())]
        );
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

    /// An invalid value on a *later* parameter must fail before the message
    /// writes anything. `PacketWriter` sends on overflow, so once
    /// serialization starts an earlier parameter can already have flushed
    /// whole packets - the error would then arrive as a half-sent RPC needing
    /// cancel-and-drain rather than a local failure.
    ///
    /// Covers a UDT name too long for its B_VARCHAR count, an unsupported
    /// sql_variant inner type, vector metadata, raw NTEXT byte alignment, and
    /// a legacy LOB over 2 GiB - 1 bytes.
    #[test]
    fn an_invalid_later_parameter_sends_nothing() {
        use crate::datatypes::sql_string::EncodingType;
        use crate::datatypes::sql_udt::UdtTypeName;

        #[cfg_attr(not(target_pointer_width = "64"), allow(unused_mut))]
        let mut invalid_values = vec![
            SqlType::Udt(
                UdtTypeName::new(None, None, "c".repeat(256)),
                Some(vec![0x01]),
            ),
            // `sql_variant` cannot carry a UDT; rejected by
            // `validate_variant_inner`, which `write_type_info` only reaches
            // after this parameter's name and flags are already written.
            SqlType::Variant(Box::new(SqlType::Udt(
                UdtTypeName::new(None, None, "Point".to_string()),
                Some(vec![0x01]),
            ))),
            // A vector whose declared dimensions disagree with its value.
            // Reachable from outside this crate - `mssql-py-core` builds these
            // from application input.
            SqlType::Vector(
                Some(
                    crate::datatypes::sql_vector::SqlVector::try_from_f32(vec![1.0, 2.0, 3.0])
                        .expect("three f32 elements is a valid vector"),
                ),
                7,
                crate::datatypes::sqldatatypes::VectorBaseType::Float32,
            ),
            SqlType::NText(Some(SqlString::new(vec![b'a'], EncodingType::Utf16))),
            SqlType::NText(Some(SqlString::new(vec![b'a'], EncodingType::DelayedSet))),
        ];
        // Zero-filled and never read, so its pages stay untouched.
        #[cfg(target_pointer_width = "64")]
        invalid_values.push(SqlType::Image(Some(vec![0; 0x8000_0000])));

        for invalid in invalid_values {
            // First parameter is large enough to overflow the 512-byte packet
            // on its own, so a missing preflight is observable as sent bytes.
            let filler = SqlString::from_utf8_string("x".repeat(600));
            let parameters = vec![
                RpcParameter::new(
                    None,
                    StatusFlags::NONE,
                    SqlType::NVarchar(Some(filler), 4000),
                ),
                RpcParameter::new(None, StatusFlags::NONE, invalid),
            ];

            // Packet size comes from the mock writer, not from
            // `PacketWriter::new` (whose third argument is a timeout).
            let mut mock = MockNetworkWriter::new(512);
            let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
            let collation = SqlCollation::default();
            let rpc = SqlRpc::new(
                RpcType::ProcId(RpcProcs::ExecuteSql),
                Some(parameters),
                None,
                &collation,
                &ExecutionContext::new(),
            );

            let result = block_on(rpc.serialize_prefix(&mut writer));
            // `UsageError` for the name/variant rules, `TypeConversionError`
            // for a vector whose declaration disagrees with its value.
            assert!(result.is_err(), "expected the invalid value to be rejected");
            assert!(
                mock.data.is_empty(),
                "no packet may reach the network before every parameter is validated"
            );
        }
    }

    /// The named counterpart: an overlong name on a *later* named parameter
    /// must also fail before anything is written. `serialize` length-checks
    /// the name only on the named path, which is why the preflight splits the
    /// two rather than validating names it will never write.
    #[test]
    fn an_overlong_name_on_a_later_named_parameter_sends_nothing() {
        let filler = SqlString::from_utf8_string("x".repeat(600));
        let parameters = vec![
            RpcParameter::new(
                Some("@ok".to_string()),
                StatusFlags::NONE,
                SqlType::NVarchar(Some(filler), 4000),
            ),
            RpcParameter::new(
                Some(format!("@{}", "n".repeat(0xFF))),
                StatusFlags::NONE,
                SqlType::Int(Some(1)),
            ),
        ];

        let mut mock = MockNetworkWriter::new(512);
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let collation = SqlCollation::default();
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::ExecuteSql),
            None,
            Some(parameters),
            &collation,
            &ExecutionContext::new(),
        );

        let result = block_on(rpc.serialize_prefix(&mut writer));
        assert!(matches!(result, Err(crate::error::Error::UsageError(_))));
        assert!(
            mock.data.is_empty(),
            "no packet may reach the network before every parameter is validated"
        );
    }

    #[test]
    fn an_overlong_encoded_later_parameter_sends_nothing() {
        let parameters = vec![
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::NVarchar(Some(SqlString::from_utf8_string("x".repeat(600))), 4000),
            ),
            RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::Varchar(Some(SqlString::from_utf8_string("éé".to_string())), 3),
            ),
        ];

        let mut mock = MockNetworkWriter::new(512);
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let collation = SqlCollation {
            info: 0x0409,
            lcid_language_id: 0,
            col_flags: 0x40,
            sort_id: 0,
        };
        let rpc = SqlRpc::new(
            RpcType::ProcId(RpcProcs::ExecuteSql),
            Some(parameters),
            None,
            &collation,
            &ExecutionContext::new(),
        );

        assert!(matches!(
            block_on(rpc.serialize_prefix(&mut writer)),
            Err(crate::error::Error::EncodedValueTooLong {
                actual: 4,
                maximum: 3
            })
        ));
        assert!(
            mock.data.is_empty(),
            "encoded length validation must finish before the first packet is sent"
        );
    }

    struct BatchRun {
        result: String,
        wire: Vec<u8>,
        /// Writer position before each command, and whether the fast path took it.
        commands: Vec<(usize, bool)>,
    }

    /// Writes `rows` as one batch either through `serialize_batch_command`
    /// alone or through the production try-then-fallback sequence in
    /// `execute_sp_execute_batch`.
    fn run_batch(
        packet_size: u32,
        collation: &SqlCollation,
        rows: &[Vec<RpcParameter>],
        fast_path: bool,
    ) -> BatchRun {
        let mut context = ExecutionContext::new();
        context.set_transaction_descriptor(1);
        let mut mock = MockNetworkWriter::new(packet_size);
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let mut commands = Vec::with_capacity(rows.len());
        let result = block_on(async {
            for (index, row) in rows.iter().enumerate() {
                let first = index == 0;
                let rpc = SqlRpc::new_batch_command(
                    RpcType::ProcId(RpcProcs::Execute),
                    Some(row.clone()),
                    None,
                    collation,
                    &context,
                    first,
                );
                let position = usize::try_from(writer.position()).unwrap();
                let fast =
                    fast_path && rpc.try_write_batch_command_within_packet(&mut writer, first)?;
                commands.push((position, fast));
                if !fast {
                    rpc.serialize_batch_command(&mut writer, first).await?;
                }
            }
            writer.finalize().await
        });
        drop(writer);
        BatchRun {
            result: format!("{result:?}"),
            wire: mock.data,
            commands,
        }
    }

    /// Encoded command length measured on the async path alone, in a packet
    /// large enough that nothing flushes.
    fn command_len(row: &[RpcParameter], first: bool) -> usize {
        let collation = db_collation();
        let mut context = ExecutionContext::new();
        context.set_transaction_descriptor(1);
        let mut mock = MockNetworkWriter::new(32767);
        let mut writer = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let rpc = SqlRpc::new_batch_command(
            RpcType::ProcId(RpcProcs::Execute),
            Some(row.to_vec()),
            None,
            &collation,
            &context,
            first,
        );
        block_on(rpc.serialize_batch_command(&mut writer, first)).unwrap();
        let length = usize::try_from(writer.position()).unwrap();
        drop(writer);
        assert!(mock.data.is_empty());
        length
    }

    /// Non-zero so a swapped or misplaced collation field changes the wire.
    fn db_collation() -> SqlCollation {
        SqlCollation {
            info: 0x00D0_0409,
            lcid_language_id: 0x0409,
            col_flags: 0x0D,
            sort_id: 52,
        }
    }

    /// Differs from [`db_collation`]; NVARCHAR TYPE_INFO must never carry it.
    fn value_collation() -> SqlCollation {
        SqlCollation {
            info: 0x0010_0411,
            lcid_language_id: 0x0411,
            col_flags: 0x01,
            sort_id: 0,
        }
    }

    fn param(value: SqlType) -> RpcParameter {
        RpcParameter::new(None, StatusFlags::NONE, value)
    }

    fn utf16(text: &str) -> SqlString {
        SqlString::from_utf8_string(text.to_string())
    }

    fn shift_row(shift: usize) -> Vec<RpcParameter> {
        vec![
            param(SqlType::Int(Some(42))),
            param(SqlType::VarBinary(Some(vec![0xab; shift]), 8000)),
        ]
    }

    fn encrypted_param() -> RpcParameter {
        use super::super::parameters::rpc_parameters::RpcEncryptionMetadata;
        let mut parameter = param(SqlType::Int(Some(7)));
        parameter.set_encrypted(
            Some(vec![1, 2, 3]),
            RpcEncryptionMetadata {
                cipher_algorithm_id: 2,
                encryption_type: 1,
                database_id: 5,
                cek_id: 6,
                cek_version: 1,
                cek_md_version: [9; 8],
                normalization_rule_version: 1,
            },
            false,
        );
        parameter
    }

    /// One command of the matrix and whether every parameter is fast-path
    /// eligible. The handle parameter leads, as in `execute_sp_execute_batch`.
    fn matrix_row(index: usize) -> (Vec<RpcParameter>, bool) {
        use crate::datatypes::sql_string::EncodingType;
        let by_ref = |value| RpcParameter::new(None, StatusFlags::BY_REF_VALUE, value);
        let text = "x".repeat(index % 9);
        let (parameters, eligible) = match index % 24 {
            0 => (
                vec![
                    param(SqlType::Int(Some(i32::MIN))),
                    param(SqlType::BigInt(Some(i64::MAX))),
                    param(SqlType::NVarchar(Some(utf16(&text)), 20)),
                ],
                true,
            ),
            1 => (
                vec![
                    param(SqlType::Int(Some(i32::MAX))),
                    param(SqlType::BigInt(Some(i64::MIN))),
                    param(SqlType::NVarchar(None, 10)),
                ],
                true,
            ),
            2 => (
                vec![
                    param(SqlType::Int(None)),
                    param(SqlType::BigInt(None)),
                    param(SqlType::NVarchar(Some(utf16("")), 0)),
                    param(SqlType::NVarchar(None, 0)),
                ],
                true,
            ),
            3 => {
                let chars = 1 + index % 5;
                (
                    vec![by_ref(SqlType::NVarchar(
                        Some(utf16(&"y".repeat(chars))),
                        u16::try_from(chars).unwrap(),
                    ))],
                    true,
                )
            }
            4 => (
                vec![
                    param(SqlType::Int(Some(i32::try_from(index).unwrap()))),
                    param(SqlType::NVarchar(
                        Some(SqlString::new(
                            vec![0x41, 0, 0x42, 0],
                            EncodingType::LcidBased(value_collation()),
                        )),
                        2,
                    )),
                ],
                true,
            ),
            5 => (
                vec![param(SqlType::NVarchar(
                    Some(SqlString::new(vec![0x43, 0], EncodingType::DelayedSet)),
                    1,
                ))],
                true,
            ),
            6 => (
                vec![param(SqlType::NVarchar(Some(utf16("你好𐐀")), 4))],
                true,
            ),
            7 => (
                vec![param(SqlType::NVarchar(
                    Some(SqlString::new(
                        text.clone().into_bytes(),
                        EncodingType::Utf8,
                    )),
                    10,
                ))],
                false,
            ),
            8 => (
                vec![param(SqlType::NVarchar(Some(utf16(&text)), 4001))],
                false,
            ),
            9 => (
                vec![
                    param(SqlType::Int(Some(1))),
                    param(SqlType::Varchar(Some(utf16("abc")), 10)),
                ],
                false,
            ),
            10 if index % 120 == 10 => (vec![encrypted_param()], false),
            10 => (vec![param(SqlType::NVarcharMax(Some(utf16(&text))))], false),
            _ => (
                vec![
                    by_ref(SqlType::Int(Some(i32::try_from(index).unwrap()))),
                    param(SqlType::BigInt(Some(i64::try_from(index).unwrap() * 7))),
                    param(SqlType::NVarchar(Some(utf16(&"z".repeat(index % 13))), 30)),
                ],
                true,
            ),
        };
        let mut row = vec![param(SqlType::Int(Some(42)))];
        row.extend(parameters);
        (row, eligible)
    }

    #[test]
    fn batch_fast_path_matches_async_wire_across_packet_boundaries() {
        for packet_size in [512u32, 4096, 16192] {
            let max_payload = usize::try_from(packet_size).unwrap() - 8;
            let row_count = 2 * max_payload / 40;
            let (mut rows, mut eligible): (Vec<_>, Vec<_>) =
                (1..=row_count).map(matrix_row).unzip();
            // Full-width NVARCHAR(4000) commands: only a 16192-byte packet can
            // take them inline.
            for at in [row_count / 3, 2 * row_count / 3] {
                rows[at] = vec![
                    param(SqlType::Int(Some(42))),
                    param(SqlType::NVarchar(Some(utf16(&"w".repeat(4000))), 4000)),
                ];
                eligible[at] = true;
            }
            let lengths: Vec<_> = rows.iter().map(|row| command_len(row, false)).collect();
            let mut boundary_ends = std::collections::BTreeSet::new();
            let (mut fast_hits, mut commands) = (0usize, 0usize);
            for shift in 0..=100 {
                let mut batch = vec![shift_row(shift)];
                batch.extend(rows.iter().cloned());
                let slow = run_batch(packet_size, &db_collation(), &batch, false);
                let fast = run_batch(packet_size, &db_collation(), &batch, true);
                assert_eq!(slow.result, "Ok(())");
                assert_eq!(fast.result, slow.result);
                assert_eq!(fast.wire, slow.wire, "packet {packet_size}, shift {shift}");
                assert!(slow.commands.iter().all(|&(_, fast)| !fast));
                assert!(!fast.commands[0].1, "the first command owns ALL_HEADERS");
                for (index, &(position, took_fast_path)) in fast.commands.iter().enumerate().skip(1)
                {
                    assert_eq!(position, slow.commands[index].0);
                    let end = position + lengths[index - 1];
                    let row_eligible = eligible[index - 1];
                    assert_eq!(
                        took_fast_path,
                        row_eligible && end < max_payload,
                        "packet {packet_size}, shift {shift}, command {index} ends at {end}"
                    );
                    if row_eligible && (max_payload - 2..=max_payload + 1).contains(&end) {
                        boundary_ends.insert(end + 2 - max_payload);
                    }
                    fast_hits += usize::from(took_fast_path);
                    commands += 1;
                }
            }
            assert_eq!(
                boundary_ends.into_iter().collect::<Vec<_>>(),
                [0, 1, 2, 3],
                "commands must end at max-2, max-1 (fast) and max, max+1 (fallback)"
            );
            assert!(
                fast_hits * 10 > commands * 6,
                "packet {packet_size}: only {fast_hits} of {commands} commands took the fast path"
            );
        }
    }

    /// The last command ends at max-2 .. max+1, so `finalize` decides between
    /// one final packet and a header-only EOM after an exact fill.
    #[test]
    fn batch_fast_path_matches_trailing_eom_at_exact_fill() {
        let final_row = |chars: usize| {
            let mut row = vec![param(SqlType::Int(Some(42)))];
            for part in 0..3 {
                let part_chars = chars.saturating_sub(part * 4000).min(4000);
                row.push(param(SqlType::NVarchar(
                    Some(utf16(&"q".repeat(part_chars))),
                    4000,
                )));
            }
            row
        };
        for packet_size in [512u32, 4096, 16192] {
            let max_payload = usize::try_from(packet_size).unwrap() - 8;
            let base = command_len(&shift_row(0), true);
            let empty = command_len(&final_row(0), false);
            for offset in -2isize..=1 {
                let target = (max_payload as isize + offset) as usize - base - empty;
                let (chars, shift) = (target / 2, target % 2);
                let batch = vec![shift_row(shift), final_row(chars)];
                assert_eq!(command_len(&batch[1], false), empty + 2 * chars);
                let slow = run_batch(packet_size, &db_collation(), &batch, false);
                let fast = run_batch(packet_size, &db_collation(), &batch, true);
                assert_eq!(fast.commands[1].0, base + shift);
                assert_eq!(slow.result, "Ok(())");
                assert_eq!(fast.result, slow.result);
                assert_eq!(
                    fast.wire, slow.wire,
                    "packet {packet_size}, offset {offset}"
                );
                assert_eq!(
                    fast.commands[1].1,
                    offset < 0,
                    "packet {packet_size} {offset}"
                );
            }
        }
    }

    /// Values the async serializer rejects mid-parameter stay on that path,
    /// so the error and the bytes already sent are unchanged.
    #[test]
    fn batch_fast_path_leaves_mid_parameter_errors_to_the_async_path() {
        use crate::datatypes::sql_string::EncodingType;
        let odd = SqlType::NVarchar(
            Some(SqlString::new(
                vec![0x41, 0, 0x42],
                EncodingType::LcidBased(value_collation()),
            )),
            10,
        );
        let too_long = SqlType::NVarchar(Some(utf16("abcd")), 3);
        for packet_size in [512u32, 4096, 16192] {
            for invalid in [&odd, &too_long] {
                for at in [1, 7, 150, 400] {
                    let mut batch = vec![shift_row(at % 5)];
                    batch.extend((1..at).map(|index| matrix_row(index).0));
                    batch.push(vec![param(SqlType::Int(Some(42))), param(invalid.clone())]);
                    batch.push(matrix_row(at + 1).0);
                    let slow = run_batch(packet_size, &db_collation(), &batch, false);
                    let fast = run_batch(packet_size, &db_collation(), &batch, true);
                    assert!(
                        slow.result.starts_with("Err(UsageError("),
                        "{}",
                        slow.result
                    );
                    assert_eq!(fast.result, slow.result);
                    assert_eq!(fast.wire, slow.wire);
                    assert_eq!(fast.commands.len(), at + 1);
                    assert!(!fast.commands[at].1);
                }
            }
        }
    }

    #[test]
    fn batch_fast_path_eligibility_and_lengths() {
        use crate::datatypes::sql_string::EncodingType;
        let inline = |value| param(value).positional_inline_len();
        assert_eq!(inline(SqlType::Int(Some(1))), Some(9));
        assert_eq!(inline(SqlType::Int(None)), Some(5));
        assert_eq!(inline(SqlType::BigInt(Some(1))), Some(13));
        assert_eq!(inline(SqlType::BigInt(None)), Some(5));
        assert_eq!(inline(SqlType::NVarchar(None, 4000)), Some(12));
        assert_eq!(inline(SqlType::NVarchar(Some(utf16("ab")), 2)), Some(16));
        assert_eq!(
            inline(SqlType::NVarchar(Some(utf16(&"a".repeat(4000))), 4000)),
            Some(8012)
        );
        for ineligible in [
            SqlType::NVarchar(None, 4001),
            SqlType::NVarchar(Some(utf16("abc")), 2),
            SqlType::NVarchar(Some(SqlString::new(b"ab".to_vec(), EncodingType::Utf8)), 10),
            SqlType::NVarchar(Some(SqlString::new(vec![1], EncodingType::DelayedSet)), 10),
            SqlType::NVarcharMax(None),
            SqlType::Varchar(None, 10),
            SqlType::SmallInt(Some(1)),
            SqlType::Bit(Some(true)),
            SqlType::VarBinary(Some(vec![1]), 10),
        ] {
            assert_eq!(inline(ineligible.clone()), None, "{ineligible:?}");
        }
        assert_eq!(encrypted_param().positional_inline_len(), None);
        let streamed = RpcParameter::data_at_exec(
            None,
            StatusFlags::NONE,
            crate::message::parameters::rpc_parameters::StreamedSqlType::NVarcharMax,
        );
        assert_eq!(streamed.positional_inline_len(), None);
    }

    #[test]
    fn prepared_insert_batch_fast_path_matches_async_wire() {
        for packet_size in [512u32, 4096, 8000, 16192] {
            let rows: Vec<_> = (0..2000).map(insert_parameters).collect();
            let fast = run_batch(packet_size, &SqlCollation::default(), &rows, true);
            assert_eq!(fast.result, "Ok(())");
            assert_eq!(fast.wire, serialize_insert_batch(packet_size));
            let hits = fast.commands.iter().filter(|&&(_, fast)| fast).count();
            // Only commands at a packet boundary fall back (an exact fill can
            // push the next one back too); the first owns ALL_HEADERS.
            let packets = fast
                .wire
                .len()
                .div_ceil(usize::try_from(packet_size).unwrap());
            assert!(
                hits + 2 * packets + 1 >= 2000,
                "{packet_size}: {hits} fast commands"
            );

            // NVARCHAR(15) TYPE_INFO built by hand: the database collation,
            // info little-endian then sort_id.
            let collated = run_batch(packet_size, &db_collation(), &rows, true);
            let slow = run_batch(packet_size, &db_collation(), &rows, false);
            assert_eq!(collated.wire, slow.wire);
            let mut payload = Vec::new();
            for packet in collated.wire.chunks(usize::try_from(packet_size).unwrap()) {
                payload.extend_from_slice(&packet[8..]);
            }
            let type_info = [0xe7, 30, 0, 0x09, 0x04, 0xd0, 0x00, 52];
            assert_eq!(
                payload
                    .windows(type_info.len())
                    .filter(|window| *window == type_info)
                    .count(),
                2000
            );
        }
    }
}
