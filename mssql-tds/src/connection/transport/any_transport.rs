// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::future::Future;
use std::time::Duration;

#[cfg(any(test, feature = "test-util", fuzzing))]
use futures::future::Either;

use crate::connection::transport::network_transport::NetworkTransport;
use crate::connection::transport::tds_transport::TdsTransport;
use crate::core::{CancelHandle, TdsResult};
use crate::datatypes::column_values::ColumnValues;
use crate::datatypes::row_writer::RowWriter;
use crate::io::reader_writer::NetworkWriter;
use crate::io::token_stream::{
    ColumnPolicy, ParserContext, PlpPauseState, RowHeader, RowPauseState, RowReadResult,
    TdsTokenStreamReader,
};
use crate::token::tokens::Tokens;

type CompleteBufferedPlp = Option<Option<(usize, Option<u64>, usize)>>;

/// Concrete transport representation held by [`crate::connection::tds_client::TdsClient`].
///
/// Production clients always use the network arm. The dynamic arm preserves the
/// custom transports used by unit tests, downstream test helpers, and fuzzing
/// without putting production row decoding behind a trait object.
#[derive(Debug)]
pub(crate) enum AnyTransport {
    Network(NetworkTransport),
    #[cfg(any(test, feature = "test-util", fuzzing))]
    Dynamic(Box<dyn TdsTransport>),
}

impl AnyTransport {
    pub(crate) fn network(transport: NetworkTransport) -> Self {
        Self::Network(transport)
    }

    #[cfg(any(test, feature = "test-util", fuzzing))]
    pub(crate) fn dynamic<T>(transport: T) -> Self
    where
        T: TdsTransport + 'static,
    {
        Self::Dynamic(Box::new(transport))
    }

    pub(crate) async fn receive_token(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Tokens> {
        match self {
            Self::Network(transport) => {
                transport
                    .receive_token(context, remaining_request_timeout, cancel_handle)
                    .await
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => {
                transport
                    .receive_token(context, remaining_request_timeout, cancel_handle)
                    .await
            }
        }
    }

    #[cfg(not(any(test, feature = "test-util", fuzzing)))]
    pub(crate) fn receive_row_into<'a, W>(
        &'a mut self,
        context: &'a ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&'a CancelHandle>,
        plan: ColumnPolicy,
        writer: &'a mut W,
    ) -> impl Future<Output = TdsResult<RowReadResult>> + Send + 'a
    where
        W: RowWriter + Send + ?Sized + 'a,
    {
        match self {
            Self::Network(transport) => transport.receive_row_into(
                context,
                remaining_request_timeout,
                cancel_handle,
                plan,
                writer,
            ),
        }
    }

    #[cfg(any(test, feature = "test-util", fuzzing))]
    pub(crate) fn receive_row_into<'a, W>(
        &'a mut self,
        context: &'a ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&'a CancelHandle>,
        plan: ColumnPolicy,
        writer: &'a mut W,
    ) -> impl Future<Output = TdsResult<RowReadResult>> + Send + 'a
    where
        W: RowWriter + Send + ?Sized + 'a,
    {
        match self {
            Self::Network(transport) => Either::Left(transport.receive_row_into(
                context,
                remaining_request_timeout,
                cancel_handle,
                plan,
                writer,
            )),
            Self::Dynamic(transport) => {
                let mut writer = DynamicRowWriter(writer);
                Either::Right(async move {
                    transport
                        .receive_row_into(
                            context,
                            remaining_request_timeout,
                            cancel_handle,
                            plan,
                            &mut writer,
                        )
                        .await
                })
            }
        }
    }

