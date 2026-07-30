// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Chunked PLP (`*(MAX)` / `xml`) streaming state for `SQLGetData`.
//!
//! When `SQLGetData` targets a PLP column, the TDS decoder pauses at the PLP
//! boundary (see [`OdbcRowWriter`](crate::fetch_engine::row_writer::OdbcRowWriter)) and
//! the bytes are pulled off the wire on demand via
//! [`ResultSet::read_active_plp_bytes`](mssql_tds::connection::tds_client::ResultSet::read_active_plp_bytes).
//! A single logical value is delivered across repeated `SQLGetData` calls: each
//! call copies as much as fits into the caller buffer, reports truncation with
//! `01004` / `SQL_SUCCESS_WITH_INFO`, and the final call returns `SQL_SUCCESS`.
//!
//! `PlpStream` owns the transcoding state machine between calls:
//! - `wire_carry` holds wire bytes that could not yet be transcoded (an odd
//!   trailing byte of a UTF-16 unit, or a lone high surrogate split across two
//!   wire chunks).
//! - `pending` holds already-transcoded output elements not yet delivered to the
//!   caller (the caller buffer may be smaller than one decoded wire chunk).

use mssql_tds::connection::tds_client::PlpEncoding;

/// Output C type for a PLP stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlpTarget {
    /// `SQL_C_CHAR` — UTF-8 bytes.
    Char,
    /// `SQL_C_WCHAR` — UTF-16 code units.
    WChar,
}

/// Transcoded output elements pending delivery.
#[derive(Debug)]
enum Pending {
    /// UTF-8 bytes (for `SQL_C_CHAR`).
    Bytes(std::collections::VecDeque<u8>),
    /// UTF-16 code units (for `SQL_C_WCHAR`).
    Units(std::collections::VecDeque<u16>),
}

/// Per-value PLP streaming state carried in `StmtState` across `SQLGetData`
/// calls for one PLP column.
#[derive(Debug)]
pub(crate) struct PlpStream {
    /// 1-based column ordinal being streamed.
    pub(crate) column: u16,
    /// Wire encoding of the source column.
    encoding: PlpEncoding,
    /// Wire bytes read but not yet transcodable (odd byte / split surrogate).
    wire_carry: Vec<u8>,
    /// Transcoded elements not yet delivered to the caller.
    pending: Pending,
    /// `true` once the wire stream reported end-of-value.
    wire_done: bool,
}

impl PlpStream {
    /// Creates a stream for `column` (1-based) with the given wire encoding and
    /// output target.
    pub(crate) fn new(column: u16, encoding: PlpEncoding, target: PlpTarget) -> Self {
        let pending = match target {
            PlpTarget::Char => Pending::Bytes(std::collections::VecDeque::new()),
            PlpTarget::WChar => Pending::Units(std::collections::VecDeque::new()),
        };
        Self {
            column,
            encoding,
            wire_carry: Vec::new(),
            pending,
            wire_done: false,
        }
    }

    /// Number of output elements currently buffered for delivery.
    fn pending_len(&self) -> usize {
        match &self.pending {
            Pending::Bytes(b) => b.len(),
            Pending::Units(u) => u.len(),
        }
    }

    /// `true` when the whole value has been transcoded and delivered.
    pub(crate) fn is_exhausted(&self) -> bool {
        self.wire_done && self.wire_carry.is_empty() && self.pending_len() == 0
    }

    /// `true` when more wire bytes must be pumped before anything can be
    /// delivered (nothing buffered yet and the wire is not drained).
    pub(crate) fn needs_pump(&self) -> bool {
        !self.wire_done && self.pending_len() == 0
    }

    /// Byte length of the output still buffered for delivery (elements × unit
    /// size). Used to report the ODBC length indicator once the wire is drained.
    pub(crate) fn pending_bytes(&self) -> usize {
        match &self.pending {
            Pending::Bytes(b) => b.len(),
            Pending::Units(u) => u.len() * std::mem::size_of::<u16>(),
        }
    }

