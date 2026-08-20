// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The single audited read of application parameter buffers.
//!
//! Every dereference of an application-supplied `ParameterValuePtr` or
//! `StrLen_or_IndPtr` happens here, so the pointer contract has one place to be
//! reviewed. Callers receive an owned [`AppValue`] and never see a raw pointer.

use std::slice;

use super::param_convert::ParamBuildError;
use crate::api::odbc_types::{
    SQL_C_BINARY, SQL_C_CHAR, SQL_C_LONG, SQL_C_SBIGINT, SQL_C_SHORT, SQL_C_SLONG, SQL_C_SS_VECTOR,
    SQL_C_SSHORT, SQL_C_STINYINT, SQL_C_TINYINT, SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT,
    SQL_C_UTINYINT, SQL_C_WCHAR, SQL_DATA_AT_EXEC, SQL_DEFAULT_PARAM, SQL_LEN_DATA_AT_EXEC_OFFSET,
    SQL_NTS, SQL_NULL_DATA, SqlLen, SqlPointer, SqlSmallInt,
};
use crate::params::BoundParam;

/// An application parameter value, copied out of the caller's buffer.
///
/// Covers the C types the conversion matrix currently admits. SQL NULL is not
/// here: [`read_indicator`] settles it before any buffer is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppValue {
    /// Any integer C type widened to `i128`, which represents every ODBC
    /// integer C type exactly — including `SQL_C_UBIGINT` above `i64::MAX`,
    /// which has no SQL Server target and becomes `22003` downstream.
    Integer(i128),
    /// `SQL_C_CHAR` bytes, as supplied.
    NarrowText(Vec<u8>),
    /// `SQL_C_WCHAR` data, as UTF-16LE bytes.
    WideText(Vec<u8>),
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
/// # Safety
/// `param.strlen_or_ind_ptr`, if non-null, must point to one valid `SqlLen`.
pub(crate) unsafe fn read_indicator(param: &BoundParam) -> Result<Indicator, ParamBuildError> {
    let indicator = if param.strlen_or_ind_ptr.is_null() {
        None
    } else {
        Some(unsafe { param.strlen_or_ind_ptr.read_unaligned() })
    };

    if let Some(ind) = indicator {
        if ind == SQL_NULL_DATA {
            return Ok(Indicator::Null);
        }
        if ind == SQL_DEFAULT_PARAM {
            // This value is valid only in a procedure called in ODBC canonical syntax,
            // which this driver does not support yet.
            return Err(ParamBuildError::InvalidUseOfDefaultParam);
        }
        if ind == SQL_DATA_AT_EXEC || ind <= SQL_LEN_DATA_AT_EXEC_OFFSET {
            return Err(ParamBuildError::DataAtExecUnsupported);
        }
        // Past the special values the indicator is a length only for the
        // character types; a fixed-width type takes its size from the C type,
        // so any leftover value is ignored rather than validated.
        if indicator_is_a_length(param.c_type) && ind < 0 && ind != SQL_NTS as SqlLen {
            return Err(ParamBuildError::InvalidLength(ind));
        }
    }

    // For the character C types a null indicator pointer means "null-terminated".
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
    // ODBC permits a null `ParameterValuePtr` only for `SQL_NULL_DATA` or
    // data-at-exec, and `read_indicator` has already returned on both.
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
        other => unsafe { read_integer(param.parameter_value_ptr, other) }
            .map(AppValue::Integer)
            .ok_or(ParamBuildError::UnsupportedCType(other)),
    }
}