    pub(crate) async fn receive_row_header(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<RowHeader> {
        match self {
            Self::Network(transport) => {
                transport
                    .receive_row_header(context, remaining_request_timeout, cancel_handle)
                    .await
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => {
                transport
                    .receive_row_header(context, remaining_request_timeout, cancel_handle)
                    .await
            }
        }
    }

    /// Attempts to position a row using only bytes already buffered by the active transport.
    pub(crate) fn try_receive_row_header(
        &mut self,
        context: &ParserContext,
    ) -> TdsResult<Option<RowPauseState>> {
        match self {
            Self::Network(transport) => {
                TdsTokenStreamReader::try_receive_row_header(transport, context)
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.try_receive_row_header(context),
        }
    }

    /// Attempts to decode one buffered column while retaining a `sql_variant` base type.
    pub(crate) fn try_read_buffered_column_with_base(
        &mut self,
        pause_state: &RowPauseState,
        target: usize,
    ) -> TdsResult<
        Option<(
            ColumnValues,
            Option<crate::datatypes::sqldatatypes::TdsDataType>,
        )>,
    > {
        match self {
            Self::Network(transport) => {
                transport.try_read_buffered_column_with_base(pause_state, target)
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport
                .try_read_buffered_column(pause_state, target)
                .map(|value| value.map(|value| (value, None))),
        }
    }

    pub(crate) fn try_begin_buffered_plp(
        &mut self,
        pause_state: &RowPauseState,
        target: usize,
    ) -> TdsResult<Option<Option<crate::datatypes::decoder::PlpColumnStream>>> {
        match self {
            Self::Network(transport) => transport.try_begin_buffered_plp(pause_state, target),
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(_) => Ok(None),
        }
    }

    pub(crate) fn try_read_complete_buffered_plp_column(
        &mut self,
        pause_state: &RowPauseState,
        target: usize,
        out: &mut [u8],
    ) -> TdsResult<CompleteBufferedPlp> {
        match self {
            Self::Network(transport) => {
                transport.try_read_complete_buffered_plp_column(pause_state, target, out)
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(_) => Ok(None),
        }
    }

    pub(crate) fn try_read_complete_buffered_plp(
        &mut self,
        plp_state: &mut PlpPauseState,
        out: &mut [u8],
    ) -> TdsResult<Option<usize>> {
        match self {
            Self::Network(transport) => transport.try_read_complete_buffered_plp(plp_state, out),
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(_) => Ok(None),
        }
    }

    /// Writes as much of the current buffered row as the active transport can complete.
    pub(crate) fn try_read_buffered_row_into<W: RowWriter + ?Sized>(
        &mut self,
        pause_state: &mut RowPauseState,
        writer: &mut W,
    ) -> TdsResult<bool> {
        self.try_read_buffered_row_prefix_into(pause_state, usize::MAX, writer)
    }

    pub(crate) fn try_read_buffered_row_prefix_into<W: RowWriter + ?Sized>(
        &mut self,
        pause_state: &mut RowPauseState,
        end_column: usize,
        writer: &mut W,
    ) -> TdsResult<bool> {
        match self {
            Self::Network(transport) => {
                transport.try_read_buffered_row_prefix_into(pause_state, end_column, writer)
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => {
                let Some((row, complete)) = transport.try_read_buffered_test_row(pause_state)?
                else {
                    return Ok(false);
                };
                for value in row
                    .into_iter()
                    .take(end_column.saturating_sub(pause_state.next_column_index))
                {
                    writer.write_i32(pause_state.next_column_index, value);
                    pause_state.next_column_index += 1;
                }
                Ok(complete || pause_state.next_column_index >= end_column)
            }
        }
    }

    pub(crate) async fn resume_row_into<W>(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut W,
    ) -> TdsResult<RowReadResult>
    where
        W: RowWriter + Send + ?Sized,
    {
        match self {
            Self::Network(transport) => {
                transport
                    .resume_row_into(
                        pause_state,
                        remaining_request_timeout,
                        cancel_handle,
                        plan,
                        writer,
                    )
                    .await
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => {
                let mut writer = DynamicRowWriter(writer);
                transport
                    .resume_row_into(
                        pause_state,
                        remaining_request_timeout,
                        cancel_handle,
                        plan,
                        &mut writer,
                    )
                    .await
            }
        }
    }

    pub(crate) async fn read_active_plp_bytes(
        &mut self,
        plp_state: &mut PlpPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        out: &mut [u8],
    ) -> TdsResult<usize> {
        match self {
            Self::Network(transport) => {
                transport
                    .read_active_plp_bytes(plp_state, remaining_request_timeout, cancel_handle, out)
                    .await
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => {
                transport
                    .read_active_plp_bytes(plp_state, remaining_request_timeout, cancel_handle, out)
                    .await
            }
        }
    }

    pub(crate) fn as_writer(&mut self) -> &mut dyn NetworkWriter {
        match self {
            Self::Network(transport) => TdsTransport::as_writer(transport),
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.as_writer(),
        }
    }

    pub(crate) fn as_writer_ref(&self) -> &dyn NetworkWriter {
        match self {
            Self::Network(transport) => TdsTransport::as_writer_ref(transport),
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.as_writer_ref(),
        }
    }

    pub(crate) fn reset_reader(&mut self) {
        match self {
            Self::Network(transport) => TdsTransport::reset_reader(transport),
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.reset_reader(),
        }
    }

    pub(crate) async fn close_transport(&mut self) -> TdsResult<()> {
        match self {
            Self::Network(transport) => TdsTransport::close_transport(transport).await,
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.close_transport().await,
        }
    }

    pub(crate) async fn send_attention_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> TdsResult<bool> {
        match self {
            Self::Network(transport) => {
                TdsTransport::send_attention_with_timeout(transport, timeout).await
            }
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.send_attention_with_timeout(timeout).await,
        }
    }

    pub(crate) fn is_connection_dead(&self) -> bool {
        match self {
            Self::Network(transport) => TdsTransport::is_connection_dead(transport),
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.is_connection_dead(),
        }
    }

    pub(crate) fn connection_known_dead(&self) -> bool {
        match self {
            Self::Network(transport) => TdsTransport::connection_known_dead(transport),
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.connection_known_dead(),
        }
    }

    /// Flags the connection as known-dead without touching the socket, so a
    /// later pool checkout discards it. See
    /// [`TdsTransport::mark_known_dead`].
    pub(crate) fn mark_known_dead(&mut self) {
        match self {
            Self::Network(transport) => TdsTransport::mark_known_dead(transport),
            #[cfg(any(test, feature = "test-util", fuzzing))]
            Self::Dynamic(transport) => transport.mark_known_dead(),
        }
    }
}

#[cfg(any(test, feature = "test-util", fuzzing))]
// Sized adapter for the legacy dynamic arm when its caller supplied a `?Sized` writer.
struct DynamicRowWriter<'a, W: RowWriter + Send + ?Sized>(&'a mut W);

#[cfg(any(test, feature = "test-util", fuzzing))]
macro_rules! forward_row_values {
    ($($method:ident: $value:ty),* $(,)?) => {
        $(
            fn $method(&mut self, col: usize, value: $value) {
                self.0.$method(col, value);
            }
        )*
    };
}

// Lets test writers in other modules reuse the boilerplate. Any user must be a
// tuple struct whose `.0` is the inner `RowWriter`.
#[cfg(any(test, feature = "test-util", fuzzing))]
#[allow(unused_imports)]
pub(crate) use forward_row_values;

#[cfg(any(test, feature = "test-util", fuzzing))]
impl<W: RowWriter + Send + ?Sized> RowWriter for DynamicRowWriter<'_, W> {
    fn write_null(&mut self, col: usize) {
        self.0.write_null(col);
    }

    forward_row_values!(
        write_bool: bool,
        write_u8: u8,
        write_i16: i16,
        write_i32: i32,
        write_i64: i64,
        write_f32: f32,
        write_f64: f64,
        write_bytes: std::borrow::Cow<'_, [u8]>,
        write_decimal: crate::datatypes::decoder::DecimalParts,
        write_numeric: crate::datatypes::decoder::DecimalParts,
        write_date: crate::datatypes::column_values::SqlDate,
        write_time: crate::datatypes::column_values::SqlTime,
        write_datetime: crate::datatypes::column_values::SqlDateTime,
        write_smalldatetime: crate::datatypes::column_values::SqlSmallDateTime,
        write_datetime2: crate::datatypes::column_values::SqlDateTime2,
        write_datetimeoffset: crate::datatypes::column_values::SqlDateTimeOffset,
        write_money: crate::datatypes::column_values::SqlMoney,
        write_smallmoney: crate::datatypes::column_values::SqlSmallMoney,
        write_uuid: uuid::Uuid,
        write_xml: crate::datatypes::column_values::SqlXml,
        write_json: crate::datatypes::sql_json::SqlJson,
        write_vector: crate::datatypes::sql_vector::SqlVector,
    );

    fn end_row(&mut self) {
        self.0.end_row();
    }

    // Carries two value arguments, so it does not fit `forward_row_values!`.
    fn write_string(
        &mut self,
        col: usize,
        bytes: std::borrow::Cow<'_, [u8]>,
        encoding_type: crate::datatypes::sql_string::EncodingType,
    ) {
        self.0.write_string(col, bytes, encoding_type);
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use super::*;
    use crate::datatypes::column_values::ColumnValues;
    use crate::datatypes::row_writer::DefaultRowWriter;
    use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfo};
    use crate::message::messages::PacketType;
    use crate::query::metadata::ColumnMetadata;
    use crate::test_packet_support::{TestPacketBuilder, create_network_transport_with_data};
    use crate::token::tokens::{ColMetadataToken, TokenType};

    /// Forwards every write to an inner `DefaultRowWriter` and records whether
    /// the string and binary values arrived still borrowed.
    struct RefSpy(DefaultRowWriter, Vec<String>);

    impl RowWriter for RefSpy {
        fn write_null(&mut self, col: usize) {
            self.0.write_null(col);
        }

        forward_row_values!(
            write_bool: bool,
            write_u8: u8,
            write_i16: i16,
            write_i32: i32,
            write_i64: i64,
            write_f32: f32,
            write_f64: f64,
            write_decimal: crate::datatypes::decoder::DecimalParts,
            write_numeric: crate::datatypes::decoder::DecimalParts,
            write_date: crate::datatypes::column_values::SqlDate,
            write_time: crate::datatypes::column_values::SqlTime,
            write_datetime: crate::datatypes::column_values::SqlDateTime,
            write_smalldatetime: crate::datatypes::column_values::SqlSmallDateTime,
            write_datetime2: crate::datatypes::column_values::SqlDateTime2,
            write_datetimeoffset: crate::datatypes::column_values::SqlDateTimeOffset,
            write_money: crate::datatypes::column_values::SqlMoney,
            write_smallmoney: crate::datatypes::column_values::SqlSmallMoney,
            write_uuid: uuid::Uuid,
            write_xml: crate::datatypes::column_values::SqlXml,
            write_json: crate::datatypes::sql_json::SqlJson,
            write_vector: crate::datatypes::sql_vector::SqlVector,
        );

        fn write_string(
            &mut self,
            col: usize,
            bytes: Cow<'_, [u8]>,
            encoding_type: crate::datatypes::sql_string::EncodingType,
        ) {
            self.1.push(format!(
                "string borrowed={}",
                matches!(&bytes, Cow::Borrowed(_))
            ));
            self.0.write_string(col, bytes, encoding_type);
        }

        fn write_bytes(&mut self, col: usize, bytes: Cow<'_, [u8]>) {
            self.1.push(format!(
                "bytes borrowed={}",
                matches!(&bytes, Cow::Borrowed(_))
            ));
            self.0.write_bytes(col, bytes);
        }

        fn end_row(&mut self) {
            self.0.end_row();
        }
    }

    #[test]
    fn dynamic_row_writer_forwards_borrows_intact() {
        use crate::datatypes::sql_string::{EncodingType, SqlString};

        let mut spy = RefSpy(DefaultRowWriter::new(2), Vec::new());

        {
            let mut wrapper = DynamicRowWriter(&mut spy);
            wrapper.write_string(0, Cow::Borrowed(b"h\0i\0"), EncodingType::Utf16);
            wrapper.write_bytes(1, Cow::Borrowed(&[1, 2, 3]));
        }

        // A wrapper that copied on the way through would deliver `Cow::Owned`,
        // silently losing the zero-copy path for every writer behind it.
        assert_eq!(spy.1, vec!["string borrowed=true", "bytes borrowed=true"]);
        assert_eq!(
            spy.0.take_row(),
            vec![
                ColumnValues::String(SqlString::new(b"h\0i\0".to_vec(), EncodingType::Utf16)),
                ColumnValues::Bytes(vec![1, 2, 3]),
            ]
        );
    }

    fn int_row_packet(value: i32) -> Vec<u8> {
        TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(TokenType::Row as u8)
            .append_byte(4)
            .append_i32(value)
            .build()
    }

    fn int_row_context() -> ParserContext {
        let column = ColumnMetadata {
            user_type: 0,
            flags: 1,
            data_type: TdsDataType::IntN,
            type_info: TypeInfo::var_len(TdsDataType::IntN, 4).unwrap(),
            column_name: "value".to_string(),
            multi_part_name: None,
            crypto_metadata: None,
        };
        ParserContext::ColumnMetadata(
            Arc::new(ColMetadataToken {
                column_count: 1,
                columns: vec![column],
                ..Default::default()
            }),
            None,
        )
    }

    #[tokio::test]
    async fn network_and_dynamic_arms_decode_the_same_row() {
        let packet = int_row_packet(42);
        let transports = [
            (
                "network",
                AnyTransport::network(create_network_transport_with_data(&packet)),
            ),
            (
                "dynamic",
                AnyTransport::dynamic(create_network_transport_with_data(&packet)),
            ),
        ];

        for (name, mut transport) in transports {
            let mut writer = DefaultRowWriter::new(1);
            let result = transport
                .receive_row_into(
                    &int_row_context(),
                    None,
                    None,
                    ColumnPolicy::DecodeAll,
                    &mut writer,
                )
                .await
                .unwrap_or_else(|error| panic!("{name} arm failed: {error}"));

            assert!(matches!(result, RowReadResult::RowWritten), "{name} arm");
            assert_eq!(writer.take_row(), vec![ColumnValues::Int(42)], "{name} arm");
        }
    }

    #[tokio::test]
    async fn dynamic_arm_accepts_an_erased_writer() {
        let packet = int_row_packet(42);
        let mut transport = AnyTransport::dynamic(create_network_transport_with_data(&packet));
        let mut writer = DefaultRowWriter::new(1);
        let erased: &mut (dyn RowWriter + Send) = &mut writer;

        let result = transport
            .receive_row_into(
                &int_row_context(),
                None,
                None,
                ColumnPolicy::DecodeAll,
                erased,
            )
            .await
            .unwrap();

        assert!(matches!(result, RowReadResult::RowWritten));
        assert_eq!(writer.take_row(), vec![ColumnValues::Int(42)]);
    }
}
