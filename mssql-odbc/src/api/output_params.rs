// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writing procedure output values back into the application's bound buffers.
//!
//! ODBC makes output parameters and the `{? = call ...}` return status
//! available only once every result set the procedure produced has been
//! consumed, so this runs at batch exhaustion rather than at execute time.
//! Values arrive as TDS `RETURNVALUE` (0xAC) tokens plus the `RETURNSTATUS`
//! (0x79) token, both already collected by `mssql-tds`.

use tracing::debug;

use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::query::result::ReturnValue;
use mssql_tds::token::tokenitems::ReturnValueStatus;

use crate::api::fetch_scroll::{RowIssue, deliver_bound_value};
use crate::api::odbc_types::{SQL_PARAM_INPUT_OUTPUT, SQL_PARAM_OUTPUT, SQL_RETURN_VALUE};
use crate::api::sqlstate::{WARN_STRING_TRUNCATION, post_diag};
use crate::handles::stmt::{ColumnBinding, StmtState};
use crate::params::BoundParam;

/// Copies every output value the server returned into the buffers the
/// application bound, and reports truncation as `01004`.
///
/// Matching follows msodbcsql's `GetReturnValue` (`sqlctokn.cpp`): a returned
/// value is matched to a binding **by name when the server supplied one, and by
/// ordinal otherwise**. A value with no matching binding is dropped rather than
/// written somewhere arbitrary.
///
/// # Safety
/// Every bound output parameter's value and indicator buffers must still be
/// valid, which is the application's obligation until it rebinds or frees the
/// statement.
pub(crate) unsafe fn write_back_output_params(
    stmt_state: &mut StmtState,
    return_values: &[ReturnValue],
    return_status: Option<i32>,
) {
    let returns_status = stmt_state.call_returns_status;
    let bound: Vec<(usize, BoundParam)> = stmt_state
        .bound_params
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.map(|p| (i, p)))
        .filter(|(_, p)| {
            matches!(
                p.input_output_type,
                SQL_PARAM_OUTPUT | SQL_PARAM_INPUT_OUTPUT | SQL_RETURN_VALUE
            )
        })
        .collect();
    if bound.is_empty() {
        return;
    }

    // Output parameters only; a UDF return value is not one of them.
    let outputs: Vec<&ReturnValue> = return_values
        .iter()
        .filter(|v| v.status == ReturnValueStatus::OutputParam)
        .collect();

    let mut truncated = false;
    let mut issues: Vec<RowIssue> = Vec::new();
    let mut consumed = 0usize;
    for (index, param) in bound {
        // The `{? = call ...}` return status comes from the RETURNSTATUS token,
        // not from a RETURNVALUE, and is always an integer. Parameter 1 of that
        // form is the status whatever direction the application bound it with.
        if param.input_output_type == SQL_RETURN_VALUE || (returns_status && index == 0) {
            let Some(status) = return_status else {
                debug!(
                    parameter = index + 1,
                    "no return status was sent for this call"
                );
                continue;
            };
            match unsafe { write_value(&param, &ColumnValues::Int(status)) } {
                Ok(true) => truncated = true,
                Ok(false) => {}
                Err(issue) => issues.push(issue),
            }
            continue;
        }

        let name = format!("@P{}", index + 1);
        let matched = outputs
            .iter()
            .find(|v| !v.param_name.is_empty() && v.param_name.eq_ignore_ascii_case(&name))
            .or_else(|| outputs.get(consumed))
            .copied();

        let Some(value) = matched else {
            debug!(
                parameter = index + 1,
                "no output value was returned for this parameter"
            );
            continue;
        };
        consumed += 1;
        match unsafe { write_value(&param, &value.value) } {
            Ok(true) => truncated = true,
            Ok(false) => {}
            Err(issue) => issues.push(issue),
        }
    }

    if truncated {
        post_diag(stmt_state, WARN_STRING_TRUNCATION);
    }
    for issue in issues {
        issue.post(stmt_state);
    }
}

