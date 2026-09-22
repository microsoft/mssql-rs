// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Re-export types that are needed for fuzzing
// These are pub(crate) in the main crate but exposed here for fuzz targets
pub use crate::connection::tds_client::TdsClient;
pub use crate::connection_provider::tds_connection_provider::TdsConnectionProvider;
pub use crate::io::packet_reader::TdsPacketReader;
pub use crate::io::token_stream::{
    ColumnPolicy, GenericTokenParserRegistry, ParserContext, PlpPauseState, RowHeader,
    RowPauseState, RowReadResult, TdsTokenStreamReader, TokenParserRegistry, TokenStreamReader,
};
pub use crate::token::parsers::common::TokenParser;
pub use crate::token::parsers::{
    FuzzDoneTokenParser as DoneTokenParser, FuzzEnvChangeTokenParser as EnvChangeTokenParser,
};
pub use crate::token::tokens::{SqlCollation, Tokens};

// Re-export the DER/PEM framing fuzz entry point.
pub use crate::security::crypto::fuzz_der_framing;

// Re-export bulk copy internals for fuzz targets
use crate::connection::bulk_copy::BulkCopyOptions;
use crate::datatypes::bulk_copy_metadata::BulkCopyColumnMetadata;

/// Wrapper around the `pub(crate)` `build_insert_bulk_command` for fuzz targets.
pub fn build_insert_bulk_command(
    table_name: &str,
    column_metadata: &[BulkCopyColumnMetadata],
    options: &BulkCopyOptions,
) -> TdsResult<String> {
    crate::message::bulk_load::build_insert_bulk_command(table_name, column_metadata, options)
}

// Import types we need internally
use crate::connection::transport::any_transport::AnyTransport;
use crate::connection::transport::tds_transport::TdsTransport;
use crate::core::NegotiatedEncryptionSetting;
use crate::datatypes::row_writer::RowWriter;
use crate::handler::handler_factory::{NegotiatedSettings, SessionSettings};
use crate::io::reader_writer::{NetworkReader, NetworkReaderWriter, NetworkWriter};

use crate::core::{CancelHandle, TdsResult};
use async_trait::async_trait;
use std::io::{Error, ErrorKind};
use std::time::Duration;

const MAX_ALLOC: usize = 1024 * 1024; // 1MB

/// Cursor-based reader over a byte slice for fuzz targets.
///
/// Implements `TdsPacketReader` with best-of-both overflow protections:
/// - `checked_add` in `read_bytes`/`skip_bytes` (prevents position overflow)
/// - `checked_mul` in `read_varchar_*`/`read_unicode` (prevents length overflow)
/// - Proper `FromUtf16Error` propagation
pub struct FuzzReader {
    data: Vec<u8>,
    position: usize,
}

impl FuzzReader {
    pub fn new(data: &[u8]) -> Self {
        Self {
            data: data.to_vec(),
            position: 0,
        }
    }

    fn try_read_array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.position.checked_add(N)?;
        let bytes = self.data.get(self.position..end)?.try_into().ok()?;
        self.position = end;
        Some(bytes)
    }
}

impl TdsPacketReader for FuzzReader {
    fn try_read_byte(&mut self) -> Option<u8> {
        self.try_read_array().map(|[value]| value)
    }

    fn try_read_int16(&mut self) -> Option<i16> {
        self.try_read_array().map(i16::from_le_bytes)
    }

    fn try_read_uint16(&mut self) -> Option<u16> {
        self.try_read_array().map(u16::from_le_bytes)
    }

    fn try_read_uint24(&mut self) -> Option<u32> {
        let [b0, b1, b2] = self.try_read_array()?;
        Some(u32::from_le_bytes([b0, b1, b2, 0]))
    }

    fn try_read_int32(&mut self) -> Option<i32> {
        self.try_read_array().map(i32::from_le_bytes)
    }

    fn try_read_uint32(&mut self) -> Option<u32> {
        self.try_read_array().map(u32::from_le_bytes)
    }

    fn try_read_uint40(&mut self) -> Option<u64> {
        let [b0, b1, b2, b3, b4] = self.try_read_array()?;
        Some(u64::from_le_bytes([b0, b1, b2, b3, b4, 0, 0, 0]))
    }

