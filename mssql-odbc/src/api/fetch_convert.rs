// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared fetch conversion core: `ColumnValues` → a requested `SQL_C_*` target
//! buffer.
//!
//! This is the value-level conversion used by both `SQLGetData` (row-by-row)
//! and, later, `SQLBindCol` block fetch (P3). It is deliberately free of any
//! statement-handle or diagnostic-list coupling: it writes the converted value
//! into the caller's buffer and reports success/failure through [`ConvError`],
//! leaving the SQLSTATE posting to the caller (whose diagnostic target differs
//! between the two fetch paths).
//!
//! Scope so far: the fixed-width integer C targets from an integer source
//! column (`tinyint` / `smallint` / `int` / `bigint` / `bit`). Unhandled
//! source/target pairs return [`ConvError::Unsupported`] so callers can fall
//! back to their existing paths (e.g. the character conversion in `get_data`).

use super::odbc_types::{
    SQL_C_BIT, SQL_C_LONG, SQL_C_SBIGINT, SQL_C_SHORT, SQL_C_SLONG, SQL_C_SSHORT, SQL_C_STINYINT,
    SQL_C_TINYINT, SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_SUCCESS, SqlLen,
    SqlPointer, SqlReturn, SqlSmallInt,
};
use super::util::write_if_some;
use mssql_tds::datatypes::column_values::ColumnValues;

/// Why a value-level conversion could not be completed. The caller maps each
/// variant to the appropriate SQLSTATE on its own diagnostic target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConvError {
    /// The value does not fit the requested C type (SQLSTATE `22003`).
    OutOfRange,
    /// This source/target pairing is not handled here (SQLSTATE `HYC00`), and
    /// the caller should try another path or report "not implemented".
    Unsupported,
}

/// Widen any integer-valued column to `i128`, which losslessly holds every
/// integer `ColumnValues` variant. Returns `None` for non-integer sources.
fn integer_source_as_i128(value: &ColumnValues) -> Option<i128> {
    match value {
        ColumnValues::TinyInt(x) => Some(i128::from(*x)),
        ColumnValues::SmallInt(x) => Some(i128::from(*x)),
        ColumnValues::Int(x) => Some(i128::from(*x)),
        ColumnValues::BigInt(x) => Some(i128::from(*x)),
        ColumnValues::Bit(b) => Some(i128::from(*b)),
        _ => None,
    }
}

/// Returns `true` if `target_type` is one of the fixed-width integer C types
/// handled by [`convert_integer_c`]. Lets a caller decide whether to route a
/// request here before it has a value in hand.
pub(crate) fn is_integer_c_target(target_type: SqlSmallInt) -> bool {
    matches!(
        target_type,
        SQL_C_STINYINT
            | SQL_C_TINYINT
            | SQL_C_UTINYINT
            | SQL_C_SSHORT
            | SQL_C_SHORT
            | SQL_C_USHORT
            | SQL_C_SLONG
            | SQL_C_LONG
            | SQL_C_ULONG
            | SQL_C_SBIGINT
            | SQL_C_UBIGINT
            | SQL_C_BIT
    )
}

/// Writes a `Copy` value of type `T` to `ptr` (when non-null) and sets the
/// indicator to `size_of::<T>()`.
///
/// # Safety
/// `ptr`, when non-null, must be valid for a write of `size_of::<T>()` bytes.
/// The write is unaligned-safe. `ind` follows the same contract as
/// [`write_if_some`].
unsafe fn write_fixed<T: Copy>(ptr: SqlPointer, value: T, ind: *mut SqlLen) -> SqlReturn {
    if !ptr.is_null() {
        unsafe { (ptr as *mut T).write_unaligned(value) };
    }
    unsafe { write_if_some(ind, std::mem::size_of::<T>() as SqlLen) };
    SQL_SUCCESS
}