    /// `true` once the wire stream has reported end-of-value.
    pub(crate) fn wire_done(&self) -> bool {
        self.wire_done
    }

    /// Feeds a freshly read wire chunk (`chunk`, possibly empty) into the
    /// transcoder, appending decoded elements to `pending`. `reached_end`
    /// indicates the wire stream is complete, which flushes any residual carry.
    fn absorb_wire(&mut self, chunk: &[u8], reached_end: bool) {
        self.wire_carry.extend_from_slice(chunk);
        match self.encoding {
            PlpEncoding::SingleByteText => self.absorb_single_byte(),
            PlpEncoding::Utf16Text => self.absorb_utf16(reached_end),
            // Binary is not reachable for CHAR/WCHAR targets (rejected earlier),
            // but treat the raw bytes as opaque passthrough for completeness.
            PlpEncoding::Binary => self.absorb_single_byte(),
        }
        if reached_end {
            self.wire_done = true;
        }
    }

    /// Single-byte / UTF-8 wire: emit bytes directly (CHAR) or widen to UTF-16
    /// (WCHAR). No cross-chunk carry is needed.
    fn absorb_single_byte(&mut self) {
        let bytes = std::mem::take(&mut self.wire_carry);
        match &mut self.pending {
            Pending::Bytes(out) => out.extend(bytes.iter().copied()),
            Pending::Units(out) => out.extend(bytes.iter().map(|b| u16::from(*b))),
        }
    }

