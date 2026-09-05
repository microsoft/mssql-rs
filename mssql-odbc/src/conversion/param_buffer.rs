// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The single audited read of application parameter buffers.
//!
//! Every dereference of an application-supplied `ParameterValuePtr` or
//! `StrLen_or_IndPtr` happens here, so the pointer contract has one place to be
//! reviewed. Callers receive an owned [`AppValue`] and never see a raw pointer.

use std::slice;

use super::datetime::DateTimeParts;
use super::param_convert::ParamBuildError;
use crate::api::odbc_types::{
    SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID,
    SQL_C_INTERVAL_MINUTE_TO_SECOND, SQL_C_INTERVAL_YEAR, SQL_C_LONG, SQL_C_NUMERIC, SQL_C_SBIGINT,
    SQL_C_SHORT, SQL_C_SLONG, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SS_VECTOR,
    SQL_C_SSHORT, SQL_C_STINYINT, SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME,
    SQL_C_TYPE_TIMESTAMP, SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_C_WCHAR,
    SQL_DATA_AT_EXEC, SQL_DEFAULT_PARAM, SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_NTS, SQL_NULL_DATA,
    SqlDateStruct, SqlGuid, SqlLen, SqlNumericStruct, SqlPointer, SqlSmallInt, SqlSsTime2Struct,
    SqlSsTimestampoffsetStruct, SqlTimeStruct, SqlTimestampStruct,
};
use crate::api::type_rules::effective_param_c_type;
use crate::params::BoundParam;

/// An application parameter value, copied out of the caller's buffer.
///
/// Covers the C types the conversion matrix currently admits. SQL NULL is not
/// here: [`read_indicator`] settles it before any buffer is read.
///
/// `PartialEq` without `Eq`, because [`AppValue::Double`] carries an `f64`:
/// `Double(NAN) != Double(NAN)`. Fine for the assertion use it has today, but
/// it cannot key a map or back a dedup.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AppValue {
    /// Any integer C type widened to `i128`, which represents every ODBC
    /// integer C type exactly — including `SQL_C_UBIGINT` above `i64::MAX`,
    /// which has no SQL Server target and becomes `22003` downstream.
    Integer(i128),
    /// `SQL_C_CHAR` bytes, as supplied.
    NarrowText(Vec<u8>),
    /// `SQL_C_WCHAR` data, as UTF-16LE bytes.
    WideText(Vec<u8>),
    /// `SQL_C_BINARY` bytes, as supplied.
    Binary(Vec<u8>),
    /// `SQL_C_BIT`, already reduced to the two values `bit` can hold.
    Bit(bool),
    /// `SQL_C_FLOAT`, kept at its own width. Widening to [`AppValue::Double`]
    /// would be lossless but would then face a `real` range check that only a
    /// genuine 64-bit source should meet.
    Float(f32),
    /// `SQL_C_DOUBLE`. A `real` target narrows with a range check.
    Double(f64),
    /// `SQL_C_GUID`, in the `SQLGUID` field layout.
    Guid(SqlGuid),
    /// Any of the five date/time C structs, normalised onto the same calendar
    /// breakdown the fetch direction fills those structs from.
    DateTime(DateTimeParts),
}

/// Byte stride between consecutive values in a column-wise parameter array.
///
/// ODBC ignores `BufferLength` for most fixed-width C types. Character,
/// binary, and SQL Server temporal values use it as the size of one array
/// slot, matching msodbcsql's
/// `BindOffset` default of `dwOffset = lpbindinfo->cbValueMax`
/// (`Sql/Ntdbms/sqlncli/odbc/sqlcfunc.cpp:2280-2283`). Zero therefore makes
/// every row address the same value buffer; a negative width has no usable
/// stride. `SQL_C_SS_VECTOR` is intentionally absent until AB#47790 defines
/// this driver's application-buffer ABI. `c_type` must already be resolved
/// from `SQL_C_DEFAULT` by the binding path.
#[allow(dead_code)] // Consumed by parameter-array execution in AB#47820.
pub(crate) fn parameter_value_stride(c_type: SqlSmallInt, buffer_length: SqlLen) -> Option<usize> {
    let width = match c_type {
        SQL_C_CHAR | SQL_C_WCHAR | SQL_C_BINARY | SQL_C_SS_TIME2 | SQL_C_SS_TIMESTAMPOFFSET => {
            return usize::try_from(buffer_length).ok();
        }
        SQL_C_BIT | SQL_C_TINYINT | SQL_C_UTINYINT => std::mem::size_of::<u8>(),
        SQL_C_STINYINT => std::mem::size_of::<i8>(),
        SQL_C_SHORT | SQL_C_SSHORT => std::mem::size_of::<i16>(),
        SQL_C_USHORT => std::mem::size_of::<u16>(),
        SQL_C_LONG | SQL_C_SLONG => std::mem::size_of::<i32>(),
        SQL_C_ULONG => std::mem::size_of::<u32>(),
        SQL_C_SBIGINT => std::mem::size_of::<i64>(),
        SQL_C_UBIGINT => std::mem::size_of::<u64>(),
        SQL_C_FLOAT => std::mem::size_of::<f32>(),
        SQL_C_DOUBLE => std::mem::size_of::<f64>(),
        SQL_C_TYPE_DATE => std::mem::size_of::<SqlDateStruct>(),
        SQL_C_TYPE_TIME => std::mem::size_of::<SqlTimeStruct>(),
        SQL_C_TYPE_TIMESTAMP => std::mem::size_of::<SqlTimestampStruct>(),
        SQL_C_GUID => std::mem::size_of::<SqlGuid>(),
        SQL_C_NUMERIC => std::mem::size_of::<SqlNumericStruct>(),
        SQL_C_INTERVAL_YEAR..=SQL_C_INTERVAL_MINUTE_TO_SECOND => 28,
        _ => return None,
    };
    Some(width)
}

