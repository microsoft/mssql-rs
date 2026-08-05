// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reading an application parameter buffer into a normalized [`CValue`].
//!
//! `SQLBindParameter` hands the driver a raw pointer plus a `SQL_C_*` tag. This
//! module decodes that buffer once, independently of the target SQL type, so the
//! C-type → SQL-type matrix in [`super::convert`] only has to reason about
//! normalized values.

use std::slice;

use crate::api::odbc_types::{
    SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DATE, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID,
    SQL_C_LONG, SQL_C_NUMERIC, SQL_C_SBIGINT, SQL_C_SHORT, SQL_C_SLONG, SQL_C_SS_TIME2,
    SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT, SQL_C_STINYINT, SQL_C_TIME, SQL_C_TIMESTAMP,
    SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_TYPE_TIMESTAMP, SQL_C_UBIGINT,
    SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_C_WCHAR, SQL_NTS, SqlDateStruct, SqlGuid,
    SqlLen, SqlNumericStruct, SqlSmallInt, SqlSsTime2Struct, SqlSsTimestampoffsetStruct,
    SqlTimeStruct, SqlTimestampStruct,
};

/// A parameter buffer decoded according to its `SQL_C_*` type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CValue {
    /// Character data from `SQL_C_CHAR` / `SQL_C_WCHAR`, already decoded to
    /// Rust `String`. `wide` records which of the two it came from.
    Text {
        text: String,
        wide: bool,
    },
    /// `SQL_C_BINARY`.
    Bytes(Vec<u8>),
    /// Any signed integer C type, plus `SQL_C_BIT`.
    Int(i64),
    /// `SQL_C_UBIGINT` values that do not fit in `i64`.
    UInt(u64),
    /// `SQL_C_FLOAT` / `SQL_C_DOUBLE`.
    Float(f64),
    /// `SQL_C_BIT`.
    Bool(bool),
    Date(SqlDateStruct),
    /// Hour/minute/second plus nanoseconds (`SQL_C_TYPE_TIME`, `SQL_C_SS_TIME2`).
    Time {
        hour: u16,
        minute: u16,
        second: u16,
        nanos: u32,
    },
    Timestamp(SqlTimestampStruct),
    TimestampOffset(SqlSsTimestampoffsetStruct),
    Numeric(SqlNumericStruct),
    Guid(SqlGuid),
}

impl CValue {
    /// Renders the value as text for character SQL targets.
    pub(crate) fn to_text(&self) -> String {
        match self {
            Self::Text { text, .. } => text.clone(),
            Self::Bytes(b) => b.iter().map(|byte| format!("{byte:02X}")).collect(),
            Self::Int(v) => v.to_string(),
            Self::UInt(v) => v.to_string(),
            Self::Float(v) => format_float(*v),
            Self::Bool(v) => u8::from(*v).to_string(),
            Self::Date(d) => format!("{:04}-{:02}-{:02}", d.year, d.month, d.day),
            Self::Time {
                hour,
                minute,
                second,
                nanos,
            } => format_time(*hour, *minute, *second, *nanos),
            Self::Timestamp(t) => format!(
                "{:04}-{:02}-{:02} {}",
                t.year,
                t.month,
                t.day,
                format_time(t.hour, t.minute, t.second, t.fraction)
            ),
            Self::TimestampOffset(t) => format!(
                "{:04}-{:02}-{:02} {} {}{:02}:{:02}",
                t.year,
                t.month,
                t.day,
                format_time(t.hour, t.minute, t.second, t.fraction),
                if t.timezone_hour < 0 || t.timezone_minute < 0 {
                    '-'
                } else {
                    '+'
                },
                t.timezone_hour.abs(),
                t.timezone_minute.abs()
            ),
            Self::Numeric(n) => numeric_to_string(n),
            Self::Guid(g) => guid_to_string(g),
        }
    }
}

