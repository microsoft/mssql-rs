// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sans-I/O [`TdsPacketReader`] over an in-memory slice.
//!
//! `NetworkTransport` already holds a whole decrypted TDS packet in its read
//! buffer, so for most columns every byte the decoder needs is present before
//! it is asked for. The async path still pays for a cancellation/timeout
//! composition, a boxed `async_trait` future per transport call, and a row
//! writer allocation — all to "wait" for bytes sitting in L1.
//!
//! `SliceReader` lets the *existing* decoders run against those buffered bytes
//! with no I/O. Because it never yields, a decode future driven over it
//! completes on the first poll, which is what makes the synchronous fast path
//! in `NetworkTransport::try_decode_column_buffered` possible without
//! duplicating any protocol logic.
//!
//! Running out of bytes is reported as [`ErrorKind::UnexpectedEof`]; the caller
//! treats that as "fall back to the async path" and leaves the transport buffer
//! untouched, so the fallback re-decodes the column from its original position.

use async_trait::async_trait;
use byteorder::{ByteOrder, LittleEndian};
use std::io::{Error, ErrorKind};

use crate::core::TdsResult;
use crate::io::packet_reader::TdsPacketReader;

/// Reads TDS primitives from a fixed in-memory slice without ever awaiting.
pub(crate) struct SliceReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> SliceReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// Bytes consumed so far. Only meaningful after a successful decode, when
    /// it tells the transport how far to advance its read buffer.
    pub(crate) fn consumed(&self) -> usize {
        self.position
    }

    fn take(&mut self, count: usize) -> TdsResult<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(Self::out_of_bytes)?;
        if end > self.data.len() {
            return Err(Self::out_of_bytes().into());
        }
        let slice = &self.data[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn out_of_bytes() -> Error {
        Error::new(
            ErrorKind::UnexpectedEof,
            "buffered slice exhausted; retry on the async path",
        )
    }
}

#[async_trait]
impl TdsPacketReader for SliceReader<'_> {
    async fn read_byte(&mut self) -> TdsResult<u8> {
        Ok(self.take(1)?[0])
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        Ok(byteorder::BigEndian::read_i16(self.take(2)?))
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        Ok(byteorder::BigEndian::read_i32(self.take(4)?))
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        Ok(LittleEndian::read_uint(self.take(5)?, 5))
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        Ok(LittleEndian::read_f32(self.take(4)?))
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        Ok(LittleEndian::read_f64(self.take(8)?))
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        Ok(LittleEndian::read_i16(self.take(2)?))
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        Ok(LittleEndian::read_u16(self.take(2)?))
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        Ok(LittleEndian::read_u24(self.take(3)?))
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        Ok(LittleEndian::read_i32(self.take(4)?))
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        Ok(LittleEndian::read_u32(self.take(4)?))
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        Ok(LittleEndian::read_i64(self.take(8)?))
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        Ok(LittleEndian::read_u64(self.take(8)?))
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        let count = buffer.len();
        buffer.copy_from_slice(self.take(count)?);
        Ok(count)
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let length = self.read_byte().await? as usize;
        Ok(self.take(length)?.to_vec())
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let length = self.read_uint16().await? as usize;
        Ok(self.take(length)?.to_vec())
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        let length: u16 = self.read_uint16().await?;
        if length == crate::io::packet_reader::PacketReader::LENGTHNULL {
            return Ok(None);
        }
        let string = self
            .read_unicode_with_byte_length((length << 1) as usize)
            .await?;
        Ok(Some(string))
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        let length: u8 = self.read_byte().await?;
        self.read_unicode_with_byte_length((length << 1) as usize)
            .await
    }

    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
        self.read_unicode_with_byte_length(string_length * 2).await
    }

    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String> {
        // Mirrors `PacketReader`: the same cap must apply so a column that is
        // rejected on the async path is not silently accepted here.
        const MAX_STRING_BYTE_LENGTH: usize = u8::MAX as usize * 2;
        if byte_length > MAX_STRING_BYTE_LENGTH {
            return Err(crate::error::Error::UsageError(format!(
                "Unicode string byte length {byte_length} exceeds maximum allowed size of {MAX_STRING_BYTE_LENGTH} bytes"
            )));
        }

        let bytes = self.take(byte_length)?;
        let mut units = Vec::with_capacity(bytes.len() / 2);
        for chunk in bytes.chunks_exact(2) {
            units.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        String::from_utf16(&units).map_err(|e| Error::new(ErrorKind::InvalidData, e).into())
    }

    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()> {
        self.take(skip_count)?;
        Ok(())
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        Ok(())
    }

    fn reset_reader(&mut self) {
        self.position = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_little_endian_primitives_and_tracks_consumption() {
        let bytes = [0x01u8, 0x02, 0x00, 0x03, 0x00, 0x00, 0x00];
        let mut reader = SliceReader::new(&bytes);

        assert_eq!(reader.read_byte().await.unwrap(), 1);
        assert_eq!(reader.read_uint16().await.unwrap(), 2);
        assert_eq!(reader.read_int32().await.unwrap(), 3);
        assert_eq!(reader.consumed(), 7);
    }

    #[tokio::test]
    async fn running_out_of_bytes_reports_unexpected_eof() {
        let bytes = [0x01u8, 0x02];
        let mut reader = SliceReader::new(&bytes);

        let err = reader.read_int32().await.unwrap_err();
        assert!(
            format!("{err}").contains("exhausted"),
            "expected an out-of-bytes error, got: {err}"
        );
    }

    #[tokio::test]
    async fn partial_read_does_not_advance_past_the_slice() {
        let bytes = [0x07u8, 0xff];
        let mut reader = SliceReader::new(&bytes);

        assert_eq!(reader.read_byte().await.unwrap(), 7);
        assert!(reader.read_int64().await.is_err());
        // The failed read must not have consumed anything beyond the slice, so
        // the caller can safely discard this reader and retry asynchronously.
        assert!(reader.consumed() <= bytes.len());
    }

    #[tokio::test]
    async fn reads_unicode_string() {
        // "hi" as UTF-16LE, preceded by a u8 character count.
        let bytes = [0x02u8, b'h', 0x00, b'i', 0x00];
        let mut reader = SliceReader::new(&bytes);

        assert_eq!(reader.read_varchar_u8_length().await.unwrap(), "hi");
        assert_eq!(reader.consumed(), 5);
    }
}