    fn try_read_int64(&mut self) -> Option<i64> {
        self.try_read_array().map(i64::from_le_bytes)
    }

    fn try_read_float32(&mut self) -> Option<f32> {
        self.try_read_array().map(f32::from_le_bytes)
    }

    fn try_read_float64(&mut self) -> Option<f64> {
        self.try_read_array().map(f64::from_le_bytes)
    }

    // Without this the corpus never reaches the decoder's borrowed arm, so the
    // length arithmetic behind it would go unfuzzed. Position moves before the
    // borrow is taken, matching `BufferState::try_read_slice`.
    fn try_read_slice(&mut self, length: usize) -> Option<&[u8]> {
        let end = self.position.checked_add(length)?;
        if end > self.data.len() {
            return None;
        }
        let start = self.position;
        self.position = end;
        Some(&self.data[start..end])
    }

    async fn read_byte(&mut self) -> TdsResult<u8> {
        if self.position >= self.data.len() {
            return Err(mssql_tds_error_eof());
        }
        let byte = self.data[self.position];
        self.position += 1;
        Ok(byte)
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        let mut buf = [0u8; 2];
        self.read_bytes(&mut buf).await?;
        Ok(i16::from_be_bytes(buf))
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf).await?;
        Ok(i32::from_be_bytes(buf))
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        let mut buf = [0u8; 8];
        self.read_bytes(&mut buf[..5]).await?;
        Ok(u64::from_le_bytes(buf))
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf).await?;
        Ok(f32::from_le_bytes(buf))
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        let mut buf = [0u8; 8];
        self.read_bytes(&mut buf).await?;
        Ok(f64::from_le_bytes(buf))
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        let mut buf = [0u8; 2];
        self.read_bytes(&mut buf).await?;
        Ok(i16::from_le_bytes(buf))
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        let mut buf = [0u8; 2];
        self.read_bytes(&mut buf).await?;
        Ok(u16::from_le_bytes(buf))
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf[..3]).await?;
        Ok(u32::from_le_bytes(buf))
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf).await?;
        Ok(i32::from_le_bytes(buf))
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        let mut buf = [0u8; 4];
        self.read_bytes(&mut buf).await?;
        Ok(u32::from_le_bytes(buf))
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        let mut buf = [0u8; 8];
        self.read_bytes(&mut buf).await?;
        Ok(i64::from_le_bytes(buf))
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        let mut buf = [0u8; 8];
        self.read_bytes(&mut buf).await?;
        Ok(u64::from_le_bytes(buf))
    }

    async fn read_bytes(&mut self, buf: &mut [u8]) -> TdsResult<usize> {
        let end_position = self.position.checked_add(buf.len()).ok_or_else(|| {
            crate::error::Error::Io(Error::new(
                ErrorKind::InvalidInput,
                "buffer length causes position overflow",
            ))
        })?;

        if end_position > self.data.len() {
            return Err(mssql_tds_error_eof());
        }
        buf.copy_from_slice(&self.data[self.position..end_position]);
        self.position = end_position;
        Ok(buf.len())
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let len = self.read_byte().await? as usize;
        if len > MAX_ALLOC {
            return Err(alloc_exceeded(len));
        }
        let mut buf = vec![0u8; len];
        self.read_bytes(&mut buf).await?;
        Ok(buf)
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let len = self.read_uint16().await? as usize;
        if len > MAX_ALLOC {
            return Err(alloc_exceeded(len));
        }
        let mut buf = vec![0u8; len];
        self.read_bytes(&mut buf).await?;
        Ok(buf)
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        let len = self.read_byte().await? as usize;
        let byte_len = len.checked_mul(2).ok_or_else(|| {
            crate::error::Error::Io(Error::new(
                ErrorKind::InvalidData,
                format!("String length {} * 2 overflows", len),
            ))
        })?;
        if byte_len > MAX_ALLOC {
            return Err(alloc_exceeded(byte_len));
        }
        let mut buf = vec![0u8; byte_len];
        self.read_bytes(&mut buf).await?;
        decode_utf16_le(&buf)
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        let len = self.read_uint16().await?;
        if len == 0xFFFF {
            return Ok(None);
        }
        let byte_len = (len as usize).checked_mul(2).ok_or_else(|| {
            crate::error::Error::Io(Error::new(
                ErrorKind::InvalidData,
                format!("String length {} * 2 overflows", len),
            ))
        })?;
        if byte_len > MAX_ALLOC {
            return Err(alloc_exceeded(byte_len));
        }
        let mut buf = vec![0u8; byte_len];
        self.read_bytes(&mut buf).await?;
        decode_utf16_le(&buf).map(Some)
    }

    async fn read_unicode(&mut self, char_count: usize) -> TdsResult<String> {
        let byte_len = char_count.checked_mul(2).ok_or_else(|| {
            crate::error::Error::Io(Error::new(
                ErrorKind::InvalidData,
                format!("String char_count {} * 2 overflows", char_count),
            ))
        })?;
        if byte_len > MAX_ALLOC {
            return Err(alloc_exceeded(byte_len));
        }
        let mut buf = vec![0u8; byte_len];
        self.read_bytes(&mut buf).await?;
        decode_utf16_le(&buf)
    }

    async fn read_unicode_with_byte_length(&mut self, byte_len: usize) -> TdsResult<String> {
        if byte_len > MAX_ALLOC {
            return Err(alloc_exceeded(byte_len));
        }
        let mut buf = vec![0u8; byte_len];
        self.read_bytes(&mut buf).await?;
        decode_utf16_le(&buf)
    }

    async fn skip_bytes(&mut self, count: usize) -> TdsResult<()> {
        let new_position = self.position.checked_add(count).ok_or_else(|| {
            crate::error::Error::Io(Error::new(
                ErrorKind::InvalidInput,
                "skip_count causes position overflow",
            ))
        })?;

        if new_position > self.data.len() {
            return Err(mssql_tds_error_eof());
        }
        self.position = new_position;
        Ok(())
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        Ok(())
    }

    fn reset_reader(&mut self) {
        self.position = 0;
    }
}

