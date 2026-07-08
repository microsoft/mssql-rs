// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common types and traits for token parsers.

use crate::{
    core::TdsResult, io::packet_reader::TdsPacketReader, io::token_stream::ParserContext,
    token::tokens::Tokens,
};
use async_trait::async_trait;

/// Maximum allowed size for Feature Extension acknowledgment data.
pub(crate) const MAX_ALLOWED_FE_DATA_IN_BYTES: usize = 1024;

/// Trait for parsing TDS tokens from a packet stream.
#[async_trait]
#[cfg(not(fuzzing))]
pub(crate) trait TokenParser<T>
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, context: &ParserContext) -> TdsResult<Tokens>;
}

#[async_trait]
#[cfg(fuzzing)]
pub trait TokenParser<T>
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, context: &ParserContext) -> TdsResult<Tokens>;
}

/// Test utilities for token parsers
#[cfg(test)]
pub(crate) mod test_utils {
    use super::*;
    use byteorder::{ByteOrder, LittleEndian};

    /// Mock reader for testing token parsers
    /// Provides a simple byte buffer implementation of TdsPacketReader
    pub(crate) struct MockReader {
        data: Vec<u8>,
        position: usize,
    }

    impl MockReader {
        pub(crate) fn new(data: Vec<u8>) -> Self {
            Self { data, position: 0 }
        }

        pub(crate) fn from_i32(value: i32) -> Self {
            let mut buf = vec![0u8; 4];
            LittleEndian::write_i32(&mut buf, value);
            Self::new(buf)
        }

        /// Encode UTF-16 LE string
        pub(crate) fn encode_utf16(s: &str) -> Vec<u8> {
            let utf16_units: Vec<u16> = s.encode_utf16().collect();
            let mut bytes = Vec::with_capacity(utf16_units.len() * 2);
            for unit in utf16_units {
                bytes.push((unit & 0xFF) as u8);
                bytes.push((unit >> 8) as u8);
            }
            bytes
        }
    }

    #[async_trait]
    impl TdsPacketReader for MockReader {
        async fn read_byte(&mut self) -> TdsResult<u8> {
            if self.position >= self.data.len() {
                return Err(crate::error::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "End of data",
                )));
            }
            let byte = self.data[self.position];
            self.position += 1;
            Ok(byte)
        }

        async fn read_int16(&mut self) -> TdsResult<i16> {
            let mut buf = [0u8; 2];
            for item in &mut buf {
                *item = self.read_byte().await?;
            }
            Ok(LittleEndian::read_i16(&buf))
        }

        async fn read_uint16(&mut self) -> TdsResult<u16> {
            let mut buf = [0u8; 2];
            for item in &mut buf {
                *item = self.read_byte().await?;
            }
            Ok(LittleEndian::read_u16(&buf))
        }

        async fn read_int32(&mut self) -> TdsResult<i32> {
            let mut buf = [0u8; 4];
            for item in &mut buf {
                *item = self.read_byte().await?;
            }
            Ok(LittleEndian::read_i32(&buf))
        }

        async fn read_uint32(&mut self) -> TdsResult<u32> {
            let mut buf = [0u8; 4];
            for item in &mut buf {
                *item = self.read_byte().await?;
            }
            Ok(LittleEndian::read_u32(&buf))
        }

        async fn read_int64(&mut self) -> TdsResult<i64> {
            let mut buf = [0u8; 8];
            for item in &mut buf {
                *item = self.read_byte().await?;
            }
            Ok(LittleEndian::read_i64(&buf))
        }

        async fn read_uint64(&mut self) -> TdsResult<u64> {
            let mut buf = [0u8; 8];
            for item in &mut buf {
                *item = self.read_byte().await?;
            }
            Ok(LittleEndian::read_u64(&buf))
        }

        async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
            let len = self.read_uint16().await? as usize;
            if len == 0xFFFF {
                return Ok(None);
            }
            let mut utf16_units = Vec::with_capacity(len);
            for _ in 0..len {
                utf16_units.push(self.read_uint16().await?);
            }
            String::from_utf16(&utf16_units).map(Some).map_err(|e| {
                crate::error::Error::ProtocolError(format!("UTF-16 decoding error: {e}"))
            })
        }

        async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
            let len = self.read_byte().await? as usize;
            let mut utf16_units = Vec::with_capacity(len);
            for _ in 0..len {
                utf16_units.push(self.read_uint16().await?);
            }
            String::from_utf16(&utf16_units).map_err(|e| {
                crate::error::Error::ProtocolError(format!("UTF-16 decoding error: {e}"))
            })
        }

        // Stub implementations for unused methods
        async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
            unimplemented!()
        }
        async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
            unimplemented!()
        }
        async fn read_uint40(&mut self) -> TdsResult<u64> {
            unimplemented!()
        }
        async fn read_float32(&mut self) -> TdsResult<f32> {
            unimplemented!()
        }
        async fn read_float64(&mut self) -> TdsResult<f64> {
            unimplemented!()
        }
        async fn read_uint24(&mut self) -> TdsResult<u32> {
            unimplemented!()
        }
        async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
            // If buffer is empty, return 0 (successfully read 0 bytes)
            if buffer.is_empty() {
                return Ok(0);
            }

            let available = self.data.len().saturating_sub(self.position);
            let to_read = available.min(buffer.len());

            if to_read == 0 {
                return Err(crate::error::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "No more data to read",
                )));
            }

            buffer[..to_read].copy_from_slice(&self.data[self.position..self.position + to_read]);
            self.position += to_read;
            Ok(to_read)
        }
        async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
            let len = self.read_byte().await? as usize;
            let mut bytes = vec![0u8; len];
            for byte in bytes.iter_mut() {
                *byte = self.read_byte().await?;
            }
            Ok(bytes)
        }
        async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
            unimplemented!()
        }
        async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
            let mut utf16_units = Vec::with_capacity(string_length);
            for _ in 0..string_length {
                utf16_units.push(self.read_uint16().await?);
            }
            String::from_utf16(&utf16_units).map_err(|e| {
                crate::error::Error::ProtocolError(format!("UTF-16 decoding error: {e}"))
            })
        }
        async fn read_unicode_with_byte_length(
            &mut self,
            _byte_length: usize,
        ) -> TdsResult<String> {
            unimplemented!()
        }
        async fn skip_bytes(&mut self, _skip_count: usize) -> TdsResult<()> {
            unimplemented!()
        }
        async fn cancel_read_stream(&mut self) -> TdsResult<()> {
            unimplemented!()
        }
        fn reset_reader(&mut self) {
            unimplemented!()
        }
    }
}
