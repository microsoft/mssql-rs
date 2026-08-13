// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::future::Future;

use crate::core::TdsResult;

/// Sentinel `u16` length marking a length-prefixed varchar field as NULL.
pub(crate) const LENGTH_NULL: u16 = 0xffff;

#[cfg(not(fuzzing))]
pub(crate) trait TdsPacketReader {
    fn read_byte(&mut self) -> impl Future<Output = TdsResult<u8>> + Send;
    fn read_int16_big_endian(&mut self) -> impl Future<Output = TdsResult<i16>> + Send;
    fn read_int32_big_endian(&mut self) -> impl Future<Output = TdsResult<i32>> + Send;
    fn read_uint40(&mut self) -> impl Future<Output = TdsResult<u64>> + Send;

    fn read_float32(&mut self) -> impl Future<Output = TdsResult<f32>> + Send;
    fn read_float64(&mut self) -> impl Future<Output = TdsResult<f64>> + Send;
    fn read_int16(&mut self) -> impl Future<Output = TdsResult<i16>> + Send;
    fn read_uint16(&mut self) -> impl Future<Output = TdsResult<u16>> + Send;
    fn read_uint24(&mut self) -> impl Future<Output = TdsResult<u32>> + Send;
    fn read_int32(&mut self) -> impl Future<Output = TdsResult<i32>> + Send;
    fn read_uint32(&mut self) -> impl Future<Output = TdsResult<u32>> + Send;
    fn read_int64(&mut self) -> impl Future<Output = TdsResult<i64>> + Send;
    fn read_uint64(&mut self) -> impl Future<Output = TdsResult<u64>> + Send;

    fn read_bytes(&mut self, buffer: &mut [u8]) -> impl Future<Output = TdsResult<usize>> + Send;
    fn read_u8_varbyte(&mut self) -> impl Future<Output = TdsResult<Vec<u8>>> + Send;
    #[allow(dead_code)]
    fn read_u16_varbyte(&mut self) -> impl Future<Output = TdsResult<Vec<u8>>> + Send;
    fn read_varchar_u16_length(&mut self)
    -> impl Future<Output = TdsResult<Option<String>>> + Send;
    fn read_varchar_u8_length(&mut self) -> impl Future<Output = TdsResult<String>> + Send;
    #[allow(dead_code)]
    fn read_unicode(
        &mut self,
        string_length: usize,
    ) -> impl Future<Output = TdsResult<String>> + Send;
    fn read_unicode_with_byte_length(
        &mut self,
        byte_length: usize,
    ) -> impl Future<Output = TdsResult<String>> + Send;
    fn skip_bytes(&mut self, skip_count: usize) -> impl Future<Output = TdsResult<()>> + Send;
    fn cancel_read_stream(&mut self) -> impl Future<Output = TdsResult<()>> + Send;
    fn reset_reader(&mut self);
}

/// Low-level TDS packet reading operations (public under `fuzzing` cfg).
#[cfg(fuzzing)]
pub trait TdsPacketReader {
    fn read_byte(&mut self) -> impl Future<Output = TdsResult<u8>> + Send;
    fn read_int16_big_endian(&mut self) -> impl Future<Output = TdsResult<i16>> + Send;
    fn read_int32_big_endian(&mut self) -> impl Future<Output = TdsResult<i32>> + Send;
    fn read_uint40(&mut self) -> impl Future<Output = TdsResult<u64>> + Send;

    fn read_float32(&mut self) -> impl Future<Output = TdsResult<f32>> + Send;
    fn read_float64(&mut self) -> impl Future<Output = TdsResult<f64>> + Send;
    fn read_int16(&mut self) -> impl Future<Output = TdsResult<i16>> + Send;
    fn read_uint16(&mut self) -> impl Future<Output = TdsResult<u16>> + Send;
    fn read_uint24(&mut self) -> impl Future<Output = TdsResult<u32>> + Send;
    fn read_int32(&mut self) -> impl Future<Output = TdsResult<i32>> + Send;
    fn read_uint32(&mut self) -> impl Future<Output = TdsResult<u32>> + Send;
    fn read_int64(&mut self) -> impl Future<Output = TdsResult<i64>> + Send;
    fn read_uint64(&mut self) -> impl Future<Output = TdsResult<u64>> + Send;

    fn read_bytes(&mut self, buffer: &mut [u8]) -> impl Future<Output = TdsResult<usize>> + Send;
    fn read_u8_varbyte(&mut self) -> impl Future<Output = TdsResult<Vec<u8>>> + Send;
    fn read_u16_varbyte(&mut self) -> impl Future<Output = TdsResult<Vec<u8>>> + Send;
    fn read_varchar_u16_length(&mut self)
    -> impl Future<Output = TdsResult<Option<String>>> + Send;
    fn read_varchar_u8_length(&mut self) -> impl Future<Output = TdsResult<String>> + Send;
    fn read_unicode(
        &mut self,
        string_length: usize,
    ) -> impl Future<Output = TdsResult<String>> + Send;
    fn read_unicode_with_byte_length(
        &mut self,
        byte_length: usize,
    ) -> impl Future<Output = TdsResult<String>> + Send;
    fn skip_bytes(&mut self, skip_count: usize) -> impl Future<Output = TdsResult<()>> + Send;
    fn cancel_read_stream(&mut self) -> impl Future<Output = TdsResult<()>> + Send;
    fn reset_reader(&mut self);
}
