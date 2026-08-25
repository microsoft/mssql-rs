// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::future::Future;
use std::mem::MaybeUninit;

use crate::core::TdsResult;

/// Sentinel `u16` length marking a length-prefixed varchar field as NULL.
pub(crate) const LENGTH_NULL: u16 = 0xffff;

macro_rules! define_tds_packet_reader {
    ($visibility:vis) => {
        /// Low-level TDS packet reading operations.
        $visibility trait TdsPacketReader {
            /// Returns a buffered byte, or `None` without consuming data if one is unavailable.
            #[inline]
            fn try_read_byte(&mut self) -> Option<u8> {
                None
            }

            /// Returns a buffered little-endian `i16`, or `None` without consuming partial data.
            #[inline]
            fn try_read_int16(&mut self) -> Option<i16> {
                None
            }

            /// Returns a buffered little-endian `u16`, or `None` without consuming partial data.
            #[inline]
            fn try_read_uint16(&mut self) -> Option<u16> {
                None
            }

            /// Returns a buffered little-endian 24-bit integer, or `None` without consuming partial data.
            #[inline]
            fn try_read_uint24(&mut self) -> Option<u32> {
                None
            }

            /// Returns a buffered little-endian `i32`, or `None` without consuming partial data.
            #[inline]
            fn try_read_int32(&mut self) -> Option<i32> {
                None
            }

            /// Returns a buffered little-endian `u32`, or `None` without consuming partial data.
            #[inline]
            fn try_read_uint32(&mut self) -> Option<u32> {
                None
            }

            /// Returns a buffered little-endian 40-bit integer, or `None` without consuming partial data.
            #[inline]
            fn try_read_uint40(&mut self) -> Option<u64> {
                None
            }

            /// Returns a buffered little-endian `i64`, or `None` without consuming partial data.
            #[inline]
            fn try_read_int64(&mut self) -> Option<i64> {
                None
            }

            /// Returns a buffered little-endian `f32`, or `None` without consuming partial data.
            #[inline]
            fn try_read_float32(&mut self) -> Option<f32> {
                None
            }

            /// Returns a buffered little-endian `f64`, or `None` without consuming partial data.
            #[inline]
            fn try_read_float64(&mut self) -> Option<f64> {
                None
            }

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

            fn read_bytes(
                &mut self,
                buffer: &mut [u8],
            ) -> impl Future<Output = TdsResult<usize>> + Send;
            /// Fills a possibly-uninitialized byte buffer.
            ///
            /// On `Ok(n)`, the first `n` elements of `buffer` are initialized.
            /// Callers rely on this to `set_len` an owned buffer, so an
            /// implementation must not report bytes it did not write.
            fn read_bytes_uninit(
                &mut self,
                buffer: &mut [MaybeUninit<u8>],
            ) -> impl Future<Output = TdsResult<usize>> + Send
            where
                Self: Send,
            {
                async move {
                    buffer.fill(MaybeUninit::new(0));
                    // SAFETY: every element was initialized immediately above.
                    let initialized = unsafe {
                        std::slice::from_raw_parts_mut(
                            buffer.as_mut_ptr().cast::<u8>(),
                            buffer.len(),
                        )
                    };
                    self.read_bytes(initialized).await
                }
            }
            fn read_u8_varbyte(&mut self) -> impl Future<Output = TdsResult<Vec<u8>>> + Send;
            #[allow(dead_code)]
            fn read_u16_varbyte(&mut self) -> impl Future<Output = TdsResult<Vec<u8>>> + Send;
            fn read_varchar_u16_length(
                &mut self,
            ) -> impl Future<Output = TdsResult<Option<String>>> + Send;
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
            fn skip_bytes(
                &mut self,
                skip_count: usize,
            ) -> impl Future<Output = TdsResult<()>> + Send;
            fn cancel_read_stream(&mut self) -> impl Future<Output = TdsResult<()>> + Send;
            fn reset_reader(&mut self);
        }
    };
}

#[cfg(not(fuzzing))]
define_tds_packet_reader!(pub(crate));

#[cfg(fuzzing)]
define_tds_packet_reader!(pub);
