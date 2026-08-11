// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;

use crate::core::TdsResult;

/// Sentinel `u16` length marking a length-prefixed varchar field as NULL.
pub(crate) const LENGTH_NULL: u16 = 0xffff;

#[async_trait]
#[cfg(not(fuzzing))]
pub(crate) trait TdsPacketReader {
    async fn read_byte(&mut self) -> TdsResult<u8>;
    async fn read_int16_big_endian(&mut self) -> TdsResult<i16>;
    async fn read_int32_big_endian(&mut self) -> TdsResult<i32>;
    async fn read_uint40(&mut self) -> TdsResult<u64>;

    async fn read_float32(&mut self) -> TdsResult<f32>;
    async fn read_float64(&mut self) -> TdsResult<f64>;
    async fn read_int16(&mut self) -> TdsResult<i16>;
    async fn read_uint16(&mut self) -> TdsResult<u16>;
    async fn read_uint24(&mut self) -> TdsResult<u32>;
    async fn read_int32(&mut self) -> TdsResult<i32>;
    async fn read_uint32(&mut self) -> TdsResult<u32>;
    async fn read_int64(&mut self) -> TdsResult<i64>;
    async fn read_uint64(&mut self) -> TdsResult<u64>;

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize>;
    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>>;
    #[allow(dead_code)]
    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>>;
    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>>;
    async fn read_varchar_u8_length(&mut self) -> TdsResult<String>;
    #[allow(dead_code)]
    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String>;
    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String>;
    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()>;
    async fn cancel_read_stream(&mut self) -> TdsResult<()>;
    fn reset_reader(&mut self);
}

/// Low-level TDS packet reading operations (public under `fuzzing` cfg).
#[async_trait]
#[cfg(fuzzing)]
pub trait TdsPacketReader {
    async fn read_byte(&mut self) -> TdsResult<u8>;
    async fn read_int16_big_endian(&mut self) -> TdsResult<i16>;
    async fn read_int32_big_endian(&mut self) -> TdsResult<i32>;
    async fn read_uint40(&mut self) -> TdsResult<u64>;

    async fn read_float32(&mut self) -> TdsResult<f32>;
    async fn read_float64(&mut self) -> TdsResult<f64>;
    async fn read_int16(&mut self) -> TdsResult<i16>;
    async fn read_uint16(&mut self) -> TdsResult<u16>;
    async fn read_uint24(&mut self) -> TdsResult<u32>;
    async fn read_int32(&mut self) -> TdsResult<i32>;
    async fn read_uint32(&mut self) -> TdsResult<u32>;
    async fn read_int64(&mut self) -> TdsResult<i64>;
    async fn read_uint64(&mut self) -> TdsResult<u64>;

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize>;
    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>>;
    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>>;
    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>>;
    async fn read_varchar_u8_length(&mut self) -> TdsResult<String>;
    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String>;
    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String>;
    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()>;
    async fn cancel_read_stream(&mut self) -> TdsResult<()>;
    fn reset_reader(&mut self);
}

// Blanket implementation for Box<dyn TdsPacketReader> to enable dynamic dispatch
#[async_trait]
impl TdsPacketReader for Box<dyn TdsPacketReader + Send + Sync> {
    async fn read_byte(&mut self) -> TdsResult<u8> {
        (**self).read_byte().await
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        (**self).read_int16_big_endian().await
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        (**self).read_int32_big_endian().await
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        (**self).read_uint40().await
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        (**self).read_float32().await
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        (**self).read_float64().await
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        (**self).read_int16().await
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        (**self).read_uint16().await
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        (**self).read_uint24().await
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        (**self).read_int32().await
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        (**self).read_uint32().await
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        (**self).read_int64().await
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        (**self).read_uint64().await
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        (**self).read_bytes(buffer).await
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        (**self).read_u8_varbyte().await
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        (**self).read_u16_varbyte().await
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        (**self).read_varchar_u16_length().await
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        (**self).read_varchar_u8_length().await
    }

    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
        (**self).read_unicode(string_length).await
    }

    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String> {
        (**self).read_unicode_with_byte_length(byte_length).await
    }

    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()> {
        (**self).skip_bytes(skip_count).await
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        (**self).cancel_read_stream().await
    }

    fn reset_reader(&mut self) {
        (**self).reset_reader()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::message::messages::PacketType;
    use byteorder::{BigEndian, ByteOrder, LittleEndian};

    macro_rules! append_method {
        ($name:ident, $type:ty, $size:expr_2021, $write_fn:ident) => {
            pub(crate) fn $name(&mut self, number: $type) -> &mut TestPacketBuilder {
                let mut buffer = [0u8; $size];
                LittleEndian::$write_fn(&mut buffer, number);
                self.data.extend_from_slice(&buffer);
                self.payload_length += $size as u16;
                self
            }
        };
    }

    /// Builds a single well-formed TDS packet: an 8-byte header (EOM status,
    /// big-endian length) followed by the appended payload bytes.
    pub(crate) struct TestPacketBuilder {
        data: Vec<u8>,
        payload_length: u16,
    }

    impl TestPacketBuilder {
        pub(crate) fn new(packet_type: PacketType) -> TestPacketBuilder {
            let mut data: Vec<u8> = vec![0; 8];
            // Set status to EOM by default
            data[1] = 0x1;
            data[0] = packet_type as u8;

            TestPacketBuilder {
                data,
                payload_length: 0,
            }
        }

        pub(crate) fn append_byte(&mut self, byte: u8) -> &mut TestPacketBuilder {
            self.data.push(byte);
            self.payload_length += 1;
            self
        }

        pub(crate) fn append_bytes(&mut self, bytes: &[u8]) -> &mut TestPacketBuilder {
            self.data.extend_from_slice(bytes);
            self.payload_length += bytes.len() as u16;
            self
        }

        append_method!(append_u16, u16, 2, write_u16);
        append_method!(append_i16, i16, 2, write_i16);
        append_method!(append_f32, f32, 4, write_f32);
        append_method!(append_f64, f64, 8, write_f64);
        append_method!(append_i64, i64, 8, write_i64);
        append_method!(append_u32, u32, 4, write_u32);
        append_method!(append_i32, i32, 4, write_i32);
        append_method!(append_u64, u64, 8, write_u64);

        pub(crate) fn build(&mut self) -> Vec<u8> {
            BigEndian::write_u16(&mut self.data[2..4], self.payload_length + 8);
            self.data.clone()
        }
    }

    /// Encodes `value` as little-endian UTF-16 bytes, the on-the-wire form of
    /// TDS unicode strings.
    pub(crate) fn encode_utf16_le(value: &str) -> Vec<u8> {
        value
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect()
    }
}