/// Formats a float the way msodbcsql renders it into a character buffer:
/// shortest round-trippable form, without a trailing `.0` for integral values.
fn format_float(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn format_time(hour: u16, minute: u16, second: u16, nanos: u32) -> String {
    if nanos == 0 {
        format!("{hour:02}:{minute:02}:{second:02}")
    } else {
        let frac = format!("{nanos:09}");
        format!(
            "{hour:02}:{minute:02}:{second:02}.{}",
            frac.trim_end_matches('0')
        )
    }
}

fn guid_to_string(g: &SqlGuid) -> String {
    let mut tail = String::new();
    for (i, b) in g.data4.iter().enumerate() {
        if i == 2 {
            tail.push('-');
        }
        tail.push_str(&format!("{b:02X}"));
    }
    format!("{:08X}-{:04X}-{:04X}-{}", g.data1, g.data2, g.data3, tail)
}

/// Renders `SQL_NUMERIC_STRUCT` as a decimal literal.
pub(crate) fn numeric_to_string(n: &SqlNumericStruct) -> String {
    let mut mantissa: u128 = 0;
    for (i, b) in n.val.iter().enumerate().take(16) {
        mantissa |= u128::from(*b) << (8 * i);
    }
    let digits = mantissa.to_string();
    let scale = n.scale.max(0) as usize;
    let body = if scale == 0 {
        digits
    } else if digits.len() > scale {
        let split = digits.len() - scale;
        format!("{}.{}", &digits[..split], &digits[split..])
    } else {
        format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
    };
    if n.sign == 0 && mantissa != 0 {
        format!("-{body}")
    } else {
        body
    }
}

/// Reads the application buffer for `c_type`.
///
/// Returns `None` when the C type is not recognized.
///
/// # Safety
/// `ptr` must be readable for the size implied by `c_type` (or for `len_spec`
/// bytes for the variable-length types), per the ODBC binding contract.
pub(crate) unsafe fn read_c_value(
    c_type: SqlSmallInt,
    ptr: *const u8,
    len_spec: SqlLen,
    buffer_length: SqlLen,
) -> Option<CValue> {
    if ptr.is_null() {
        return match c_type {
            SQL_C_CHAR => Some(CValue::Text {
                text: String::new(),
                wide: false,
            }),
            SQL_C_WCHAR => Some(CValue::Text {
                text: String::new(),
                wide: true,
            }),
            SQL_C_BINARY => Some(CValue::Bytes(Vec::new())),
            _ => None,
        };
    }

    let value = match c_type {
        SQL_C_CHAR => {
            let bytes = unsafe { read_char_bytes(ptr, len_spec) };
            CValue::Text {
                text: crate::api::ansi::decode(&bytes),
                wide: false,
            }
        }
        SQL_C_WCHAR => {
            let units = unsafe { read_wchar_units(ptr as *const u16, len_spec) };
            CValue::Text {
                text: String::from_utf16_lossy(&units),
                wide: true,
            }
        }
        SQL_C_BINARY => {
            let len = if len_spec >= 0 {
                len_spec as usize
            } else if buffer_length > 0 {
                buffer_length as usize
            } else {
                0
            };
            CValue::Bytes(unsafe { slice::from_raw_parts(ptr, len) }.to_vec())
        }
        SQL_C_BIT => CValue::Bool(unsafe { *ptr } != 0),
        SQL_C_TINYINT | SQL_C_STINYINT => CValue::Int(i64::from(unsafe { *(ptr as *const i8) })),
        SQL_C_UTINYINT => CValue::Int(i64::from(unsafe { *ptr })),
        SQL_C_SHORT | SQL_C_SSHORT => CValue::Int(i64::from(unsafe { read::<i16>(ptr) })),
        SQL_C_USHORT => CValue::Int(i64::from(unsafe { read::<u16>(ptr) })),
        SQL_C_LONG | SQL_C_SLONG => CValue::Int(i64::from(unsafe { read::<i32>(ptr) })),
        SQL_C_ULONG => CValue::Int(i64::from(unsafe { read::<u32>(ptr) })),
        SQL_C_SBIGINT => CValue::Int(unsafe { read::<i64>(ptr) }),
        SQL_C_UBIGINT => {
            let v = unsafe { read::<u64>(ptr) };
            match i64::try_from(v) {
                Ok(i) => CValue::Int(i),
                Err(_) => CValue::UInt(v),
            }
        }
        SQL_C_FLOAT => CValue::Float(f64::from(unsafe { read::<f32>(ptr) })),
        SQL_C_DOUBLE => CValue::Float(unsafe { read::<f64>(ptr) }),
        SQL_C_NUMERIC => CValue::Numeric(unsafe { read::<SqlNumericStruct>(ptr) }),
        SQL_C_GUID => CValue::Guid(unsafe { read::<SqlGuid>(ptr) }),
        SQL_C_DATE | SQL_C_TYPE_DATE => CValue::Date(unsafe { read::<SqlDateStruct>(ptr) }),
        SQL_C_TIME | SQL_C_TYPE_TIME => {
            let t = unsafe { read::<SqlTimeStruct>(ptr) };
            CValue::Time {
                hour: t.hour,
                minute: t.minute,
                second: t.second,
                nanos: 0,
            }
        }
        SQL_C_SS_TIME2 => {
            let t = unsafe { read::<SqlSsTime2Struct>(ptr) };
            CValue::Time {
                hour: t.hour,
                minute: t.minute,
                second: t.second,
                nanos: t.fraction,
            }
        }
        SQL_C_TIMESTAMP | SQL_C_TYPE_TIMESTAMP => {
            CValue::Timestamp(unsafe { read::<SqlTimestampStruct>(ptr) })
        }
        SQL_C_SS_TIMESTAMPOFFSET => {
            CValue::TimestampOffset(unsafe { read::<SqlSsTimestampoffsetStruct>(ptr) })
        }
        _ => return None,
    };
    Some(value)
}

/// Reads a `#[repr(C)]` POD from a possibly unaligned application buffer.
///
/// # Safety
/// `ptr` must be readable for `size_of::<T>()` bytes.
unsafe fn read<T: Copy>(ptr: *const u8) -> T {
    unsafe { (ptr as *const T).read_unaligned() }
}

/// Reads narrow bytes. `len_spec` is a byte count, or `SQL_NTS` for a
/// NUL-terminated string.
///
/// # Safety
/// `ptr` must be readable for the resolved length (or up to the first NUL when
/// `len_spec == SQL_NTS`).
pub(crate) unsafe fn read_char_bytes(ptr: *const u8, len_spec: SqlLen) -> Vec<u8> {
    if ptr.is_null() {
        return Vec::new();
    }
    let len = if len_spec == SQL_NTS as SqlLen {
        let mut n = 0usize;
        while unsafe { *ptr.add(n) } != 0 {
            n += 1;
        }
        n
    } else if len_spec < 0 {
        0
    } else {
        len_spec as usize
    };
    unsafe { slice::from_raw_parts(ptr, len) }.to_vec()
}

/// Reads wide data as UTF-16 code units. `len_spec` is a **byte** count per the
/// ODBC spec, or `SQL_NTS` for a NUL-terminated string.
///
/// # Safety
/// `ptr` must be readable for the resolved number of `u16` units (or up to the
/// first NUL when `len_spec == SQL_NTS`).
pub(crate) unsafe fn read_wchar_units(ptr: *const u16, len_spec: SqlLen) -> Vec<u16> {
    if ptr.is_null() {
        return Vec::new();
    }
    let units = if len_spec == SQL_NTS as SqlLen {
        let mut n = 0usize;
        while unsafe { ptr.add(n).read_unaligned() } != 0 {
            n += 1;
        }
        n
    } else if len_spec < 0 {
        0
    } else {
        (len_spec as usize) / size_of::<u16>()
    };
    (0..units)
        .map(|i| unsafe { ptr.add(i).read_unaligned() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_signed_bigint() {
        let v: i64 = -42;
        let got = unsafe { read_c_value(SQL_C_SBIGINT, &v as *const i64 as *const u8, 8, 8) };
        assert_eq!(got, Some(CValue::Int(-42)));
    }

    #[test]
    fn reads_tinyint_as_signed() {
        let v: i8 = -3;
        let got = unsafe { read_c_value(SQL_C_TINYINT, &v as *const i8 as *const u8, 1, 1) };
        assert_eq!(got, Some(CValue::Int(-3)));
    }

    #[test]
    fn reads_bit() {
        let v: u8 = 1;
        let got = unsafe { read_c_value(SQL_C_BIT, &v as *const u8, 1, 1) };
        assert_eq!(got, Some(CValue::Bool(true)));
    }

    #[test]
    fn reads_double() {
        let v: f64 = 1.5;
        let got = unsafe { read_c_value(SQL_C_DOUBLE, &v as *const f64 as *const u8, 8, 8) };
        assert_eq!(got, Some(CValue::Float(1.5)));
    }

    #[test]
    fn reads_binary_using_indicator() {
        let buf = [1u8, 2, 3, 4];
        let got = unsafe { read_c_value(SQL_C_BINARY, buf.as_ptr(), 3, 4) };
        assert_eq!(got, Some(CValue::Bytes(vec![1, 2, 3])));
    }

    #[test]
    fn unknown_c_type_is_none() {
        let v: u8 = 0;
        assert!(unsafe { read_c_value(12345, &v as *const u8, 1, 1) }.is_none());
    }

    #[test]
    fn numeric_struct_renders_scaled_decimal() {
        let mut n = SqlNumericStruct {
            precision: 10,
            scale: 2,
            sign: 1,
            val: [0; 16],
        };
        n.val[0] = 0xD2; // 1234 -> "12.34"
        n.val[1] = 0x04;
        assert_eq!(numeric_to_string(&n), "12.34");
        n.sign = 0;
        assert_eq!(numeric_to_string(&n), "-12.34");
    }

    #[test]
    fn numeric_struct_pads_small_mantissa() {
        let mut n = SqlNumericStruct {
            precision: 10,
            scale: 4,
            sign: 1,
            val: [0; 16],
        };
        n.val[0] = 5;
        assert_eq!(numeric_to_string(&n), "0.0005");
    }

    #[test]
    fn guid_renders_canonical_text() {
        let g = SqlGuid {
            data1: 0x0123_4567,
            data2: 0x89AB,
            data3: 0xCDEF,
            data4: [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF],
        };
        assert_eq!(guid_to_string(&g), "01234567-89AB-CDEF-0123-456789ABCDEF");
    }

    #[test]
    fn float_text_drops_trailing_zero() {
        assert_eq!(format_float(3.0), "3");
        assert_eq!(format_float(3.25), "3.25");
    }

    #[test]
    fn time_text_trims_fraction() {
        assert_eq!(format_time(1, 2, 3, 0), "01:02:03");
        assert_eq!(format_time(1, 2, 3, 500_000_000), "01:02:03.5");
    }
}
