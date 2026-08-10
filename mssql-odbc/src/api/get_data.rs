// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal SQLGetData implementation for Phase 1.

use tracing::{debug, error};

use super::odbc_types::{
    SQL_C_CHAR, SQL_C_WCHAR, SQL_ERROR, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_NULL_DATA, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlUSmallInt,
};
use super::sqlstate::*;
use crate::api::odbc_types::SqlWChar;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{
    GetDataProgress, GetDataUnits, STMT_STATE_CURSOR_OPEN, StmtState,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};
use mssql_tds::datatypes::column_values::{ColumnValues, SqlDateTime2};

/// Implements SQLGetData for current-row retrieval.
///
/// Phase 1 scope:
/// - Requires an open cursor and a current fetched row.
/// - Supports only `SQL_C_CHAR` output.
/// - Supports basic scalar conversion to UTF-8 text.
/// - Repeated calls on the same column do not advance an offset; each call
///   returns the same prefix for the current value (no chunked streaming yet).
pub(crate) unsafe fn sql_get_data(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number,
        target_type,
        ?target_value_ptr,
        buffer_length,
        ?strlen_or_ind_ptr,
        "SQLGetData called",
    );

    crate::ffi_entry!("SQLGetData", unsafe {
        sql_get_data_impl(
            statement_handle,
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        )
    })
}

unsafe fn sql_get_data_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLGetData: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLGetData: handle is not a STMT"
    );

    sql_get_data_safe(
        stmt,
        column_number,
        target_type,
        target_value_ptr,
        buffer_length,
        strlen_or_ind_ptr,
    )
}