    /// UTF-16LE wire: assemble whole `u16` units (buffering an odd trailing
    /// byte), then emit as UTF-16 units (WCHAR) or transcode to UTF-8 (CHAR).
    fn absorb_utf16(&mut self, reached_end: bool) {
        let full_units = self.wire_carry.len() / 2;
        let mut units: Vec<u16> = Vec::with_capacity(full_units);
        for pair in self.wire_carry.chunks_exact(2) {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        // Retain a dangling odd byte (start of the next unit) for the next chunk.
        let consumed = full_units * 2;
        self.wire_carry.drain(..consumed);

        match &mut self.pending {
            Pending::Units(out) => out.extend(units.iter().copied()),
            Pending::Bytes(out) => {
                // Transcode UTF-16 → UTF-8. A trailing lone high surrogate is held
                // back until its low surrogate arrives in the next chunk (unless
                // the stream ended, in which case it is emitted lossily).
                let split = if !reached_end
                    && units.last().is_some_and(|u| (0xD800..=0xDBFF).contains(u))
                {
                    units.len() - 1
                } else {
                    units.len()
                };
                let (decodable, carry_units) = units.split_at(split);
                for ch in char::decode_utf16(decodable.iter().copied()) {
                    let c = ch.unwrap_or('\u{FFFD}');
                    let mut b = [0u8; 4];
                    out.extend(c.encode_utf8(&mut b).as_bytes().iter().copied());
                }
                // Push a held high surrogate back onto the wire carry as raw LE
                // bytes so it recombines with the next chunk.
                for u in carry_units {
                    let le = u.to_le_bytes();
                    // Prepend before any dangling odd byte already retained.
                    self.wire_carry.insert(0, le[1]);
                    self.wire_carry.insert(0, le[0]);
                }
            }
        }
    }

    /// Delivers the next slice of the value into a `SQL_C_CHAR` caller buffer.
    /// Returns `(byte_length_indicator, remaining_after_call)` where the
    /// indicator is the untruncated byte length still available *before* this
    /// copy when the total is known, or `SQL_NO_TOTAL` semantics are signaled by
    /// the caller via `remaining_after_call` when the wire is not yet drained.
    pub(crate) fn deliver_char(&mut self, dst: *mut u8, cap_bytes: usize) -> PlpDelivery {
        let Pending::Bytes(pending) = &mut self.pending else {
            unreachable!("deliver_char on non-char stream");
        };
        // Contiguous view for copying.
        let pending_slice: Vec<u8> = pending.iter().copied().collect();
        deliver_generic::<u8>(pending_slice.as_slice(), dst, cap_bytes, |copied| {
            for _ in 0..copied {
                pending.pop_front();
            }
        })
    }

    /// Delivers the next slice of the value into a `SQL_C_WCHAR` caller buffer.
    /// `cap_units` is the buffer capacity in `u16` code units.
    pub(crate) fn deliver_wchar(&mut self, dst: *mut u16, cap_units: usize) -> PlpDelivery {
        let Pending::Units(pending) = &mut self.pending else {
            unreachable!("deliver_wchar on non-wchar stream");
        };
        let pending_slice: Vec<u16> = pending.iter().copied().collect();
        deliver_generic::<u16>(pending_slice.as_slice(), dst, cap_units, |copied| {
            for _ in 0..copied {
                pending.pop_front();
            }
        })
    }
}

/// Outcome of one delivery step.
pub(crate) struct PlpDelivery {
    /// Elements still buffered after this copy.
    #[allow(dead_code)]
    pub(crate) remaining_pending: usize,
    /// Whether the copy truncated (more data remains for a later call).
    pub(crate) truncated: bool,
}

/// Copies up to `cap - 1` elements from `src` into `dst`, NUL-terminating within
/// the buffer, and invokes `consume(copied)` to advance the source. Mirrors the
/// ODBC streaming contract used by `copy_with_nul` but keeps the remainder.
fn deliver_generic<T: Copy + Default>(
    src: &[T],
    dst: *mut T,
    cap: usize,
    consume: impl FnOnce(usize),
) -> PlpDelivery {
    if dst.is_null() {
        // Size query: nothing copied, nothing consumed.
        return PlpDelivery {
            remaining_pending: src.len(),
            truncated: !src.is_empty(),
        };
    }
    if cap == 0 {
        consume(0);
        return PlpDelivery {
            remaining_pending: src.len(),
            truncated: !src.is_empty(),
        };
    }
    let copy_len = src.len().min(cap - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst, copy_len);
        dst.add(copy_len).write(T::default());
    }
    consume(copy_len);
    PlpDelivery {
        remaining_pending: src.len() - copy_len,
        truncated: copy_len < src.len(),
    }
}