/// Whether `StrLen_or_Ind` carries a length for this C type.
///
/// The complement of msodbcsql's `IsFixedCType` (`sqlcprot.h:1301`).
fn indicator_is_a_length(c_type: SqlSmallInt) -> bool {
    matches!(
        c_type,
        SQL_C_CHAR | SQL_C_WCHAR | SQL_C_BINARY | SQL_C_SS_VECTOR
    )
}

/// What `StrLen_or_Ind` says about a binding, before its value buffer is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Indicator {
    /// The application bound SQL NULL; the value buffer is not read at all.
    Null,
    /// A length for the character C types, or `SQL_NTS`. Ignored by the
    /// fixed-width types, which take their size from the C type.
    Length(SqlLen),
}

/// Classifies `StrLen_or_Ind` without touching the value buffer, so a caller
/// can reject a binding before any application data is dereferenced.
///
/// Per ODBC's "Deferred Fields" spec, `SQL_DESC_INDICATOR_PTR` and
/// `SQL_DESC_OCTET_LENGTH_PTR` are independent: the indicator carries only
/// `SQL_NULL_DATA` status, while the octet-length pointer carries the length
/// or a data-at-execution sentinel (a null octet-length pointer means "assume
/// NUL-terminated" for a character parameter). `SQLBindParameter` writes the
/// same pointer to both (`BoundParam::write_to_records`), so this reduces to
/// the historical single-pointer check for the common case; a
/// `SQLSetDescFieldW`/`SQLSetDescRec` bind that sets them to different
/// buffers is now read correctly instead of one of the two being discarded.
///
/// # Safety
/// `param.strlen_or_ind_ptr` and `param.octet_length_ptr`, if non-null, must
/// each point to one valid `SqlLen`.
pub(crate) unsafe fn read_indicator(param: &BoundParam) -> Result<Indicator, ParamBuildError> {
    // `SQLBindParameter` aims `SQL_DESC_INDICATOR_PTR` and
    // `SQL_DESC_OCTET_LENGTH_PTR` at one address, so reading NULL from the
    // first and the length from the second agrees unless a separate
    // `SQLSetDescField` splits them - unmeasured against msodbcsql.
    if !param.strlen_or_ind_ptr.is_null() {
        let ind = unsafe { param.strlen_or_ind_ptr.read_unaligned() };
        if ind == SQL_NULL_DATA {
            return Ok(Indicator::Null);
        }
    }

    let indicator = if param.octet_length_ptr.is_null() {
        None
    } else {
        Some(unsafe { param.octet_length_ptr.read_unaligned() })
    };

    if let Some(ind) = indicator {
        if ind == SQL_DEFAULT_PARAM {
            // This value is valid only in a procedure called in ODBC canonical syntax,
            // which this driver does not support yet.
            return Err(ParamBuildError::InvalidUseOfDefaultParam);
        }
        if ind == SQL_DATA_AT_EXEC || ind <= SQL_LEN_DATA_AT_EXEC_OFFSET {
            return Err(ParamBuildError::DataAtExecNotStaged);
        }
        // Past the special values the indicator is a length only for the
        // character types; a fixed-width type takes its size from the C type,
        // so any leftover value is ignored rather than validated.
        if indicator_is_a_length(param.c_type) && ind < 0 && ind != SQL_NTS as SqlLen {
            return Err(ParamBuildError::InvalidLength(ind));
        }
    }

    // A null value buffer with a zero length is the SQLPutData NULL/0
    // convention (`sqlccmd.cpp:4497`) and reaches the wire as NULL - but only
    // for a variable-length C type, which is the only kind that carries a
    // length. Measured on retail 18.6.2.1: `SQL_C_CHAR`/`WCHAR`/`BINARY` with
    // an indicator of 0 execute successfully, while the same C types with a
    // non-zero length or `SQL_NTS`, and every fixed-width C type whatever the
    // indicator, answer `HY090`.
    if param.parameter_value_ptr.is_null() {
        if indicator_is_a_length(param.c_type) && indicator == Some(0) {
            return Ok(Indicator::Null);
        }
        return Err(ParamBuildError::InvalidBufferLength);
    }

    // For the character C types a null octet-length pointer means "null-terminated".
    Ok(Indicator::Length(indicator.unwrap_or(SQL_NTS as SqlLen)))
}