/// Converts an integer column value to a fixed-width integer C target,
/// range-checking against the target type.
///
/// Returns [`ConvError::Unsupported`] when either the source is not an integer
/// column or the target is not a fixed-width integer C type, letting the caller
/// fall back to another conversion path.
///
/// # Safety
/// `target_value_ptr`, when non-null, must be valid for a write of the target
/// C type's size, and `strlen_or_ind_ptr` must be null or valid for a
/// `SqlLen` write.
pub(crate) unsafe fn convert_integer_c(
    value: &ColumnValues,
    target_type: SqlSmallInt,
    target_value_ptr: SqlPointer,
    strlen_or_ind_ptr: *mut SqlLen,
) -> Result<SqlReturn, ConvError> {
    let Some(v) = integer_source_as_i128(value) else {
        return Err(ConvError::Unsupported);
    };

    // Helper: narrow `v` to the target's range or fail with OutOfRange.
    macro_rules! narrow {
        ($ty:ty) => {{ <$ty>::try_from(v).map_err(|_| ConvError::OutOfRange)? }};
    }

    let ret = match target_type {
        // SQL_C_TINYINT is signed per the ODBC C type mapping (same as
        // SQL_C_STINYINT); SQL_C_UTINYINT is the unsigned form.
        SQL_C_STINYINT | SQL_C_TINYINT => unsafe {
            write_fixed(target_value_ptr, narrow!(i8), strlen_or_ind_ptr)
        },
        SQL_C_UTINYINT => unsafe { write_fixed(target_value_ptr, narrow!(u8), strlen_or_ind_ptr) },
        SQL_C_SSHORT | SQL_C_SHORT => unsafe {
            write_fixed(target_value_ptr, narrow!(i16), strlen_or_ind_ptr)
        },
        SQL_C_USHORT => unsafe { write_fixed(target_value_ptr, narrow!(u16), strlen_or_ind_ptr) },
        SQL_C_SLONG | SQL_C_LONG => unsafe {
            write_fixed(target_value_ptr, narrow!(i32), strlen_or_ind_ptr)
        },
        SQL_C_ULONG => unsafe { write_fixed(target_value_ptr, narrow!(u32), strlen_or_ind_ptr) },
        SQL_C_SBIGINT => unsafe { write_fixed(target_value_ptr, narrow!(i64), strlen_or_ind_ptr) },
        SQL_C_UBIGINT => unsafe { write_fixed(target_value_ptr, narrow!(u64), strlen_or_ind_ptr) },
        // A bit target only accepts 0 or 1; any other integer is out of range.
        SQL_C_BIT => {
            let b: u8 = match v {
                0 => 0,
                1 => 1,
                _ => return Err(ConvError::OutOfRange),
            };
            unsafe { write_fixed(target_value_ptr, b, strlen_or_ind_ptr) }
        }
        _ => return Err(ConvError::Unsupported),
    };
    Ok(ret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SqlPointer;

    fn conv(
        v: &ColumnValues,
        target: SqlSmallInt,
        ptr: SqlPointer,
        ind: *mut SqlLen,
    ) -> Result<SqlReturn, ConvError> {
        unsafe { convert_integer_c(v, target, ptr, ind) }
    }

    #[test]
    fn int_to_slong_roundtrip() {
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let ret = conv(
            &ColumnValues::Int(-123456),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, -123456);
        assert_eq!(ind, 4);
    }

    #[test]
    fn tinyint_to_utinyint() {
        let mut out: u8 = 0;
        let mut ind: SqlLen = 0;
        let ret = conv(
            &ColumnValues::TinyInt(200),
            SQL_C_UTINYINT,
            (&mut out as *mut u8).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 200);
        assert_eq!(ind, 1);
    }

    #[test]
    fn bigint_to_sbigint() {
        let mut out: i64 = 0;
        let mut ind: SqlLen = 0;
        conv(
            &ColumnValues::BigInt(i64::MIN),
            SQL_C_SBIGINT,
            (&mut out as *mut i64).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(out, i64::MIN);
        assert_eq!(ind, 8);
    }

    #[test]
    fn bit_true_to_bit() {
        let mut out: u8 = 0xFF;
        let mut ind: SqlLen = 0;
        conv(
            &ColumnValues::Bit(true),
            SQL_C_BIT,
            (&mut out as *mut u8).cast(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(out, 1);
        assert_eq!(ind, 1);
    }

    #[test]
    fn int_out_of_range_for_smallint() {
        let mut out: i16 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Int(40000),
            SQL_C_SSHORT,
            (&mut out as *mut i16).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }

    #[test]
    fn negative_into_unsigned_is_out_of_range() {
        let mut out: u32 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Int(-1),
            SQL_C_ULONG,
            (&mut out as *mut u32).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }

    #[test]
    fn non_integer_source_is_unsupported() {
        let mut out: i32 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Real(1.5),
            SQL_C_SLONG,
            (&mut out as *mut i32).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::Unsupported);
    }

    #[test]
    fn non_integer_target_is_unsupported() {
        let mut out: [u8; 8] = [0; 8];
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Int(1),
            super::super::odbc_types::SQL_C_DOUBLE,
            out.as_mut_ptr().cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::Unsupported);
    }

    #[test]
    fn null_target_pointer_still_sets_indicator() {
        let mut ind: SqlLen = -99;
        let ret = conv(
            &ColumnValues::Int(7),
            SQL_C_SLONG,
            std::ptr::null_mut(),
            &mut ind,
        )
        .unwrap();
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(ind, 4);
    }

    #[test]
    fn bit_out_of_range_rejected() {
        let mut out: u8 = 0;
        let mut ind: SqlLen = 0;
        let err = conv(
            &ColumnValues::Int(2),
            SQL_C_BIT,
            (&mut out as *mut u8).cast(),
            &mut ind,
        )
        .unwrap_err();
        assert_eq!(err, ConvError::OutOfRange);
    }
}