/// Always-EOF reader for fuzz targets that only care about context variations.
pub struct EmptyReader;

impl TdsPacketReader for EmptyReader {
    async fn read_byte(&mut self) -> TdsResult<u8> {
        Err(mssql_tds_error_eof())
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        Err(mssql_tds_error_eof())
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        Err(mssql_tds_error_eof())
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        Err(mssql_tds_error_eof())
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        Err(mssql_tds_error_eof())
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        Err(mssql_tds_error_eof())
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        Err(mssql_tds_error_eof())
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        Err(mssql_tds_error_eof())
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        Err(mssql_tds_error_eof())
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        Err(mssql_tds_error_eof())
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        Err(mssql_tds_error_eof())
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        Err(mssql_tds_error_eof())
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        Err(mssql_tds_error_eof())
    }

    async fn read_bytes(&mut self, _buf: &mut [u8]) -> TdsResult<usize> {
        Err(mssql_tds_error_eof())
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        Err(mssql_tds_error_eof())
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        Err(mssql_tds_error_eof())
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        Err(mssql_tds_error_eof())
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        Err(mssql_tds_error_eof())
    }

    async fn read_unicode(&mut self, _string_length: usize) -> TdsResult<String> {
        Err(mssql_tds_error_eof())
    }

    async fn read_unicode_with_byte_length(&mut self, _byte_length: usize) -> TdsResult<String> {
        Err(mssql_tds_error_eof())
    }

    async fn skip_bytes(&mut self, _skip_count: usize) -> TdsResult<()> {
        Err(mssql_tds_error_eof())
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        Ok(())
    }

    fn reset_reader(&mut self) {}
}

fn mssql_tds_error_eof() -> crate::error::Error {
    crate::error::Error::Io(Error::new(ErrorKind::UnexpectedEof, "EOF"))
}

fn alloc_exceeded(size: usize) -> crate::error::Error {
    crate::error::Error::Io(Error::new(
        ErrorKind::InvalidData,
        format!("Allocation size {} exceeds max {}", size, MAX_ALLOC),
    ))
}

fn decode_utf16_le(buf: &[u8]) -> TdsResult<String> {
    let chars: Vec<u16> = buf
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    String::from_utf16(&chars)
        .map_err(|e| crate::error::Error::Io(Error::new(ErrorKind::InvalidData, e)))
}