/// Writes one value into a bound parameter buffer, returning whether it was
/// truncated.
///
/// The parameter binding is adapted to a [`ColumnBinding`] so the delivery goes
/// through exactly the same conversion, indicator and truncation handling that
/// bound columns use on the fetch path; an output parameter is the same problem
/// as a fetched column, only reached from a different token.
unsafe fn write_value(param: &BoundParam, value: &ColumnValues) -> Result<bool, RowIssue> {
    let binding = ColumnBinding {
        column_number: 1,
        target_type: param.c_type,
        target_value_ptr: param.parameter_value_ptr,
        buffer_length: param.buffer_length,
        strlen_or_ind_ptr: param.strlen_or_ind_ptr,
        octet_length_ptr: param.octet_length_ptr,
    };
    unsafe { deliver_bound_value(&binding, value) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_SLONG, SQL_INTEGER, SQL_NULL_DATA, SQL_VARCHAR,
    };
    use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
    use std::ffi::c_void;

    fn output_param(
        c_type: crate::api::odbc_types::SqlSmallInt,
        sql_type: crate::api::odbc_types::SqlSmallInt,
        value: *mut c_void,
        buffer_length: crate::api::odbc_types::SqlLen,
        indicator: *mut crate::api::odbc_types::SqlLen,
    ) -> BoundParam {
        BoundParam {
            input_output_type: SQL_PARAM_OUTPUT,
            c_type,
            sql_type,
            column_size: 0,
            decimal_digits: 0,
            app_precision: 0,
            app_scale: 0,
            precision_scale_explicit: false,
            parameter_value_ptr: value,
            buffer_length,
            strlen_or_ind_ptr: indicator,
            octet_length_ptr: indicator,
        }
    }

    #[test]
    fn an_integer_output_lands_in_the_bound_buffer() {
        let mut buf = 0i32;
        let mut ind: crate::api::odbc_types::SqlLen = -999;
        let param = output_param(
            SQL_C_SLONG,
            SQL_INTEGER,
            (&raw mut buf).cast(),
            size_of::<i32>() as crate::api::odbc_types::SqlLen,
            &raw mut ind,
        );
        let truncated = unsafe { write_value(&param, &ColumnValues::Int(4711)) }.unwrap();
        assert!(!truncated);
        assert_eq!(buf, 4711);
        assert_eq!(ind, size_of::<i32>() as crate::api::odbc_types::SqlLen);
    }

    /// A NULL output must be reported through the indicator, not left as
    /// whatever the buffer happened to hold.
    #[test]
    fn a_null_output_sets_the_indicator() {
        let mut buf = 1234i32;
        let mut ind: crate::api::odbc_types::SqlLen = 0;
        let param = output_param(
            SQL_C_SLONG,
            SQL_INTEGER,
            (&raw mut buf).cast(),
            size_of::<i32>() as crate::api::odbc_types::SqlLen,
            &raw mut ind,
        );
        unsafe { write_value(&param, &ColumnValues::Null) }.unwrap();
        assert_eq!(ind, SQL_NULL_DATA as crate::api::odbc_types::SqlLen);
    }

    /// A NULL with nowhere to report it is an error, not a silent stale value.
    #[test]
    fn a_null_output_without_an_indicator_is_an_error() {
        let mut buf = 0i32;
        let param = output_param(
            SQL_C_SLONG,
            SQL_INTEGER,
            (&raw mut buf).cast(),
            size_of::<i32>() as crate::api::odbc_types::SqlLen,
            std::ptr::null_mut(),
        );
        assert!(unsafe { write_value(&param, &ColumnValues::Null) }.is_err());
    }

    /// An output value too long for the bound buffer truncates and says so, so
    /// the caller can raise 01004.
    #[test]
    fn an_oversized_output_reports_truncation() {
        let mut buf = [0u8; 4];
        let mut ind: crate::api::odbc_types::SqlLen = 0;
        let param = output_param(
            SQL_C_CHAR,
            SQL_VARCHAR,
            buf.as_mut_ptr().cast(),
            buf.len() as crate::api::odbc_types::SqlLen,
            &raw mut ind,
        );
        let value = ColumnValues::String(SqlString::new(b"abcdefgh".to_vec(), EncodingType::Utf8));
        let truncated = unsafe { write_value(&param, &value) }.unwrap();
        assert!(
            truncated,
            "a value longer than the buffer must report truncation"
        );
    }
}
