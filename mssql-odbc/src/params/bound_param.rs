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
    /// C data type of the application buffer (ODBC `ValueType`, `SQL_C_*`).
    pub(crate) c_type: SqlSmallInt,
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