/// MockWriter captures all writes without sending over network
#[derive(Debug)]
pub struct MockWriter {
    buffer: Vec<u8>,
    packet_size: u32,
}

impl MockWriter {
    pub fn new(packet_size: u32) -> Self {
        Self {
            buffer: Vec::new(),
            packet_size,
        }
    }

    #[allow(dead_code)]
    pub fn get_buffer(&self) -> &[u8] {
        &self.buffer
    }
}

#[async_trait]
impl crate::connection::transport::network_transport::TransportSslHandler for MockWriter {
    async fn enable_ssl(&mut self) -> TdsResult<()> {
        Ok(())
    }

    async fn disable_ssl(&mut self) -> TdsResult<()> {
        Ok(())
    }
}

#[async_trait]
impl NetworkWriter for MockWriter {
    async fn send(&mut self, data: &[u8]) -> TdsResult<()> {
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    fn packet_size(&self) -> u32 {
        self.packet_size
    }

    fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
        NegotiatedEncryptionSetting::NoEncryption
    }

    // The fuzz harness never carries a reset, so there is nothing to record.
    fn note_reset_dispatched(&mut self) {}

    fn take_reset_dispatched(&mut self) -> bool {
        false
    }
}

/// Concrete packet-reader used by the fuzz harness.
///
/// `TdsPacketReader` returns `impl Future` per method, which makes it dyn-incompatible.
/// The harness only ever supplies one of two readers, so an enum recovers the runtime
/// choice that `Box<dyn TdsPacketReader>` previously provided, with static dispatch.
pub enum FuzzPacketReader {
    /// Reads from the fuzzer-supplied byte slice.
    Fuzz(FuzzReader),
    /// Always reports end-of-stream.
    Empty(EmptyReader),
}

impl FuzzPacketReader {
    /// Builds a reader over the fuzzer-supplied input.
    pub fn from_data(data: &[u8]) -> Self {
        Self::Fuzz(FuzzReader::new(data))
    }

    /// Builds a reader that always reports end-of-stream.
    pub fn empty() -> Self {
        Self::Empty(EmptyReader)
    }
}

impl TdsPacketReader for FuzzPacketReader {
    fn try_read_byte(&mut self) -> Option<u8> {
        match self {
            Self::Fuzz(r) => r.try_read_byte(),
            Self::Empty(r) => r.try_read_byte(),
        }
    }

    fn try_read_int16(&mut self) -> Option<i16> {
        match self {
            Self::Fuzz(r) => r.try_read_int16(),
            Self::Empty(r) => r.try_read_int16(),
        }
    }

    fn try_read_uint16(&mut self) -> Option<u16> {
        match self {
            Self::Fuzz(r) => r.try_read_uint16(),
            Self::Empty(r) => r.try_read_uint16(),
        }
    }

    fn try_read_uint24(&mut self) -> Option<u32> {
        match self {
            Self::Fuzz(r) => r.try_read_uint24(),
            Self::Empty(r) => r.try_read_uint24(),
        }
    }

    fn try_read_int32(&mut self) -> Option<i32> {
        match self {
            Self::Fuzz(r) => r.try_read_int32(),
            Self::Empty(r) => r.try_read_int32(),
        }
    }

    fn try_read_uint32(&mut self) -> Option<u32> {
        match self {
            Self::Fuzz(r) => r.try_read_uint32(),
            Self::Empty(r) => r.try_read_uint32(),
        }
    }

    fn try_read_uint40(&mut self) -> Option<u64> {
        match self {
            Self::Fuzz(r) => r.try_read_uint40(),
            Self::Empty(r) => r.try_read_uint40(),
        }
    }

    fn try_read_int64(&mut self) -> Option<i64> {
        match self {
            Self::Fuzz(r) => r.try_read_int64(),
            Self::Empty(r) => r.try_read_int64(),
        }
    }

    fn try_read_float32(&mut self) -> Option<f32> {
        match self {
            Self::Fuzz(r) => r.try_read_float32(),
            Self::Empty(r) => r.try_read_float32(),
        }
    }

    fn try_read_float64(&mut self) -> Option<f64> {
        match self {
            Self::Fuzz(r) => r.try_read_float64(),
            Self::Empty(r) => r.try_read_float64(),
        }
    }

