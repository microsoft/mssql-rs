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