/// Reads a fixed-width integer C buffer, widening to `i128`. `None` for a C
/// type this driver does not read as an integer, or a null buffer.
///
/// `SQL_C_TINYINT` is the ODBC 2.x spelling and is signed, so it groups with
/// `SQL_C_STINYINT`; only `SQL_C_UTINYINT` is unsigned.
///
/// # Safety
/// `ptr`, if non-null, must be readable for the C type's width. Reads are
/// unaligned: the ODBC contract does not promise an aligned application buffer.
unsafe fn read_integer(ptr: SqlPointer, c_type: SqlSmallInt) -> Option<i128> {
    if ptr.is_null() {
        return None;
    }
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
/// `ptr`, if non-null, must be readable for the resolved length (or up to the
/// first NUL when `len_spec == SQL_NTS`).
unsafe fn read_char_bytes(ptr: *const u8, len_spec: SqlLen) -> Vec<u8> {
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
    unsafe { slice::from_raw_parts(ptr, len).to_vec() }
}

/// Reads wide (`SQL_C_WCHAR`) data as UTF-16LE bytes. `len_spec` is a **byte**
/// count per the ODBC spec, or `SQL_NTS` for a NUL-terminated string.
///
/// # Safety
/// `ptr`, if non-null, must be readable for the resolved number of `u16` units
/// (or up to the first NUL when `len_spec == SQL_NTS`).
unsafe fn read_wchar_bytes(ptr: *const u16, len_spec: SqlLen) -> Vec<u8> {
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
        (len_spec as usize) / std::mem::size_of::<u16>()
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
    use crate::api::odbc_types::{SQL_NO_TOTAL, SQL_PARAM_INPUT};
    use std::ffi::c_void;

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
        }
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
            (SQL_DATA_AT_EXEC, ParamBuildError::DataAtExecUnsupported),
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
        let mut val: f32 = 1.5;
        let p = param(
            crate::api::odbc_types::SQL_C_FLOAT,
            &mut val as *mut f32 as *mut c_void,
            &mut ind,
        );
        let err = read(&p).unwrap_err();
        assert_eq!(
            err,
            ParamBuildError::UnsupportedCType(crate::api::odbc_types::SQL_C_FLOAT)
        );
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

    /// `SQL_C_TINYINT` is the ODBC 2.x spelling of the signed form, so 0xFF is
    /// -1; only `SQL_C_UTINYINT` reads it as 255.
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
        for stray in [-7, 0, 999] {
            let mut ind: SqlLen = stray;
            let p = param(SQL_C_SLONG, ptr, &mut ind);
            assert_eq!(
                read(&p).unwrap(),
                Some(AppValue::Integer(42)),
                "indicator {stray} should be ignored"
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

    /// Both pointers null is the case msodbcsql rejects: without an indicator
    /// the parameter is not NULL, so there is a value to send and nowhere to
    /// read it from.
    #[test]
    fn non_null_parameter_without_a_value_buffer_is_rejected() {
        for c_type in [SQL_C_CHAR, SQL_C_WCHAR, SQL_C_SLONG] {
            let p = param(c_type, std::ptr::null_mut(), std::ptr::null_mut());
            assert_eq!(
                read(&p).unwrap_err(),
                ParamBuildError::NullValuePointer,
                "c_type {c_type}"
            );
        }
        // An explicit length does not make the missing buffer readable either.
        let mut ind: SqlLen = 4;
        let p = param(SQL_C_SLONG, std::ptr::null_mut(), &mut ind);
        let err = read(&p).unwrap_err();
        assert_eq!(err, ParamBuildError::NullValuePointer);
        assert_eq!(err.diag().state, *b"HY009");
    }

    #[test]
    fn read_char_bytes_edge_cases() {
        assert!(unsafe { read_char_bytes(std::ptr::null(), 5) }.is_empty());
        let buf = b"abc";
        // Negative (non-NTS) length yields no bytes.
        assert!(unsafe { read_char_bytes(buf.as_ptr(), -5) }.is_empty());
        // Explicit positive length reads exactly that many bytes.
        assert_eq!(unsafe { read_char_bytes(buf.as_ptr(), 3) }, b"abc");
    }

    #[test]
    fn read_wchar_bytes_edge_cases() {
        assert!(unsafe { read_wchar_bytes(std::ptr::null(), 5) }.is_empty());
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