    fn try_read_slice(&mut self, length: usize) -> Option<&[u8]> {
        match self {
            Self::Fuzz(r) => r.try_read_slice(length),
            Self::Empty(r) => r.try_read_slice(length),
        }
    }

    async fn read_byte(&mut self) -> TdsResult<u8> {
        match self {
            Self::Fuzz(r) => r.read_byte().await,
            Self::Empty(r) => r.read_byte().await,
        }
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        match self {
            Self::Fuzz(r) => r.read_int16_big_endian().await,
            Self::Empty(r) => r.read_int16_big_endian().await,
        }
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        match self {
            Self::Fuzz(r) => r.read_int32_big_endian().await,
            Self::Empty(r) => r.read_int32_big_endian().await,
        }
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        match self {
            Self::Fuzz(r) => r.read_uint40().await,
            Self::Empty(r) => r.read_uint40().await,
        }
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        match self {
            Self::Fuzz(r) => r.read_float32().await,
            Self::Empty(r) => r.read_float32().await,
        }
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        match self {
            Self::Fuzz(r) => r.read_float64().await,
            Self::Empty(r) => r.read_float64().await,
        }
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        match self {
            Self::Fuzz(r) => r.read_int16().await,
            Self::Empty(r) => r.read_int16().await,
        }
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        match self {
            Self::Fuzz(r) => r.read_uint16().await,
            Self::Empty(r) => r.read_uint16().await,
        }
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        match self {
            Self::Fuzz(r) => r.read_uint24().await,
            Self::Empty(r) => r.read_uint24().await,
        }
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        match self {
            Self::Fuzz(r) => r.read_int32().await,
            Self::Empty(r) => r.read_int32().await,
        }
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        match self {
            Self::Fuzz(r) => r.read_uint32().await,
            Self::Empty(r) => r.read_uint32().await,
        }
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        match self {
            Self::Fuzz(r) => r.read_int64().await,
            Self::Empty(r) => r.read_int64().await,
        }
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        match self {
            Self::Fuzz(r) => r.read_uint64().await,
            Self::Empty(r) => r.read_uint64().await,
        }
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        match self {
            Self::Fuzz(r) => r.read_bytes(buffer).await,
            Self::Empty(r) => r.read_bytes(buffer).await,
        }
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        match self {
            Self::Fuzz(r) => r.read_u8_varbyte().await,
            Self::Empty(r) => r.read_u8_varbyte().await,
        }
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        match self {
            Self::Fuzz(r) => r.read_u16_varbyte().await,
            Self::Empty(r) => r.read_u16_varbyte().await,
        }
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        match self {
            Self::Fuzz(r) => r.read_varchar_u16_length().await,
            Self::Empty(r) => r.read_varchar_u16_length().await,
        }
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        match self {
            Self::Fuzz(r) => r.read_varchar_u8_length().await,
            Self::Empty(r) => r.read_varchar_u8_length().await,
        }
    }

    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
        match self {
            Self::Fuzz(r) => r.read_unicode(string_length).await,
            Self::Empty(r) => r.read_unicode(string_length).await,
        }
    }

    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String> {
        match self {
            Self::Fuzz(r) => r.read_unicode_with_byte_length(byte_length).await,
            Self::Empty(r) => r.read_unicode_with_byte_length(byte_length).await,
        }
    }

    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()> {
        match self {
            Self::Fuzz(r) => r.skip_bytes(skip_count).await,
            Self::Empty(r) => r.skip_bytes(skip_count).await,
        }
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        match self {
            Self::Fuzz(r) => r.cancel_read_stream().await,
            Self::Empty(r) => r.cancel_read_stream().await,
        }
    }

    fn reset_reader(&mut self) {
        match self {
            Self::Fuzz(r) => r.reset_reader(),
            Self::Empty(r) => r.reset_reader(),
        }
    }
}

/// MockTransport simulates a transport layer for fuzzing
pub struct MockTransport {
    token_stream_reader: TokenStreamReader<FuzzPacketReader, GenericTokenParserRegistry>,
    mock_writer: MockWriter,
    packet_size: u32,
    encryption_setting: NegotiatedEncryptionSetting,
}