fn sql_get_data_safe(
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    buffer_length: SqlLen,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    debug_assert!(
        buffer_length >= 0,
        "SQLGetData: DM should reject negative buffer_length (HY090)"
    );

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetData: stmt mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut stmt_state);

    if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
        post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
        return SQL_ERROR;
    }

    let Some(row) = stmt_state.current_row.as_ref() else {
        post_sql_error(&mut stmt_state, SQLSTATE_24000, 0, "No current row");
        return SQL_ERROR;
    };

    let col_index = usize::from(column_number);
    if col_index == 0 || col_index > row.len() {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    if target_type != SQL_C_CHAR && target_type != SQL_C_WCHAR {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_HYC00,
            0,
            "Target type not yet implemented",
        );
        return SQL_ERROR;
    }

    // Output buffer capacity in element units (u8 for SQL_C_CHAR, SqlWChar for
    // SQL_C_WCHAR). buffer_length is always in bytes per the ODBC spec.
    let buf_elements = if target_type == SQL_C_WCHAR {
        (buffer_length as usize) / std::mem::size_of::<SqlWChar>()
    } else {
        buffer_length as usize
    };

    let is_wchar = target_type == SQL_C_WCHAR;

    // (Re)build the streaming cursor when a different column becomes active or
    // the requested C type changed. The value is converted + encoded exactly
    // once here; subsequent calls serve cached chunks, keeping chunked LOB
    // retrieval O(n) total instead of re-encoding the full value per call.
    let needs_build = match &stmt_state.getdata {
        Some(p) => p.column != column_number || !p.units.matches_wchar(is_wchar),
        None => true,
    };
    if needs_build {
        let value = &row[col_index - 1];

        // Zero-heap fast path: numeric/temporal/GUID values render to short
        // ASCII that virtually always fits the caller's buffer in one call
        // (the bench uses an 8 KiB buffer). Format straight onto the stack and
        // copy out, skipping the per-value String + Vec allocation that native
        // avoids. Only the rare doesn't-fit case falls through to the cursor.
        if let Some((tmp, len)) = format_scalar_ascii(value) {
            let cap = buf_elements.saturating_sub(1);
            if len <= cap {
                let ascii = &tmp[..len];
                let ind_bytes = if is_wchar { len * 2 } else { len };
                unsafe { write_if_some(strlen_or_ind_ptr, ind_bytes as SqlLen) };
                if is_wchar {
                    let mut wide = [0u16; SCALAR_ASCII_CAP];
                    for (w, &b) in wide.iter_mut().zip(ascii) {
                        *w = u16::from(b);
                    }
                    unsafe {
                        copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &wide[..len]);
                    }
                } else {
                    unsafe {
                        copy_with_nul(target_value_ptr as *mut u8, buf_elements, ascii);
                    }
                }
                // Record consumption without allocating (empty Vec) so a second
                // SQLGetData on this column returns SQL_NO_DATA.
                stmt_state.getdata = Some(GetDataProgress {
                    column: column_number,
                    units: GetDataUnits::Char(Vec::new()),
                    offset: 0,
                    exhausted: true,
                });
                return SQL_SUCCESS;
            }
        }

        // Zero-copy string fast path: an ASCII/UTF-8 char column served as
        // SQL_C_CHAR needs no transcoding — serve its bytes straight from the
        // fetched row when they fit, skipping the decode + String allocation
        // that `to_utf8_string()` would perform.
        if !is_wchar
            && let ColumnValues::String(s) = value
            && let Some(bytes) = string_passthrough_bytes(s)
        {
            let len = bytes.len();
            let cap = buf_elements.saturating_sub(1);
            if len <= cap {
                unsafe { write_if_some(strlen_or_ind_ptr, len as SqlLen) };
                unsafe {
                    copy_with_nul(target_value_ptr as *mut u8, buf_elements, bytes);
                }
                stmt_state.getdata = Some(GetDataProgress {
                    column: column_number,
                    units: GetDataUnits::Char(Vec::new()),
                    offset: 0,
                    exhausted: true,
                });
                return SQL_SUCCESS;
            }
        }

        // Zero-alloc UTF-16 → UTF-8 fast path: an NVarchar column served as
        // SQL_C_CHAR transcodes straight onto the stack, skipping the encoding_rs
        // decode + String allocation in `to_utf8_string()`. Small strings (the
        // common case) fit `STRING_STACK_CAP`; anything larger or malformed falls
        // through to the allocating cursor path.
        if !is_wchar
            && let ColumnValues::String(s) = value
            && s.is_utf16()
        {
            let mut tmp = [0u8; STRING_STACK_CAP];
            if let Some(len) = utf16le_to_utf8(&s.bytes, &mut tmp) {
                let cap = buf_elements.saturating_sub(1);
                if len <= cap {
                    unsafe { write_if_some(strlen_or_ind_ptr, len as SqlLen) };
                    unsafe {
                        copy_with_nul(target_value_ptr as *mut u8, buf_elements, &tmp[..len]);
                    }
                    stmt_state.getdata = Some(GetDataProgress {
                        column: column_number,
                        units: GetDataUnits::Char(Vec::new()),
                        offset: 0,
                        exhausted: true,
                    });
                    return SQL_SUCCESS;
                }
            }
        }

        let units = if matches!(value, ColumnValues::Null) {
            GetDataUnits::Null
        } else {
            let Some(as_text) = column_value_to_text(value) else {
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HYC00,
                    0,
                    "Column type conversion not yet implemented",
                );
                return SQL_ERROR;
            };
            if is_wchar {
                GetDataUnits::WChar(as_text.encode_utf16().collect())
            } else {
                GetDataUnits::Char(as_text.into_bytes())
            }
        };
        stmt_state.getdata = Some(GetDataProgress {
            column: column_number,
            units,
            offset: 0,
            exhausted: false,
        });
    }

    serve_getdata_chunk(
        &mut stmt_state,
        is_wchar,
        target_value_ptr,
        buf_elements,
        strlen_or_ind_ptr,
    )
}