/// Reads one wire chunk into `stream` from the result set, updating transcode
/// state. Returns `Ok(())`; errors propagate the TDS read failure.
///
/// Pulls at most one 8 KiB chunk so control returns to the caller promptly; the
/// caller loops across `SQLGetData` invocations to drain the whole value.
pub(crate) async fn pump_wire<R>(
    stream: &mut PlpStream,
    rs: &mut R,
) -> mssql_tds::core::TdsResult<()>
where
    R: mssql_tds::connection::tds_client::ResultSet + Send + ?Sized,
{
    if stream.wire_done {
        return Ok(());
    }
    let mut buf = [0u8; 8192];
    let n = rs.read_active_plp_bytes(&mut buf).await?;
    let reached_end = rs.active_plp_reached_end();
    stream.absorb_wire(&buf[..n], reached_end || n == 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_char(stream: &mut PlpStream, cap: usize) -> (Vec<u8>, bool) {
        let mut buf = vec![0u8; cap];
        let d = stream.deliver_char(buf.as_mut_ptr(), cap);
        // Trim at NUL.
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        (buf[..end].to_vec(), d.truncated)
    }

    #[test]
    fn single_byte_char_passthrough_chunked() {
        let mut s = PlpStream::new(1, PlpEncoding::SingleByteText, PlpTarget::Char);
        s.absorb_wire(b"hello world", true);
        // 6-byte buffer → 5 chars + NUL, truncated.
        let (part1, trunc1) = drain_char(&mut s, 6);
        assert_eq!(part1, b"hello");
        assert!(trunc1);
        let (part2, trunc2) = drain_char(&mut s, 32);
        assert_eq!(part2, b" world");
        assert!(!trunc2);
        assert!(s.is_exhausted());
    }

    #[test]
    fn utf16_to_char_transcodes() {
        let mut s = PlpStream::new(1, PlpEncoding::Utf16Text, PlpTarget::Char);
        // "AB" in UTF-16LE.
        let wire: Vec<u8> = "AB".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        s.absorb_wire(&wire, true);
        let (out, trunc) = drain_char(&mut s, 32);
        assert_eq!(out, b"AB");
        assert!(!trunc);
        assert!(s.is_exhausted());
    }

    #[test]
    fn utf16_to_char_handles_odd_byte_split() {
        let mut s = PlpStream::new(1, PlpEncoding::Utf16Text, PlpTarget::Char);
        // 'Z' == 0x5A 0x00 in UTF-16LE; feed the two bytes in separate chunks.
        s.absorb_wire(&[0x5A], false);
        // Nothing decodable yet (odd byte held in carry).
        let (empty, _) = drain_char(&mut s, 32);
        assert!(empty.is_empty());
        s.absorb_wire(&[0x00], true);
        let (out, _) = drain_char(&mut s, 32);
        assert_eq!(out, b"Z");
        assert!(s.is_exhausted());
    }

    #[test]
    fn utf16_to_char_handles_surrogate_split_across_chunks() {
        let mut s = PlpStream::new(1, PlpEncoding::Utf16Text, PlpTarget::Char);
        // U+1F600 😀 → surrogate pair D83D DE00.
        let units: Vec<u16> = "😀".encode_utf16().collect();
        let hi = units[0].to_le_bytes();
        let lo = units[1].to_le_bytes();
        // First chunk: only the high surrogate.
        s.absorb_wire(&hi, false);
        let (empty, _) = drain_char(&mut s, 32);
        assert!(empty.is_empty(), "high surrogate alone must not decode");
        // Second chunk: the low surrogate completes the pair.
        s.absorb_wire(&lo, true);
        let (out, _) = drain_char(&mut s, 32);
        assert_eq!(String::from_utf8(out).unwrap(), "😀");
        assert!(s.is_exhausted());
    }

    #[test]
    fn utf16_to_wchar_passthrough() {
        let mut s = PlpStream::new(1, PlpEncoding::Utf16Text, PlpTarget::WChar);
        let wire: Vec<u8> = "hi".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        s.absorb_wire(&wire, true);
        let cap = 8usize;
        let mut buf = vec![0u16; cap];
        let d = s.deliver_wchar(buf.as_mut_ptr(), cap);
        assert!(!d.truncated);
        let end = buf.iter().position(|u| *u == 0).unwrap();
        assert_eq!(String::from_utf16(&buf[..end]).unwrap(), "hi");
        assert!(s.is_exhausted());
    }

    #[test]
    fn empty_value_is_immediately_exhausted() {
        let mut s = PlpStream::new(1, PlpEncoding::SingleByteText, PlpTarget::Char);
        s.absorb_wire(&[], true);
        let (out, trunc) = drain_char(&mut s, 16);
        assert!(out.is_empty());
        assert!(!trunc);
        assert!(s.is_exhausted());
    }

    #[test]
    fn zero_capacity_reports_truncation_without_consuming() {
        let mut s = PlpStream::new(1, PlpEncoding::SingleByteText, PlpTarget::Char);
        s.absorb_wire(b"abc", true);
        let d = s.deliver_char(std::ptr::null_mut(), 0);
        assert!(d.truncated);
        assert_eq!(d.remaining_pending, 3);
        // Still deliverable afterwards.
        let (out, _) = drain_char(&mut s, 16);
        assert_eq!(out, b"abc");
    }
}