impl std::fmt::Debug for MockTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockTransport")
            .field("packet_size", &self.packet_size)
            .field("mock_writer", &self.mock_writer)
            .finish()
    }
}

impl MockTransport {
    pub fn new(packet_reader: FuzzPacketReader, packet_size: u32) -> Self {
        let parser_registry = Box::new(GenericTokenParserRegistry::default());
        let token_stream_reader = TokenStreamReader::new(packet_reader, parser_registry);

        Self {
            token_stream_reader,
            mock_writer: MockWriter::new(packet_size),
            packet_size,
            encryption_setting: NegotiatedEncryptionSetting::NoEncryption,
        }
    }
}

#[async_trait]
impl NetworkReader for MockTransport {
    fn packet_size(&self) -> u32 {
        self.packet_size
    }
}

#[async_trait]
impl crate::connection::transport::network_transport::TransportSslHandler for MockTransport {
    async fn enable_ssl(&mut self) -> TdsResult<()> {
        Ok(())
    }

    async fn disable_ssl(&mut self) -> TdsResult<()> {
        Ok(())
    }
}

#[async_trait]
impl NetworkWriter for MockTransport {
    async fn send(&mut self, data: &[u8]) -> TdsResult<()> {
        self.mock_writer.send(data).await
    }

    fn packet_size(&self) -> u32 {
        self.packet_size
    }

    fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
        self.encryption_setting
    }

    // The fuzz harness never carries a reset, so there is nothing to record.
    fn note_reset_dispatched(&mut self) {}

    fn take_reset_dispatched(&mut self) -> bool {
        false
    }
}

#[async_trait]
impl NetworkReaderWriter for MockTransport {
    fn notify_encryption_setting_change(&mut self, setting: NegotiatedEncryptionSetting) {
        self.encryption_setting = setting;
    }

    fn notify_session_setting_change(&mut self, _settings: &SessionSettings) {}

    fn as_writer(&mut self) -> &mut dyn NetworkWriter {
        self
    }
}