/// Copies the application's value buffer. Call only after [`read_indicator`]
/// has returned [`Indicator::Length`] and the caller has accepted the binding.
///
/// # Safety
/// `param.parameter_value_ptr` must satisfy the `SQLBindParameter` contract:
/// readable for `len_spec` (or to the first NUL when `len_spec` is `SQL_NTS`)
/// for the character types, and for the C type's width otherwise.
pub(crate) unsafe fn read_param_value(
    param: &BoundParam,
    len_spec: SqlLen,
) -> Result<AppValue, ParamBuildError> {
    // Unreachable: `read_indicator` answers every null-buffer shape itself,
    // with `Null` or `InvalidBufferLength`. Kept as an FFI-boundary guard.
    if param.parameter_value_ptr.is_null() {
        return Err(ParamBuildError::NullValuePointer);
    }
    match param.c_type {
        SQL_C_CHAR => Ok(AppValue::NarrowText(unsafe {
            read_char_bytes(param.parameter_value_ptr as *const u8, len_spec)
        })),
        SQL_C_WCHAR => Ok(AppValue::WideText(unsafe {
            read_wchar_bytes(param.parameter_value_ptr as *const u16, len_spec)
        })),
        SQL_C_BINARY => unsafe { read_binary_bytes(param, len_spec) }.map(AppValue::Binary),
        // In msodbcsql, anything non-zero reaches `bit` as 1.
        // Matched here rather than made stricter.
        SQL_C_BIT => Ok(AppValue::Bit(
            unsafe { (param.parameter_value_ptr as *const u8).read_unaligned() } != 0,
        )),
        SQL_C_FLOAT => Ok(AppValue::Float(unsafe {
            (param.parameter_value_ptr as *const f32).read_unaligned()
        })),
        SQL_C_DOUBLE => Ok(AppValue::Double(unsafe {
            (param.parameter_value_ptr as *const f64).read_unaligned()
        })),
        SQL_C_GUID => Ok(AppValue::Guid(unsafe {
            (param.parameter_value_ptr as *const SqlGuid).read_unaligned()
        })),
        SQL_C_TYPE_DATE
        | SQL_C_TYPE_TIME
        | SQL_C_TYPE_TIMESTAMP
        | SQL_C_SS_TIME2
        | SQL_C_SS_TIMESTAMPOFFSET => Ok(AppValue::DateTime(unsafe {
            read_datetime_parts(param.parameter_value_ptr, param.c_type)
        })),
        other => {
            let effective = effective_param_c_type(other, param.sql_type);
            unsafe { read_integer(param.parameter_value_ptr, effective) }
                .map(AppValue::Integer)
                .ok_or(ParamBuildError::UnsupportedCType(other))
        }
    }
}

/// Reads one of the five date/time C structs into the shared calendar
/// breakdown. Nothing is validated here - the converter decides which
/// components its target needs and reports an impossible date itself.
///
/// `scale` stays 0: an application struct carries no declared precision, so the
/// wire scale comes from `DecimalDigits` instead.
///
/// # Safety
/// `ptr` must be non-null and readable for the struct `c_type` names;
/// `read_param_value` has already rejected a null buffer. Reads are unaligned:
/// the ODBC contract does not promise an aligned application buffer.
unsafe fn read_datetime_parts(ptr: SqlPointer, c_type: SqlSmallInt) -> DateTimeParts {
    debug_assert!(!ptr.is_null(), "read_param_value rejects a null buffer");
    let mut p = DateTimeParts::default();
    match c_type {
        SQL_C_TYPE_DATE => {
            let s = unsafe { (ptr as *const SqlDateStruct).read_unaligned() };
            (p.year, p.month, p.day) = (s.year, s.month, s.day);
            p.has_date = true;
        }
        SQL_C_TYPE_TIME => {
            let s = unsafe { (ptr as *const SqlTimeStruct).read_unaligned() };
            (p.hour, p.minute, p.second) = (s.hour, s.minute, s.second);
            p.has_time = true;
        }
        SQL_C_SS_TIME2 => {
            let s = unsafe { (ptr as *const SqlSsTime2Struct).read_unaligned() };
            (p.hour, p.minute, p.second) = (s.hour, s.minute, s.second);
            p.fraction_ns = s.fraction;
            p.has_time = true;
        }
        SQL_C_TYPE_TIMESTAMP => {
            let s = unsafe { (ptr as *const SqlTimestampStruct).read_unaligned() };
            (p.year, p.month, p.day) = (s.year, s.month, s.day);
            (p.hour, p.minute, p.second) = (s.hour, s.minute, s.second);
            p.fraction_ns = s.fraction;
            (p.has_date, p.has_time) = (true, true);
        }
        _ => {
            let s = unsafe { (ptr as *const SqlSsTimestampoffsetStruct).read_unaligned() };
            (p.year, p.month, p.day) = (s.year, s.month, s.day);
            (p.hour, p.minute, p.second) = (s.hour, s.minute, s.second);
            p.fraction_ns = s.fraction;
            (p.tz_hour, p.tz_minute) = (s.timezone_hour, s.timezone_minute);
            (p.has_date, p.has_time, p.has_tz) = (true, true, true);
        }
    }
    p
}

/// Reads a fixed-width integer C buffer, widening to `i128`. `None` for a C
/// type this driver does not read as an integer.
///
/// `SQL_C_TINYINT` is sign-unknown, and reads signed here because that is the
/// rule for every pairing except a same-width `tinyint` transfer. The caller
/// resolves that one case by rewriting the C type to `SQL_C_UTINYINT` before
/// arriving here, so this function never has to decide it - see
/// `type_rules::effective_param_c_type`.
///
/// # Safety
/// `ptr` must be non-null and readable for the C type's width; `read_param_value`
/// has already rejected a null buffer. Reads are unaligned: the ODBC contract
/// does not promise an aligned application buffer.
unsafe fn read_integer(ptr: SqlPointer, c_type: SqlSmallInt) -> Option<i128> {
    debug_assert!(!ptr.is_null(), "read_param_value rejects a null buffer");
    Some(match c_type {
        SQL_C_STINYINT | SQL_C_TINYINT => {
            i128::from(unsafe { (ptr as *const i8).read_unaligned() })
        }
        SQL_C_UTINYINT => i128::from(unsafe { (ptr as *const u8).read_unaligned() }),
        SQL_C_SSHORT | SQL_C_SHORT => i128::from(unsafe { (ptr as *const i16).read_unaligned() }),
        SQL_C_USHORT => i128::from(unsafe { (ptr as *const u16).read_unaligned() }),
        SQL_C_SLONG | SQL_C_LONG => i128::from(unsafe { (ptr as *const i32).read_unaligned() }),
        SQL_C_ULONG => i128::from(unsafe { (ptr as *const u32).read_unaligned() }),
        SQL_C_SBIGINT => i128::from(unsafe { (ptr as *const i64).read_unaligned() }),
        SQL_C_UBIGINT => i128::from(unsafe { (ptr as *const u64).read_unaligned() }),
        _ => return None,
    })
}

