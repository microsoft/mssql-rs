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

    /*
     * Non-blocking reads.
     *
     * Each of these returns a value when the reader's buffer already holds
     * enough bytes, and None otherwise. None is not an error. It means the
     * caller has to fall back to the matching async method, which is able to
     * wait for the next packet.
     *
     * They exist because one TDS packet normally carries many rows. Once the
     * packet has landed, every read inside it is a memory move, but the async
     * form still allocates a boxed future on each call to find that out.
     *
     * The defaults all decline, so a reader that keeps no buffer of its own
     * compiles unchanged and never takes the fast path.
     */

    /// Reads one byte if the buffer already holds it.
    fn try_read_byte(&mut self) -> Option<u8> {
        None
    }

    /// Reads a little-endian `u16` if the buffer already holds it.
    fn try_read_uint16(&mut self) -> Option<u16> {
        None
    }

    /// Reads a little-endian `i32` if the buffer already holds it.
    fn try_read_int32(&mut self) -> Option<i32> {
        None
    }

    /// Fills `buffer` if that many bytes are already held.
    fn try_read_bytes(&mut self, _buffer: &mut [u8]) -> Option<usize> {
        None
    }

    /// Borrows `length` bytes and consumes them.
    ///
    /// The borrow ends the next time the reader is used, so the caller has to
    /// copy or hand off the bytes before then.
    fn try_read_slice(&mut self, _length: usize) -> Option<&[u8]> {
        None
    }

    /// Borrows everything the buffer currently holds, consuming nothing.
    ///
    /// The caller decides afterwards how much it used and reports that through
    /// [`Self::consume_buffered`]. This is what lets a whole row be measured
    /// and then decoded under one borrow.
    fn buffered_slice(&self) -> Option<&[u8]> {
        None
    }

    /// Advances past `length` bytes that were seen through
    /// [`Self::buffered_slice`].
    ///
    /// Returns `false` when the buffer no longer holds that many bytes, which
    /// means the caller's view of it went stale.
    fn consume_buffered(&mut self, _length: usize) -> bool {
        false
    }
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

    fn try_read_byte(&mut self) -> Option<u8> {
        None
    }

    fn try_read_uint16(&mut self) -> Option<u16> {
        None
    }

    fn try_read_int32(&mut self) -> Option<i32> {
        None
    }

    fn try_read_bytes(&mut self, _buffer: &mut [u8]) -> Option<usize> {
        None
    }

    fn try_read_slice(&mut self, _length: usize) -> Option<&[u8]> {
        None
    }

    fn buffered_slice(&self) -> Option<&[u8]> {
        None
    }

    fn consume_buffered(&mut self, _length: usize) -> bool {
        false
    }
}

/*
 * Buffered read helpers.
 *
 * Each one tries the non-blocking form and falls back to the async form. They
 * are free functions rather than trait methods so that the buffered case stays
 * a direct call. A trait method under #[async_trait] would allocate a boxed
 * future first and only then discover that the bytes were already in hand.
 */

/// Reads one byte, from the buffer when it is there and from the network otherwise.
#[inline(always)]
pub(crate) async fn read_byte_buffered<T>(reader: &mut T) -> TdsResult<u8>
where
    T: TdsPacketReader + Send + Sync + ?Sized,
{
    match reader.try_read_byte() {
        Some(value) => Ok(value),
        None => reader.read_byte().await,
    }
}

/// Reads a little-endian `u16`, from the buffer when it is there.
#[inline(always)]
pub(crate) async fn read_uint16_buffered<T>(reader: &mut T) -> TdsResult<u16>
where
    T: TdsPacketReader + Send + Sync + ?Sized,
{
    match reader.try_read_uint16() {
        Some(value) => Ok(value),
        None => reader.read_uint16().await,
    }
}

/// Reads a little-endian `i32`, from the buffer when it is there.
#[inline(always)]
pub(crate) async fn read_int32_buffered<T>(reader: &mut T) -> TdsResult<i32>
where
    T: TdsPacketReader + Send + Sync + ?Sized,
{
    match reader.try_read_int32() {
        Some(value) => Ok(value),
        None => reader.read_int32().await,
    }
}

/// Fills `buffer`, from the reader's buffer when it holds that many bytes.
#[inline(always)]
pub(crate) async fn read_bytes_buffered<T>(reader: &mut T, buffer: &mut [u8]) -> TdsResult<usize>
where
    T: TdsPacketReader + Send + Sync + ?Sized,
{
    match reader.try_read_bytes(buffer) {
        Some(bytes_read) => Ok(bytes_read),
        None => reader.read_bytes(buffer).await,
    }
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