#[async_trait]
impl TdsTokenStreamReader for MockTransport {
    async fn receive_token(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Tokens> {
        self.token_stream_reader
            .receive_token(context, remaining_request_timeout, cancel_handle)
            .await
    }

    async fn receive_row_into(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        self.token_stream_reader
            .receive_row_into(
                context,
                remaining_request_timeout,
                cancel_handle,
                plan,
                writer,
            )
            .await
    }

    async fn receive_row_header(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<RowHeader> {
        self.token_stream_reader
            .receive_row_header(context, remaining_request_timeout, cancel_handle)
            .await
    }

    async fn resume_row_into(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        self.token_stream_reader
            .resume_row_into(
                pause_state,
                remaining_request_timeout,
                cancel_handle,
                plan,
                writer,
            )
            .await
    }

    async fn read_active_plp_bytes(
        &mut self,
        plp_state: &mut PlpPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        out: &mut [u8],
    ) -> TdsResult<usize> {
        self.token_stream_reader
            .read_active_plp_bytes(plp_state, remaining_request_timeout, cancel_handle, out)
            .await
    }
}

#[async_trait]
impl TdsTransport for MockTransport {
    fn as_writer_ref(&self) -> &dyn NetworkWriter {
        self
    }

    fn as_writer(&mut self) -> &mut dyn NetworkWriter {
        self
    }

    fn reset_reader(&mut self) {
        self.token_stream_reader.packet_reader.reset_reader();
    }

    fn packet_size(&self) -> u32 {
        self.packet_size
    }

    async fn close_transport(&mut self) -> TdsResult<()> {
        Ok(())
    }

    async fn send_attention_with_timeout(
        &mut self,
        _context: &crate::io::token_stream::ParserContext,
        _timeout: std::time::Duration,
    ) -> TdsResult<bool> {
        Ok(true)
    }

    fn is_connection_dead(&self) -> bool {
        false
    }
}

impl TdsPacketReader for MockTransport {
    fn try_read_byte(&mut self) -> Option<u8> {
        self.token_stream_reader.packet_reader.try_read_byte()
    }

    fn try_read_int16(&mut self) -> Option<i16> {
        self.token_stream_reader.packet_reader.try_read_int16()
    }

    fn try_read_uint16(&mut self) -> Option<u16> {
        self.token_stream_reader.packet_reader.try_read_uint16()
    }

    fn try_read_uint24(&mut self) -> Option<u32> {
        self.token_stream_reader.packet_reader.try_read_uint24()
    }

    fn try_read_int32(&mut self) -> Option<i32> {
        self.token_stream_reader.packet_reader.try_read_int32()
    }

    fn try_read_uint32(&mut self) -> Option<u32> {
        self.token_stream_reader.packet_reader.try_read_uint32()
    }

    fn try_read_uint40(&mut self) -> Option<u64> {
        self.token_stream_reader.packet_reader.try_read_uint40()
    }

    fn try_read_int64(&mut self) -> Option<i64> {
        self.token_stream_reader.packet_reader.try_read_int64()
    }

    fn try_read_float32(&mut self) -> Option<f32> {
        self.token_stream_reader.packet_reader.try_read_float32()
    }

    fn try_read_float64(&mut self) -> Option<f64> {
        self.token_stream_reader.packet_reader.try_read_float64()
    }

    fn try_read_slice(&mut self, length: usize) -> Option<&[u8]> {
        self.token_stream_reader
            .packet_reader
            .try_read_slice(length)
    }

    async fn read_byte(&mut self) -> TdsResult<u8> {
        self.token_stream_reader.packet_reader.read_byte().await
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        self.token_stream_reader
            .packet_reader
            .read_int16_big_endian()
            .await
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        self.token_stream_reader
            .packet_reader
            .read_int32_big_endian()
            .await
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        self.token_stream_reader.packet_reader.read_uint40().await
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        self.token_stream_reader.packet_reader.read_float32().await
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        self.token_stream_reader.packet_reader.read_float64().await
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        self.token_stream_reader.packet_reader.read_int16().await
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        self.token_stream_reader.packet_reader.read_uint16().await
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        self.token_stream_reader.packet_reader.read_uint24().await
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        self.token_stream_reader.packet_reader.read_int32().await
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        self.token_stream_reader.packet_reader.read_uint32().await
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        self.token_stream_reader.packet_reader.read_int64().await
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        self.token_stream_reader.packet_reader.read_uint64().await
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        self.token_stream_reader
            .packet_reader
            .read_bytes(buffer)
            .await
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        self.token_stream_reader
            .packet_reader
            .read_u8_varbyte()
            .await
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        self.token_stream_reader
            .packet_reader
            .read_u16_varbyte()
            .await
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        self.token_stream_reader
            .packet_reader
            .read_varchar_u16_length()
            .await
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        self.token_stream_reader
            .packet_reader
            .read_varchar_u8_length()
            .await
    }

    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
        self.token_stream_reader
            .packet_reader
            .read_unicode(string_length)
            .await
    }

    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String> {
        self.token_stream_reader
            .packet_reader
            .read_unicode_with_byte_length(byte_length)
            .await
    }

    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()> {
        self.token_stream_reader
            .packet_reader
            .skip_bytes(skip_count)
            .await
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        self.token_stream_reader
            .packet_reader
            .cancel_read_stream()
            .await
    }

    fn reset_reader(&mut self) {
        self.token_stream_reader.packet_reader.reset_reader();
    }
}

/// Helper function to create test NegotiatedSettings
#[allow(private_interfaces)]
pub fn create_test_negotiated_settings() -> NegotiatedSettings {
    crate::handler::handler_factory::create_test_negotiated_settings_internal()
}

/// Helper function to create test ExecutionContext
#[allow(private_interfaces)]
pub fn create_test_execution_context() -> crate::connection::execution_context::ExecutionContext {
    crate::connection::execution_context::ExecutionContext::new()
}

/// Helper function to create TdsClient for fuzzing
pub fn create_fuzz_tds_client(packet_reader: FuzzPacketReader, packet_size: u32) -> TdsClient {
    let mock_transport = MockTransport::new(packet_reader, packet_size);
    let negotiated_settings = create_test_negotiated_settings();
    let execution_context = create_test_execution_context();
    let client_context =
        crate::connection::client_context::ClientContext::with_data_source("tcp:localhost,1433");

    TdsClient::new(
        AnyTransport::dynamic(mock_transport),
        negotiated_settings,
        execution_context,
        client_context,
        Vec::new(),
    )
}