/// Serves the next chunk from the active `SQLGetData` stream, advancing its
/// offset and posting `01004` on truncation. Returns `SQL_NO_DATA` once the
/// value has been fully delivered (the call after the terminal chunk).
fn serve_getdata_chunk(
    stmt_state: &mut StmtState,
    is_wchar: bool,
    target_value_ptr: SqlPointer,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
) -> SqlReturn {
    let Some(mut prog) = stmt_state.getdata.take() else {
        return SQL_NO_DATA;
    };
    if prog.exhausted {
        // Terminal chunk already delivered; end the sequence and drop the
        // cursor so a re-fetch of the same column starts fresh.
        return SQL_NO_DATA;
    }

    let (advance, truncated) = match &prog.units {
        GetDataUnits::Null => {
            unsafe { write_if_some(strlen_or_ind_ptr, SQL_NULL_DATA) };
            if is_wchar {
                unsafe { copy_with_nul(target_value_ptr as *mut SqlWChar, buf_elements, &[]) };
            } else {
                unsafe { copy_with_nul(target_value_ptr as *mut u8, buf_elements, &[]) };
            }
            (0usize, false)
        }
        GetDataUnits::Char(data) => serve_units::<u8>(
            data,
            prog.offset,
            target_value_ptr as *mut u8,
            buf_elements,
            strlen_or_ind_ptr,
        ),
        GetDataUnits::WChar(data) => serve_units::<u16>(
            data,
            prog.offset,
            target_value_ptr as *mut SqlWChar,
            buf_elements,
            strlen_or_ind_ptr,
        ),
    };

    prog.offset += advance;
    prog.exhausted = !truncated;
    stmt_state.getdata = Some(prog);

    if truncated {
        post_diag(stmt_state, ERR_STRING_RIGHT_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

/// Copies one buffer-sized chunk of `data` starting at `offset`, writing the
/// remaining byte count into the length/indicator pointer per the ODBC spec.
/// Returns `(units_delivered, truncated)` where `truncated` means more data
/// remains after this chunk.
fn serve_units<T: Copy + Default>(
    data: &[T],
    offset: usize,
    dst: *mut T,
    buf_elements: usize,
    strlen_or_ind_ptr: *mut SqlLen,
) -> (usize, bool) {
    let total = data.len();
    let remaining = total - offset;
    let remaining_bytes = (remaining * std::mem::size_of::<T>()) as SqlLen;
    unsafe { write_if_some(strlen_or_ind_ptr, remaining_bytes) };

    // Leave room for the NUL terminator the driver always appends.
    let cap = buf_elements.saturating_sub(1);
    let n = remaining.min(cap);
    unsafe {
        copy_with_nul(dst, buf_elements, &data[offset..offset + n]);
    }
    (n, remaining > n)
}

/// Writes `src` to the caller's output buffer with ODBC string semantics:
/// the indicator (when present) reports the untruncated byte length, the
/// payload is NUL-terminated within the buffer, and truncation is reported via
/// SQLSTATE 01004 + `SQL_SUCCESS_WITH_INFO`.
///
/// `buf_elements` is the buffer capacity in units of `T` (not bytes).
///
/// The caller-provided pointers are written through small `unsafe` blocks
/// inside this function; both pointer arguments are obligations of the FFI
/// caller (validated against the buffer length passed by the DM).
fn column_value_to_text(v: &ColumnValues) -> Option<String> {
    match v {
        ColumnValues::TinyInt(x) => Some(x.to_string()),
        ColumnValues::SmallInt(x) => Some(x.to_string()),
        ColumnValues::Int(x) => Some(x.to_string()),
        ColumnValues::BigInt(x) => Some(x.to_string()),
        ColumnValues::Real(x) => Some(x.to_string()),
        ColumnValues::Float(x) => Some(x.to_string()),
        ColumnValues::Bit(x) => Some(if *x { "1".into() } else { "0".into() }),
        ColumnValues::String(s) => Some(s.to_utf8_string()),
        ColumnValues::Uuid(u) => Some(u.to_string()),
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => Some(d.to_string()),
        ColumnValues::DateTime2(dt2) => Some(format_datetime2(dt2)),
        ColumnValues::Null => Some(String::new()),
        _ => None,
    }
}

/// Stack scratch size for one-shot scalar rendering. Fits the widest scalar
/// text: `decimal(38)` sign+digits+point (41), `uniqueidentifier` (36),
/// `datetime2` (27), and any integer/float shortest form.
const SCALAR_ASCII_CAP: usize = 64;

/// Minimal stack-backed `fmt::Write` sink; renders scalar text with no heap
/// allocation. Writes beyond capacity are dropped, which `format_scalar_ascii`
/// treats as "doesn't fit" and falls back to the allocating path.
struct StackBuf {
    buf: [u8; SCALAR_ASCII_CAP],
    len: usize,
}

impl std::fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let bytes = s.as_bytes();
        if self.len + bytes.len() > self.buf.len() {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

/// Renders a numeric/temporal/GUID value to ASCII on the stack, returning the
/// scratch buffer and byte length. Returns `None` for strings, NULL, and
/// unsupported types, which take the allocating cursor path. All produced text
/// is pure ASCII, so the SQL_C_WCHAR path can widen bytes 1:1.
fn format_scalar_ascii(v: &ColumnValues) -> Option<([u8; SCALAR_ASCII_CAP], usize)> {
    use std::fmt::Write as _;
    let mut sb = StackBuf {
        buf: [0u8; SCALAR_ASCII_CAP],
        len: 0,
    };
    let ok = match v {
        ColumnValues::TinyInt(x) => write!(sb, "{x}"),
        ColumnValues::SmallInt(x) => write!(sb, "{x}"),
        ColumnValues::Int(x) => write!(sb, "{x}"),
        ColumnValues::BigInt(x) => write!(sb, "{x}"),
        ColumnValues::Real(x) => write!(sb, "{x}"),
        ColumnValues::Float(x) => write!(sb, "{x}"),
        ColumnValues::Bit(x) => write!(sb, "{}", if *x { 1 } else { 0 }),
        ColumnValues::Uuid(u) => write!(sb, "{u}"),
        ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => write_decimal(&mut sb, d),
        ColumnValues::DateTime2(dt2) => write_datetime2(&mut sb, dt2),
        _ => return None,
    };
    ok.ok().map(|()| (sb.buf, sb.len))
}

/// Returns a char column's bytes when they can be handed to an `SQL_C_CHAR`
/// caller with no transcoding: UTF-8 payloads pass through directly, and
/// single-byte (collation-based) or delayed-encoding payloads pass through when
/// they are pure ASCII (identical under ASCII, Windows-125x, and UTF-8). Any
/// non-ASCII single-byte or UTF-16 payload returns `None` and takes the
/// allocating decode path.
fn string_passthrough_bytes(s: &mssql_tds::datatypes::sql_string::SqlString) -> Option<&[u8]> {
    use mssql_tds::datatypes::sql_string::EncodingType;
    match s.encoding_type() {
        EncodingType::Utf8 => Some(&s.bytes),
        EncodingType::LcidBased(_) | EncodingType::DelayedSet => {
            s.bytes.iter().all(u8::is_ascii).then_some(&s.bytes[..])
        }
        EncodingType::Utf16 => None,
    }
}

/// Stack scratch for one-shot string transcodes. Covers the common inline
/// `NVARCHAR`/`VARCHAR` column (a few hundred bytes); larger values fall through
/// to the allocating cursor path.
const STRING_STACK_CAP: usize = 1024;

/// Transcodes UTF-16LE `bytes` into UTF-8 written to `out`, returning the byte
/// length on success. Returns `None` on odd byte counts, malformed surrogate
/// pairs, or when the output would not fit `out` — all of which defer to the
/// allocating `to_utf8_string()` path.
fn utf16le_to_utf8(bytes: &[u8], out: &mut [u8]) -> Option<usize> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut o = 0usize;
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        let u = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        i += 2;
        let cp: u32 = if (0xD800..0xDC00).contains(&u) {
            if i + 1 >= bytes.len() {
                return None;
            }
            let lo = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
            if !(0xDC00..0xE000).contains(&lo) {
                return None;
            }
            i += 2;
            0x1_0000 + ((u32::from(u) - 0xD800) << 10) + (u32::from(lo) - 0xDC00)
        } else if (0xDC00..0xE000).contains(&u) {
            return None;
        } else {
            u32::from(u)
        };
        o += encode_utf8(cp, out.get_mut(o..)?)?;
    }
    Some(o)
}

/// Encodes a scalar Unicode code point as UTF-8 into `out`, returning the byte
/// length, or `None` if `out` is too small.
#[inline]
fn encode_utf8(cp: u32, out: &mut [u8]) -> Option<usize> {
    match cp {
        0x0000..=0x007F => {
            *out.first_mut()? = cp as u8;
            Some(1)
        }
        0x0080..=0x07FF => {
            let b = out.get_mut(..2)?;
            b[0] = 0xC0 | (cp >> 6) as u8;
            b[1] = 0x80 | (cp & 0x3F) as u8;
            Some(2)
        }
        0x0800..=0xFFFF => {
            let b = out.get_mut(..3)?;
            b[0] = 0xE0 | (cp >> 12) as u8;
            b[1] = 0x80 | ((cp >> 6) & 0x3F) as u8;
            b[2] = 0x80 | (cp & 0x3F) as u8;
            Some(3)
        }
        _ => {
            let b = out.get_mut(..4)?;
            b[0] = 0xF0 | (cp >> 18) as u8;
            b[1] = 0x80 | ((cp >> 12) & 0x3F) as u8;
            b[2] = 0x80 | ((cp >> 6) & 0x3F) as u8;
            b[3] = 0x80 | (cp & 0x3F) as u8;
            Some(4)
        }
    }
}

/// Renders a `DecimalParts` straight into a `fmt::Write` sink with no heap
/// allocation, unlike its `Display`, which builds two intermediate `String`s.
/// Folds the little-endian 32-bit parts into a `u128` (SQL Server decimals are
/// at most 38 digits) and places the decimal point by digit position.
fn write_decimal<W: std::fmt::Write>(
    w: &mut W,
    d: &mssql_tds::datatypes::decoder::DecimalParts,
) -> std::fmt::Result {
    let value: u128 = d
        .int_parts
        .iter()
        .enumerate()
        .fold(0u128, |acc, (i, &part)| acc + ((part as u32 as u128) << (i * 32)));

    // Most-significant-first ASCII digits on the stack (u128 fits in 39).
    let mut digits = [0u8; 40];
    let n = if value == 0 {
        digits[0] = b'0';
        1
    } else {
        let mut rev = [0u8; 40];
        let mut rn = 0;
        let mut v = value;
        while v > 0 {
            rev[rn] = b'0' + (v % 10) as u8;
            v /= 10;
            rn += 1;
        }
        for i in 0..rn {
            digits[i] = rev[rn - 1 - i];
        }
        rn
    };

    if !d.is_positive {
        w.write_char('-')?;
    }
    let scale = d.scale as usize;
    let s = |a: usize, b: usize| -> &str {
        // All entries are ASCII digits, so this slice is valid UTF-8.
        std::str::from_utf8(&digits[a..b]).unwrap_or("")
    };
    if scale == 0 {
        w.write_str(s(0, n))
    } else if n <= scale {
        w.write_str("0.")?;
        for _ in 0..(scale - n) {
            w.write_char('0')?;
        }
        w.write_str(s(0, n))
    } else {
        let split = n - scale;
        w.write_str(s(0, split))?;
        w.write_char('.')?;
        w.write_str(s(split, n))
    }
}

/// Render a `datetime2` as the canonical `YYYY-MM-DD HH:MM:SS.fffffff` text,
/// matching what msodbcsql18 returns for `SQL_C_CHAR`. `days` is 0-based from
/// 0001-01-01 (proleptic Gregorian); `time_nanoseconds` is in 100 ns ticks.
fn format_datetime2(dt2: &SqlDateTime2) -> String {
    let mut s = String::with_capacity(27);
    let _ = write_datetime2(&mut s, dt2);
    s
}

/// Shared `datetime2` renderer that writes into any `fmt::Write` sink, so both
/// the allocating and zero-heap paths share one formatting implementation.
fn write_datetime2<W: std::fmt::Write>(w: &mut W, dt2: &SqlDateTime2) -> std::fmt::Result {
    let (y, m, d) = civil_from_days(i64::from(dt2.days) - 719_162);
    let ticks = dt2.time.time_nanoseconds;
    let hour = ticks / 36_000_000_000;
    let rem = ticks % 36_000_000_000;
    let minute = rem / 600_000_000;
    let rem = rem % 600_000_000;
    let second = rem / 10_000_000;
    let frac = rem % 10_000_000;
    write!(w, "{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}:{second:02}.{frac:07}")
}

/// Civil date from a day count relative to the Unix epoch (1970-01-01 == 0).
/// Howard Hinnant's algorithm, valid across the full proleptic Gregorian range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_LONG, SQL_NULL_HANDLE};
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::decoder::DecimalParts;
    use mssql_tds::datatypes::sql_string::SqlString;

    fn fast(v: &ColumnValues) -> String {
        let (buf, len) = format_scalar_ascii(v).expect("fast path should handle this scalar");
        String::from_utf8(buf[..len].to_vec()).unwrap()
    }

    #[test]
    fn fast_path_matches_display_for_scalars() {
        // The zero-heap fast path must render byte-identically to the
        // allocating `column_value_to_text` reference for every scalar it claims.
        let cases = [
            ColumnValues::TinyInt(0),
            ColumnValues::TinyInt(255),
            ColumnValues::SmallInt(-12345),
            ColumnValues::Int(0),
            ColumnValues::Int(-2147483648),
            ColumnValues::Int(1467152272),
            ColumnValues::BigInt(9223372036854775807),
            ColumnValues::BigInt(-9223372036854775808),
            ColumnValues::Bit(true),
            ColumnValues::Bit(false),
        ];
        for c in &cases {
            assert_eq!(fast(c), column_value_to_text(c).unwrap(), "mismatch for {c:?}");
        }
    }

    #[test]
    fn fast_path_matches_display_for_decimal() {
        for s in ["0", "1467152272.0000", "-0.0001", "12345.6789", "0.0001"] {
            let (prec, scale) = {
                let frac = s.split('.').nth(1).map(str::len).unwrap_or(0) as u8;
                (38u8, frac)
            };
            let d = DecimalParts::from_string(s, prec, scale).unwrap();
            let v = ColumnValues::Decimal(d);
            assert_eq!(fast(&v), column_value_to_text(&v).unwrap(), "mismatch for {s}");
        }
    }

    #[test]
    fn utf16_fast_path_matches_encoding_rs() {
        // The zero-alloc UTF-16LE → UTF-8 transcode must render byte-identically
        // to the encoding_rs reference in `to_utf8_string()` across BMP, non-ASCII,
        // and astral (surrogate-pair) code points.
        for text in ["", "x", "hello", "éééé", "café \u{2764} au lait", "𝄞𝕏🚀"] {
            let s = SqlString::from_utf8_string(text.to_string());
            let mut out = [0u8; STRING_STACK_CAP];
            let len = utf16le_to_utf8(&s.bytes, &mut out).expect("valid utf16");
            assert_eq!(
                std::str::from_utf8(&out[..len]).unwrap(),
                s.to_utf8_string(),
                "mismatch for {text:?}"
            );
        }
    }

    #[test]
    fn utf16_fast_path_rejects_odd_and_overflow() {
        // Odd byte counts and outputs larger than the sink defer to the
        // allocating path rather than corrupting data.
        assert_eq!(utf16le_to_utf8(&[0x41], &mut [0u8; 16]), None);
        let s = SqlString::from_utf8_string("éééé".to_string());
        assert_eq!(utf16le_to_utf8(&s.bytes, &mut [0u8; 4]), None);
    }

    #[test]
    fn get_data_null_handle() {
        let ret = unsafe {
            sql_get_data(
                SQL_NULL_HANDLE,
                1,
                SQL_C_CHAR,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn get_data_without_cursor_returns_24000() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let mut buf = [0u8; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_string_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                "hello".to_string(),
            ))]);
        }

        let mut buf = [0u8; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 5);
        assert_eq!(std::str::from_utf8(&buf[..5]).unwrap(), "hello");
    }

    #[test]
    fn get_data_truncation_returns_info() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(12345)]);
        }

        let mut buf = [0u8; 3];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_eq!(ind, 5);
    }

    #[test]
    fn get_data_empty_string_zero_buffer_no_truncation() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                String::new(),
            ))]);
        }

        let mut ind: SqlLen = -1;
        let ret = unsafe { sql_get_data(stmt, 1, SQL_C_CHAR, std::ptr::null_mut(), 0, &mut ind) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 0);
    }

    #[test]
    fn get_data_null_column_writes_indicator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Null]);
        }

        let mut buf = [0u8; 4];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
    }

    #[test]
    fn get_data_unsupported_target_type() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(1)]);
        }

        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_LONG,
                (&mut out as *mut i32).cast(),
                std::mem::size_of::<i32>() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_data_invalid_column_index() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(1)]);
        }

        let mut buf = [0u8; 8];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                2,
                SQL_C_CHAR,
                buf.as_mut_ptr() as SqlPointer,
                buf.len() as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    /// Helper: read a NUL-terminated UTF-16 buffer back to a Rust String.
    fn read_until_nul(buf: &[u16]) -> String {
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16(&buf[..len]).unwrap()
    }

    #[test]
    fn get_data_wchar_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::String(SqlString::from_utf8_string(
                "héllo".to_string(),
            ))]);
        }

        let mut buf = [0u16; 16];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        // Indicator is byte length of untruncated value, excluding NUL.
        // "héllo" → 5 u16 units → 10 bytes.
        assert_eq!(ind, 10);
        assert_eq!(read_until_nul(&buf), "héllo");
    }

    #[test]
    fn get_data_wchar_truncation_returns_info() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Int(12345)]);
        }

        // 3 u16 slots = 6 bytes. "12345" needs 6 units (5 chars + NUL) → truncated.
        let mut buf = [0u16; 3];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        // Untruncated byte length: 5 chars × 2 bytes = 10.
        assert_eq!(ind, 10);
        assert_eq!(read_until_nul(&buf), "12");
    }

    #[test]
    fn get_data_wchar_null_column_writes_nul_and_indicator() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = h.stmt;
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        {
            let mut s = stmt_handle.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.current_row = Some(vec![ColumnValues::Null]);
        }

        let mut buf = [0xDEADu16; 4];
        let mut ind: SqlLen = 0;
        let ret = unsafe {
            sql_get_data(
                stmt,
                1,
                SQL_C_WCHAR,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlLen,
                &mut ind,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, SQL_NULL_DATA);
        // First slot must be NUL; nothing else touched.
        assert_eq!(buf[0], 0);
        assert_eq!(&buf[1..], &[0xDEAD; 3]);
    }
}