/// Reads narrow (`SQL_C_CHAR`) bytes. `len_spec` is a byte count, or `SQL_NTS`
/// for a NUL-terminated string.
///
/// # Safety
/// `ptr` must be non-null and readable for the resolved length (or up to the
/// first NUL when `len_spec == SQL_NTS`); `read_param_value` has already
/// rejected a null buffer, and `read_indicator` every negative `len_spec` other
/// than `SQL_NTS`.
unsafe fn read_char_bytes(ptr: *const u8, len_spec: SqlLen) -> Vec<u8> {
    debug_assert!(!ptr.is_null(), "read_param_value rejects a null buffer");
    debug_assert!(
        len_spec >= 0 || len_spec == SQL_NTS as SqlLen,
        "read_indicator rejects a negative length that is not SQL_NTS"
    );
    let len = if len_spec == SQL_NTS as SqlLen {
        let mut n = 0usize;
        while unsafe { *ptr.add(n) } != 0 {
            n += 1;
        }
        n
    } else {
        len_spec.max(0) as usize
    };
    unsafe { slice::from_raw_parts(ptr, len).to_vec() }
}

/// Reads raw (`SQL_C_BINARY`) bytes.
///
/// Binary buffers have no terminator, so the length must be stated. `len_spec`
/// is the indicator value when the application supplied an indicator pointer;
/// `SQL_NTS` here means it did not, and the ODBC contract falls back to
/// `BufferLength`. A binding that states neither has no readable extent, so it
/// is rejected rather than guessed at.
///
/// # Safety
/// `param.parameter_value_ptr` must be readable for the resolved byte count;
/// `read_param_value` has already rejected a null buffer.
unsafe fn read_binary_bytes(
    param: &BoundParam,
    len_spec: SqlLen,
) -> Result<Vec<u8>, ParamBuildError> {
    debug_assert!(
        !param.parameter_value_ptr.is_null(),
        "read_param_value rejects a null buffer"
    );
    let len = if len_spec == SQL_NTS as SqlLen {
        if param.buffer_length < 0 {
            return Err(ParamBuildError::InvalidLength(param.buffer_length));
        }
        param.buffer_length
    } else {
        len_spec
    };
    if len == 0 {
        return Ok(Vec::new());
    }
    Ok(
        unsafe { slice::from_raw_parts(param.parameter_value_ptr as *const u8, len as usize) }
            .to_vec(),
    )
}

