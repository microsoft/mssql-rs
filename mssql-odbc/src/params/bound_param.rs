// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;

use crate::api::odbc_types::{SqlLen, SqlSmallInt, SqlULen};

/// A bound parameter — the lightweight equivalent of msodbcsql's implicit
/// APD + IPD records (`cmdp.APD`), populated by `SQLBindParameter`.
///
/// ODBC binds parameters **by reference**: the application's value buffer and
/// its length/indicator buffer are read at `SQLExecute` time, not at bind time.
/// The raw pointers are stored here and dereferenced during execution. The
/// caller owns those buffers and must keep them valid (and unchanged in
/// location) until execution completes.
///
/// Some fields (`sql_type`, `column_size`, `decimal_digits`,
/// `buffer_length`, `input_output_type`) form the complete binding descriptor
/// but are not yet read in Phase 1, which maps purely by `c_type`.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct BoundParam {
    /// `SQL_PARAM_INPUT` / `SQL_PARAM_INPUT_OUTPUT` / `SQL_PARAM_OUTPUT`.
    pub(crate) input_output_type: SqlSmallInt,
    /// C data type of the application buffer (ODBC `ValueType`, `SQL_C_*`),
    /// with `SQL_C_DEFAULT` already resolved to a concrete type.
    pub(crate) c_type: SqlSmallInt,
    /// Whether the application bound `SQL_C_DEFAULT`, before `c_type` was
    /// resolved. A defaulted binding describes its value entirely through
    /// `sql_type`, so a NULL is materialised from that rather than from
    /// `c_type` — `SQL_DECIMAL` defaults to `SQL_C_CHAR`, and a NULL `decimal`
    /// is not a NULL `varchar`.
    pub(crate) c_type_defaulted: bool,
    /// SQL data type of the column/expression (ODBC `ParameterType`, `SQL_*`).
    pub(crate) sql_type: SqlSmallInt,
    /// Column size (precision) as passed by the application.
    pub(crate) column_size: SqlULen,
    /// Decimal digits (scale) as passed by the application.
    pub(crate) decimal_digits: SqlSmallInt,
    /// Pointer to the application's value buffer (read at execute time).
    pub(crate) parameter_value_ptr: *mut c_void,
    /// Length in bytes of the application value buffer.
    pub(crate) buffer_length: SqlLen,
    /// Pointer to the application's length/indicator buffer (read at execute
    /// time). May be null.
    pub(crate) strlen_or_ind_ptr: *mut SqlLen,
}

impl BoundParam {
    /// Returns the binding with `SQL_ATTR_PARAM_BIND_OFFSET_PTR` applied.
    ///
    /// ODBC adds the offset, in bytes, to the value pointer and the
    /// length/indicator pointer alike, which lets an application walk a
    /// binding across a buffer without rebinding it. A null pointer stays
    /// null — the offset addresses a buffer that was never supplied, and
    /// offsetting null would turn "no indicator" into a wild pointer.
    pub(crate) fn with_bind_offset(mut self, offset: isize) -> Self {
        if offset == 0 {
            return self;
        }
        if !self.parameter_value_ptr.is_null() {
            self.parameter_value_ptr = self.parameter_value_ptr.wrapping_byte_offset(offset);
        }
        if !self.strlen_or_ind_ptr.is_null() {
            self.strlen_or_ind_ptr = self.strlen_or_ind_ptr.wrapping_byte_offset(offset);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_PARAM_INPUT, SQL_VARCHAR};

    fn param(value: *mut c_void, ind: *mut SqlLen) -> BoundParam {
        BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type: SQL_C_CHAR,
            c_type_defaulted: false,
            sql_type: SQL_VARCHAR,
            column_size: 8,
            decimal_digits: 0,
            parameter_value_ptr: value,
            buffer_length: 8,
            strlen_or_ind_ptr: ind,
        }
    }

    /// The offset moves both pointers by the same byte count, which is what
    /// lets an application step a whole binding across a buffer.
    #[test]
    fn bind_offset_shifts_value_and_indicator_together() {
        let mut buf = [0u8; 32];
        let mut ind: SqlLen = 4;
        let value = buf.as_mut_ptr().cast::<c_void>();
        let shifted = param(value, &raw mut ind).with_bind_offset(16);
        assert_eq!(shifted.parameter_value_ptr as usize, value as usize + 16);
        assert_eq!(
            shifted.strlen_or_ind_ptr as usize,
            (&raw mut ind) as usize + 16
        );
    }

    /// A negative offset is legal: ODBC does not constrain the direction.
    #[test]
    fn bind_offset_may_be_negative() {
        let mut buf = [0u8; 32];
        let value = unsafe { buf.as_mut_ptr().add(16) }.cast::<c_void>();
        let shifted = param(value, std::ptr::null_mut()).with_bind_offset(-16);
        assert_eq!(shifted.parameter_value_ptr as usize, value as usize - 16);
    }

    /// A null indicator means "no indicator supplied", so offsetting it would
    /// manufacture a wild pointer out of the absence of one.
    #[test]
    fn bind_offset_leaves_a_null_indicator_null() {
        let mut buf = [0u8; 32];
        let shifted = param(buf.as_mut_ptr().cast(), std::ptr::null_mut()).with_bind_offset(8);
        assert!(shifted.strlen_or_ind_ptr.is_null());
    }

    /// The overwhelmingly common case, and the one that must stay free.
    #[test]
    fn zero_offset_returns_the_binding_unchanged() {
        let mut buf = [0u8; 32];
        let mut ind: SqlLen = 4;
        let original = param(buf.as_mut_ptr().cast(), &raw mut ind);
        let shifted = original.with_bind_offset(0);
        assert_eq!(
            shifted.parameter_value_ptr as usize,
            original.parameter_value_ptr as usize
        );
        assert_eq!(
            shifted.strlen_or_ind_ptr as usize,
            original.strlen_or_ind_ptr as usize
        );
    }
}