/// Reads wide (`SQL_C_WCHAR`) data as UTF-16LE bytes. `len_spec` is a **byte**
/// count per the ODBC spec, or `SQL_NTS` for a NUL-terminated string.
///
/// # Safety
/// `ptr` must be non-null and readable for the resolved number of `u16` units
/// (or up to the first NUL when `len_spec == SQL_NTS`); `read_param_value` has
/// already rejected a null buffer, and `read_indicator` every negative
/// `len_spec` other than `SQL_NTS`.
unsafe fn read_wchar_bytes(ptr: *const u16, len_spec: SqlLen) -> Vec<u8> {
    debug_assert!(!ptr.is_null(), "read_param_value rejects a null buffer");
    debug_assert!(
        len_spec >= 0 || len_spec == SQL_NTS as SqlLen,
        "read_indicator rejects a negative length that is not SQL_NTS"
    );
    let units = if len_spec == SQL_NTS as SqlLen {
        let mut n = 0usize;
        while unsafe { ptr.add(n).read_unaligned() } != 0 {
            n += 1;
        }
        n
    } else {
        (len_spec.max(0) as usize) / std::mem::size_of::<u16>()
    };
    // Read unit by unit rather than through a slice: a slice reference would
    // assert an alignment the application never promised.
    let mut bytes = Vec::with_capacity(units * std::mem::size_of::<u16>());
    for i in 0..units {
        bytes.extend_from_slice(&unsafe { ptr.add(i).read_unaligned() }.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_DEFAULT, SQL_NO_TOTAL, SQL_PARAM_INPUT, SQL_TINYINT};
    use std::ffi::c_void;

    #[test]
    fn parameter_array_variable_width_strides_use_buffer_length_bytes() {
        for c_type in [
            SQL_C_CHAR,
            SQL_C_WCHAR,
            SQL_C_BINARY,
            SQL_C_SS_TIME2,
            SQL_C_SS_TIMESTAMPOFFSET,
        ] {
            for buffer_length in [0, 1, 7, 2056, 4096] {
                assert_eq!(
                    parameter_value_stride(c_type, buffer_length),
                    Some(buffer_length as usize),
                    "C type {c_type}, BufferLength {buffer_length}",
                );
            }
            assert_eq!(parameter_value_stride(c_type, -1), None);
            assert_eq!(parameter_value_stride(c_type, SqlLen::MIN), None);
        }
    }

    #[test]
    fn parameter_array_fixed_width_strides_ignore_buffer_length() {
        let cases = [
            (SQL_C_BIT, 1),
            (SQL_C_STINYINT, 1),
            (SQL_C_TINYINT, 1),
            (SQL_C_UTINYINT, 1),
            (SQL_C_SHORT, 2),
            (SQL_C_SSHORT, 2),
            (SQL_C_USHORT, 2),
            (SQL_C_LONG, 4),
            (SQL_C_SLONG, 4),
            (SQL_C_ULONG, 4),
            (SQL_C_SBIGINT, 8),
            (SQL_C_UBIGINT, 8),
            (SQL_C_FLOAT, 4),
            (SQL_C_DOUBLE, 8),
            (SQL_C_TYPE_DATE, 6),
            (SQL_C_TYPE_TIME, 6),
            (SQL_C_TYPE_TIMESTAMP, 16),
            (SQL_C_GUID, 16),
            (SQL_C_NUMERIC, 19),
            (SQL_C_INTERVAL_YEAR, 28),
            (SQL_C_INTERVAL_MINUTE_TO_SECOND, 28),
        ];

        for (c_type, expected) in cases {
            for buffer_length in [SqlLen::MIN, -1, 0, 1, SqlLen::MAX] {
                assert_eq!(
                    parameter_value_stride(c_type, buffer_length),
                    Some(expected),
                    "C type {c_type}, BufferLength {buffer_length}",
                );
            }
        }
    }

    #[test]
    fn every_interval_c_type_uses_the_sql_interval_struct_width() {
        for c_type in SQL_C_INTERVAL_YEAR..=SQL_C_INTERVAL_MINUTE_TO_SECOND {
            assert_eq!(parameter_value_stride(c_type, 0), Some(28));
        }
    }

    #[test]
    fn parameter_array_stride_rejects_unresolved_or_unknown_c_types() {
        for c_type in [
            SQL_C_DEFAULT,
            SQL_C_SS_VECTOR,
            0,
            SqlSmallInt::MIN,
            SqlSmallInt::MAX,
        ] {
            assert_eq!(parameter_value_stride(c_type, 8), None, "C type {c_type}");
        }
    }

    fn param(c_type: SqlSmallInt, ptr: *mut c_void, ind: *mut SqlLen) -> BoundParam {
        BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type,
            sql_type: 0,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: ptr,
            buffer_length: 0,
            strlen_or_ind_ptr: ind,
            octet_length_ptr: ind,
        }
    }

    /// msodbcsql's `ParamToSQLType` rewrites `SQL_C_TINYINT` to unsigned when the
    /// SQL type is also `tinyint`, so `0xFF` is 255 rather than -1.
    #[test]
    fn tinyint_c_type_reads_unsigned_against_a_tinyint_parameter() {
        let mut value: u8 = 0xFF;
        let mut ind: SqlLen = 0;
        let mut p = param(
            SQL_C_TINYINT,
            &mut value as *mut u8 as *mut c_void,
            &mut ind,
        );
        p.sql_type = SQL_TINYINT;
        let got = unsafe { read_param_value(&p, 0) }.unwrap();
        assert_eq!(got, AppValue::Integer(255));

        // A wider SQL type keeps the signed read.
        p.sql_type = crate::api::odbc_types::SQL_SMALLINT;
        let got = unsafe { read_param_value(&p, 0) }.unwrap();
        assert_eq!(got, AppValue::Integer(-1));
    }

    /// The two-step read the converter performs: `None` is SQL NULL.
    fn read(p: &BoundParam) -> Result<Option<AppValue>, ParamBuildError> {
        match unsafe { read_indicator(p) }? {
            Indicator::Null => Ok(None),
            Indicator::Length(len) => unsafe { read_param_value(p, len) }.map(Some),
        }
    }

    #[test]
    fn null_indicator_yields_null_regardless_of_c_type() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR, SQL_C_SLONG] {
            let p = param(c_type, std::ptr::null_mut(), &mut ind);
            assert_eq!(read(&p).unwrap(), None);
        }
    }

    #[test]
    fn indicator_values_map_to_their_errors() {
        for (ind_value, expected) in [
            (SQL_DEFAULT_PARAM, ParamBuildError::InvalidUseOfDefaultParam),
            (SQL_DATA_AT_EXEC, ParamBuildError::DataAtExecNotStaged),
            (SQL_NO_TOTAL, ParamBuildError::InvalidLength(SQL_NO_TOTAL)),
        ] {
            let mut ind: SqlLen = ind_value;
            let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
            assert_eq!(read(&p).unwrap_err(), expected);
        }
    }

    #[test]
    fn unsupported_c_type_is_rejected() {
        let mut ind: SqlLen = 4;
        let mut val: [u8; 8] = [0; 8];
        let p = param(SQL_C_SS_VECTOR, val.as_mut_ptr() as *mut c_void, &mut ind);
        let err = read(&p).unwrap_err();
        assert_eq!(err, ParamBuildError::UnsupportedCType(SQL_C_SS_VECTOR));
    }

    /// msodbcsql reads the buffer as one `SCHAR` and widens it like a tinyint
    /// (`sqlccnvt.cpp:5057`), so no byte is rejected and every non-zero one is 1.
    #[test]
    fn bit_reads_any_non_zero_byte_as_one() {
        let mut ind: SqlLen = 0;
        for (byte, expected) in [(0u8, false), (1, true), (2, true), (0xFF, true)] {
            let mut raw = byte;
            let p = param(SQL_C_BIT, (&mut raw as *mut u8).cast(), &mut ind);
            assert_eq!(
                read(&p).unwrap(),
                Some(AppValue::Bit(expected)),
                "byte {byte:#x}"
            );
        }
    }

    /// Each float C type keeps its own width. Widening `SQL_C_FLOAT` here would
    /// be lossless but would hand a `real` target a value it then range-checks
    /// as though something had narrowed.
    #[test]
    fn each_float_c_type_keeps_its_width() {
        let mut ind: SqlLen = 0;
        let mut f: f32 = -2.25;
        let p = param(SQL_C_FLOAT, (&mut f as *mut f32).cast(), &mut ind);
        assert_eq!(read(&p).unwrap(), Some(AppValue::Float(-2.25)));

        let mut d: f64 = 1.5;
        let p = param(SQL_C_DOUBLE, (&mut d as *mut f64).cast(), &mut ind);
        assert_eq!(read(&p).unwrap(), Some(AppValue::Double(1.5)));
    }

    #[test]
    fn guid_is_read_in_its_field_layout() {
        let mut ind: SqlLen = 0;
        let mut g = SqlGuid {
            data1: 0x0123_4567,
            data2: 0x89AB,
            data3: 0xCDEF,
            data4: [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF],
        };
        let p = param(SQL_C_GUID, (&mut g as *mut SqlGuid).cast(), &mut ind);
        assert_eq!(read(&p).unwrap(), Some(AppValue::Guid(g)));
    }

    /// Each of the five date/time structs fills only the components it carries,
    /// so the converter can tell a bare time from a timestamp. Nothing is
    /// validated here - an impossible date reaches the converter intact.
    #[test]
    fn each_datetime_c_struct_fills_only_its_own_components() {
        let mut ind: SqlLen = 0;

        let mut date = SqlDateStruct {
            year: 2024,
            month: 2,
            day: 29,
        };
        let p = param(
            SQL_C_TYPE_DATE,
            (&mut date as *mut SqlDateStruct).cast(),
            &mut ind,
        );
        let Some(AppValue::DateTime(p)) = read(&p).unwrap() else {
            panic!("expected a DateTime value");
        };
        assert_eq!((p.year, p.month, p.day), (2024, 2, 29));
        assert_eq!((p.has_date, p.has_time, p.has_tz), (true, false, false));

        let mut time = SqlTimeStruct {
            hour: 13,
            minute: 45,
            second: 30,
        };
        let p = param(
            SQL_C_TYPE_TIME,
            (&mut time as *mut SqlTimeStruct).cast(),
            &mut ind,
        );
        let Some(AppValue::DateTime(p)) = read(&p).unwrap() else {
            panic!("expected a DateTime value");
        };
        assert_eq!((p.hour, p.minute, p.second, p.fraction_ns), (13, 45, 30, 0));
        assert_eq!((p.has_date, p.has_time, p.has_tz), (false, true, false));

        let mut t2 = SqlSsTime2Struct {
            hour: 13,
            minute: 45,
            second: 30,
            fraction: 123_000_000,
        };
        let p = param(
            SQL_C_SS_TIME2,
            (&mut t2 as *mut SqlSsTime2Struct).cast(),
            &mut ind,
        );
        let Some(AppValue::DateTime(p)) = read(&p).unwrap() else {
            panic!("expected a DateTime value");
        };
        assert_eq!(p.fraction_ns, 123_000_000);
        assert_eq!((p.has_date, p.has_time, p.has_tz), (false, true, false));

        let mut ts = SqlTimestampStruct {
            year: 2024,
            month: 6,
            day: 15,
            hour: 12,
            minute: 30,
            second: 45,
            fraction: 500_000_000,
        };
        let p = param(
            SQL_C_TYPE_TIMESTAMP,
            (&mut ts as *mut SqlTimestampStruct).cast(),
            &mut ind,
        );
        let Some(AppValue::DateTime(p)) = read(&p).unwrap() else {
            panic!("expected a DateTime value");
        };
        assert_eq!(
            (p.year, p.day, p.second, p.fraction_ns),
            (2024, 15, 45, 500_000_000)
        );
        assert_eq!((p.has_date, p.has_time, p.has_tz), (true, true, false));

        let mut dto = SqlSsTimestampoffsetStruct {
            year: 2024,
            month: 6,
            day: 15,
            hour: 12,
            minute: 30,
            second: 0,
            fraction: 0,
            timezone_hour: -5,
            timezone_minute: -30,
        };
        let p = param(
            SQL_C_SS_TIMESTAMPOFFSET,
            (&mut dto as *mut SqlSsTimestampoffsetStruct).cast(),
            &mut ind,
        );
        let Some(AppValue::DateTime(p)) = read(&p).unwrap() else {
            panic!("expected a DateTime value");
        };
        assert_eq!((p.tz_hour, p.tz_minute), (-5, -30));
        assert_eq!((p.has_date, p.has_time, p.has_tz), (true, true, true));

        // An application struct carries no declared precision, so the wire scale
        // comes from `DecimalDigits` rather than from here.
        assert_eq!(p.scale, 0);
    }

    /// An impossible date is carried through rather than rejected here: the
    /// converter owns the `22007`, and a reader that silently normalised would
    /// take that decision away from it.
    #[test]
    fn an_impossible_date_survives_the_read_unchanged() {
        let mut ind: SqlLen = 0;
        let mut date = SqlDateStruct {
            year: 2023,
            month: 2,
            day: 30,
        };
        let p = param(
            SQL_C_TYPE_DATE,
            (&mut date as *mut SqlDateStruct).cast(),
            &mut ind,
        );
        let Some(AppValue::DateTime(p)) = read(&p).unwrap() else {
            panic!("expected a DateTime value");
        };
        assert_eq!((p.year, p.month, p.day), (2023, 2, 30));
    }

    /// The scalar structs are read with `read_unaligned` for the same reason the
    /// integer buffers are: ODBC promises no alignment, and a plain read of a
    /// misaligned struct is UB on every target.
    #[test]
    fn misaligned_scalar_buffers_are_read() {
        #[repr(align(8))]
        struct Backing([u8; 64]);
        let mut backing = Backing([0u8; 64]);
        let mut ind: SqlLen = 0;

        let ts = SqlTimestampStruct {
            year: 2024,
            month: 6,
            day: 15,
            hour: 12,
            minute: 30,
            second: 45,
            fraction: 500_000_000,
        };
        let ptr = unsafe { backing.0.as_mut_ptr().add(1) };
        unsafe { (ptr as *mut SqlTimestampStruct).write_unaligned(ts) };
        let p = param(SQL_C_TYPE_TIMESTAMP, ptr as *mut c_void, &mut ind);
        let Some(AppValue::DateTime(got)) = read(&p).unwrap() else {
            panic!("expected a DateTime value");
        };
        assert_eq!((got.year, got.month, got.day), (2024, 6, 15));
        assert_eq!(got.fraction_ns, 500_000_000);

        let ptr = unsafe { backing.0.as_mut_ptr().add(3) };
        unsafe { (ptr as *mut f64).write_unaligned(1.5) };
        let p = param(SQL_C_DOUBLE, ptr as *mut c_void, &mut ind);
        assert_eq!(read(&p).unwrap(), Some(AppValue::Double(1.5)));
    }

    #[test]
    fn every_integer_c_type_widens_to_its_value() {
        let mut ind: SqlLen = 0;
        let mut i8v: i8 = -5;
        let mut u8v: u8 = 250;
        let mut i16v: i16 = -300;
        let mut u16v: u16 = 60_000;
        let mut i32v: i32 = -70_000;
        let mut u32v: u32 = 4_000_000_000;
        let mut i64v: i64 = i64::MIN;
        let mut u64v: u64 = u64::MAX;

        let cases: [(SqlSmallInt, *mut c_void, i128); 8] = [
            (SQL_C_STINYINT, (&mut i8v as *mut i8).cast(), -5),
            (SQL_C_UTINYINT, (&mut u8v as *mut u8).cast(), 250),
            (SQL_C_SSHORT, (&mut i16v as *mut i16).cast(), -300),
            (SQL_C_USHORT, (&mut u16v as *mut u16).cast(), 60_000),
            (SQL_C_SLONG, (&mut i32v as *mut i32).cast(), -70_000),
            (SQL_C_ULONG, (&mut u32v as *mut u32).cast(), 4_000_000_000),
            (
                SQL_C_SBIGINT,
                (&mut i64v as *mut i64).cast(),
                i128::from(i64::MIN),
            ),
            (
                SQL_C_UBIGINT,
                (&mut u64v as *mut u64).cast(),
                i128::from(u64::MAX),
            ),
        ];

        for (c_type, ptr, expected) in cases {
            let p = param(c_type, ptr, &mut ind);
            assert_eq!(
                read(&p).unwrap(),
                Some(AppValue::Integer(expected)),
                "c_type {c_type}"
            );
        }
    }

    /// `SQL_C_TINYINT` is sign-unknown but reads signed here, because `param`
    /// leaves `sql_type` at 0 and so misses the rewrite in
    /// `effective_param_c_type`; 0xFF is -1. Only `SQL_C_UTINYINT` reads 255.
    /// The rewritten case is
    /// `tinyint_c_type_reads_unsigned_against_a_tinyint_parameter`.
    #[test]
    fn tinyint_is_signed_but_utinyint_is_unsigned() {
        let mut ind: SqlLen = 0;
        let mut raw: u8 = 0xFF;
        let ptr = (&mut raw as *mut u8).cast();

        for c_type in [SQL_C_TINYINT, SQL_C_STINYINT] {
            let p = param(c_type, ptr, &mut ind);
            assert_eq!(
                read(&p).unwrap(),
                Some(AppValue::Integer(-1)),
                "c_type {c_type}"
            );
        }

        let p = param(SQL_C_UTINYINT, ptr, &mut ind);
        assert_eq!(read(&p).unwrap(), Some(AppValue::Integer(255)));
    }

    /// For a fixed-width C type `StrLen_or_Ind` is not a length, so a value that
    /// would be rejected on a character binding is ignored here.
    #[test]
    fn indicator_is_not_a_length_for_fixed_width_c_types() {
        let mut val: i32 = 42;
        let ptr = (&mut val as *mut i32).cast();
        for stray in [-7, 0, 999, SQL_LEN_DATA_AT_EXEC_OFFSET + 1] {
            let mut ind: SqlLen = stray;
            let p = param(SQL_C_SLONG, ptr, &mut ind);
            assert_eq!(
                read(&p).unwrap(),
                Some(AppValue::Integer(42)),
                "indicator {stray} should be ignored"
            );
        }

        // The special values are still checked first, whatever the C type, so
        // the boundary at SQL_LEN_DATA_AT_EXEC_OFFSET applies here too.
        for at_exec in [SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_LEN_DATA_AT_EXEC_OFFSET - 1] {
            let mut ind: SqlLen = at_exec;
            let p = param(SQL_C_SLONG, ptr, &mut ind);
            assert_eq!(
                read(&p).unwrap_err(),
                ParamBuildError::DataAtExecNotStaged,
                "indicator {at_exec}"
            );
        }

        // The same value is still a length error on a character binding.
        let mut ind: SqlLen = -7;
        let p = param(SQL_C_CHAR, ptr, &mut ind);
        assert_eq!(read(&p).unwrap_err(), ParamBuildError::InvalidLength(-7));
    }

    /// The special indicators still apply to a fixed-width binding.
    #[test]
    fn special_indicators_still_apply_to_fixed_width_c_types() {
        let mut val: i32 = 42;
        let ptr = (&mut val as *mut i32).cast();

        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = param(SQL_C_SLONG, ptr, &mut ind);
        assert_eq!(read(&p).unwrap(), None);

        let mut ind: SqlLen = SQL_DEFAULT_PARAM;
        let p = param(SQL_C_SLONG, ptr, &mut ind);
        assert_eq!(
            read(&p).unwrap_err(),
            ParamBuildError::InvalidUseOfDefaultParam
        );
    }

    /// The application picks the indicator's address, so it is read with
    /// `read_unaligned`; a plain dereference here would be UB.
    #[test]
    fn misaligned_indicator_pointer_is_read() {
        #[repr(align(8))]
        struct Backing([u8; 32]);
        let mut backing = Backing([0u8; 32]);

        let ind_ptr = unsafe { backing.0.as_mut_ptr().add(1) } as *mut SqlLen;
        unsafe { ind_ptr.write_unaligned(SQL_NULL_DATA) };
        let p = param(SQL_C_SLONG, std::ptr::null_mut(), ind_ptr);
        assert_eq!(read(&p).unwrap(), None);

        let mut value: i32 = 42;
        unsafe { ind_ptr.write_unaligned(4) };
        let p = param(SQL_C_SLONG, (&mut value as *mut i32).cast(), ind_ptr);
        assert_eq!(read(&p).unwrap(), Some(AppValue::Integer(42)));
    }

    /// `SQL_C_WCHAR` is read unit by unit rather than through a slice, so an
    /// odd-offset buffer must decode the same as an aligned one.
    #[test]
    fn misaligned_wchar_buffer_is_read() {
        #[repr(align(8))]
        struct Backing([u8; 16]);
        let mut backing = Backing([0u8; 16]);
        // "hi" as UTF-16LE, starting one byte in.
        backing.0[1..5].copy_from_slice(&[b'h', 0, b'i', 0]);

        let mut ind: SqlLen = 4;
        let ptr = unsafe { backing.0.as_mut_ptr().add(1) } as *mut c_void;
        let p = param(SQL_C_WCHAR, ptr, &mut ind);
        assert_eq!(
            read(&p).unwrap(),
            Some(AppValue::WideText(vec![b'h', 0, b'i', 0]))
        );
    }

    #[test]
    fn null_indicator_pointer_means_null_terminated() {
        let mut buf: Vec<u8> = b"abc\0".to_vec();
        let p = param(
            SQL_C_CHAR,
            buf.as_mut_ptr() as *mut c_void,
            std::ptr::null_mut(),
        );
        assert_eq!(
            read(&p).unwrap(),
            Some(AppValue::NarrowText(b"abc".to_vec()))
        );
    }

    /// The null-buffer rule, measured on retail 18.6.2.1 across every shape:
    /// a variable-length C type with a zero length is NULL, everything else is
    /// `HY090`.
    #[test]
    fn a_null_value_buffer_follows_the_zero_length_rule() {
        // Variable-length C types with a zero length: SQL NULL.
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR, SQL_C_BINARY] {
            let mut zero: SqlLen = 0;
            let p = param(c_type, std::ptr::null_mut(), &mut zero);
            assert_eq!(read(&p).unwrap(), None, "c_type {c_type}");
        }

        // Same C type, non-zero length: HY090.
        let mut four: SqlLen = 4;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut four);
        assert_eq!(read(&p).unwrap_err().diag().state, *b"HY090");

        // A fixed-width C type never carries a length, so even zero is HY090.
        let mut zero: SqlLen = 0;
        let p = param(SQL_C_SLONG, std::ptr::null_mut(), &mut zero);
        assert_eq!(read(&p).unwrap_err().diag().state, *b"HY090");

        // No indicator at all means SQL_NTS, not zero.
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), std::ptr::null_mut());
        assert_eq!(read(&p).unwrap_err().diag().state, *b"HY090");
    }

    /// Pins the helpers' own length handling. The null-pointer and negative-length
    /// cases are not covered because `read_param_value` and `read_indicator`
    /// reject both before dispatch; the helpers `debug_assert` that contract.
    #[test]
    fn read_char_bytes_edge_cases() {
        let buf = b"abc";
        // Explicit positive length reads exactly that many bytes.
        assert_eq!(unsafe { read_char_bytes(buf.as_ptr(), 3) }, b"abc");
        // A shorter count stops early rather than running to the NUL.
        assert_eq!(unsafe { read_char_bytes(buf.as_ptr(), 2) }, b"ab");
    }

    #[test]
    fn read_wchar_bytes_edge_cases() {
        let units: Vec<u16> = "hi".encode_utf16().chain(std::iter::once(0)).collect();
        // SQL_NTS reads u16 units up to the NUL terminator.
        assert_eq!(
            unsafe { read_wchar_bytes(units.as_ptr(), SQL_NTS as SqlLen) },
            vec![b'h', 0, b'i', 0]
        );
        // An explicit byte count is halved into u16 units.
        assert_eq!(
            unsafe { read_wchar_bytes(units.as_ptr(), 2) },
            vec![b'h', 0]
        );
    }
}
