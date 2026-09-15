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

use crate::api::fetch_scroll::{RowOutcome, deliver_bound_value};
use crate::api::odbc_types::{
    SQL_ATTR_PARAM_BIND_TYPE, SQL_BIND_BY_COLUMN, SQL_ERROR, SQL_PARAM_INPUT_OUTPUT,
    SQL_PARAM_OUTPUT, SQL_RETURN_VALUE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SqlReturn,
};
use crate::api::sqlstate::{ERR_INVALID_STRING_OR_BUFFER_LENGTH, post_diag};
use crate::handles::stmt::{ColumnBinding, StmtState};
use crate::params::BoundParam;

/// Copies every output value the server returned into the buffers the
/// application currently bound, preserving conversion diagnostics and the
/// aggregate return code.
///
/// Matching follows msodbcsql's `GetReturnValue` (`sqlctokn.cpp`): a returned
/// value is matched to a binding **by name when the server supplied one, and by
/// ordinal otherwise**. A value with no matching binding is dropped rather than
/// written somewhere arbitrary.
///
/// # Safety
/// Every bound output parameter's value and indicator buffers must still be
/// writable at the current parameter binding offset. `bound_params` must be a
/// fresh effective APD/IPD snapshot, taken before acquiring the STMT lock, not
/// the execution-time input snapshot. Its `ParameterSnapshot` lease must remain
/// alive until every value and indicator write has completed.
pub(crate) unsafe fn write_back_output_params(
    stmt_state: &mut StmtState,
    bound_params: &[Option<BoundParam>],
    return_values: &[ReturnValue],
    return_status: Option<i32>,
) -> SqlReturn {
    let returns_status = stmt_state.call_returns_status;
    let bound = bound_params
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.map(|p| (i, p)))
        .filter(|(_, p)| {
            matches!(
                p.input_output_type,
                SQL_PARAM_OUTPUT | SQL_PARAM_INPUT_OUTPUT | SQL_RETURN_VALUE
            )
        });
    // The application can change the pointed-to offset after execution.
    let bind_offset = unsafe { stmt_state.inert_attrs.param_bind_offset() };
    let bind_type = stmt_state
        .inert_attrs
        .get(SQL_ATTR_PARAM_BIND_TYPE)
        .unwrap_or(SQL_BIND_BY_COLUMN);

    // Output parameters only; a UDF return value is not one of them.
    let outputs: Vec<&ReturnValue> = return_values
        .iter()
        .filter(|v| v.status == ReturnValueStatus::OutputParam)
        .collect();

    let mut result = SQL_SUCCESS;
    for (index, param) in bound {
        let Ok(param) = param.for_row(0, bind_offset, bind_type) else {
            post_diag(stmt_state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
            result = SQL_ERROR;
            continue;
        };
        // Only a direct RPC uses RETURNSTATUS. Text/prepared calls return
        // their status variable as @P1, like every other output binding.
        let value = if returns_status && index == 0 {
            let Some(status) = return_status else {
                debug!(
                    parameter = index + 1,
                    "no return status was sent for this call"
                );
                continue;
            };
            &ColumnValues::Int(status)
        } else {
            let name = format!("@P{}", index + 1);
            let matched = outputs.iter().find(|v| {
                if v.param_name.is_empty() {
                    usize::from(v.param_ordinal) == index - usize::from(returns_status)
                } else {
                    v.param_name.eq_ignore_ascii_case(&name)
                }
            });
            let Some(value) = matched else {
                debug!(
                    parameter = index + 1,
                    "no output value was returned for this parameter"
                );
                continue;
            };
            &value.value
        };
        match unsafe { write_value(&param, value) } {
            RowOutcome::Success => {}
            RowOutcome::Info(issue) => {
                issue.post(stmt_state);
                if result == SQL_SUCCESS {
                    result = SQL_SUCCESS_WITH_INFO;
                }
            }
            RowOutcome::Error(issue) => {
                issue.post(stmt_state);
                result = SQL_ERROR;
            }
        }
    }
    result
}

/// Writes one value into a bound parameter buffer.
///
/// The parameter binding is adapted to a [`ColumnBinding`] so the delivery goes
/// through exactly the same conversion, indicator and truncation handling that
/// bound columns use on the fetch path; an output parameter is the same problem
/// as a fetched column, only reached from a different token.
///
/// # Safety
/// The parameter's value, indicator, and octet-length buffers must be writable
/// for one element according to its bound C type and buffer length.
unsafe fn write_value(param: &BoundParam, value: &ColumnValues) -> RowOutcome {
    let binding = ColumnBinding {
        column_number: 1,
        target_type: param.c_type,
        target_value_ptr: param.parameter_value_ptr,
        buffer_length: if param.parameter_value_ptr.is_null() {
            0
        } else {
            param.buffer_length
        },
        strlen_or_ind_ptr: param.strlen_or_ind_ptr,
        octet_length_ptr: param.octet_length_ptr,
    };
    unsafe { deliver_bound_value(&binding, value) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::bind_param::{sql_bind_parameter, sql_free_stmt_reset_params};
    use crate::api::exec_common::snapshot_bound_params;
    use crate::api::more_results::sql_more_results;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_SLONG, SQL_INTEGER, SQL_NO_DATA, SQL_NULL_DATA, SQL_VARCHAR,
    };
    use crate::handles::{DbcHandle, StmtHandle, handle_from_raw};
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
    use mssql_tds::test_client_support::{int_columns, tds_client_from_tokens};
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

    fn returned(name: &str, ordinal: u16, value: ColumnValues) -> ReturnValue {
        ReturnValue {
            param_ordinal: ordinal,
            param_name: name.to_owned(),
            value,
            column_metadata: Box::new(int_columns(1).remove(0)),
            status: ReturnValueStatus::OutputParam,
        }
    }

    #[test]
    fn indicator_only_output_never_writes_a_null_destination() {
        let mut length = -2;
        let integer = output_param(
            SQL_C_SLONG,
            SQL_INTEGER,
            std::ptr::null_mut(),
            128,
            &raw mut length,
        );
        assert!(matches!(
            unsafe { write_value(&integer, &ColumnValues::Int(73)) },
            RowOutcome::Success
        ));
        assert_eq!(length, 4);
        assert!(matches!(
            unsafe { write_value(&integer, &ColumnValues::Null) },
            RowOutcome::Success
        ));
        assert_eq!(length, crate::api::odbc_types::SQL_NULL_DATA);
        let text = output_param(
            SQL_C_CHAR,
            SQL_VARCHAR,
            std::ptr::null_mut(),
            128,
            &raw mut length,
        );
        assert!(matches!(
            unsafe { write_value(&text, &ColumnValues::Int(73)) },
            RowOutcome::Info(crate::api::fetch_scroll::RowIssue::StringTruncated)
        ));
        assert_eq!(length, 2);
        assert!(matches!(
            unsafe { write_value(&text, &ColumnValues::Null) },
            RowOutcome::Success
        ));
        assert_eq!(length, crate::api::odbc_types::SQL_NULL_DATA);
    }

    fn bind(h: &TestHandles, param: BoundParam) {
        assert_eq!(
            unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    param.input_output_type,
                    param.c_type,
                    param.sql_type,
                    8,
                    0,
                    param.parameter_value_ptr,
                    param.buffer_length,
                    param.strlen_or_ind_ptr,
                )
            },
            SQL_SUCCESS
        );
    }

    #[test]
    fn pending_outputs_use_current_bindings_and_survive_another_busy_statement() {
        for reset in [false, true] {
            let mut h = TestHandles::with_env_dbc_stmt();
            let other = h.alloc_extra_stmt();
            let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
            let stmt = &*stmt_owner;
            let dbc_owner = handle_from_raw::<DbcHandle>(h.dbc).unwrap().into_arc();
            let dbc = &*dbc_owner;
            let mut old = -1i32;
            let mut new = -2i32;
            let old_param = output_param(
                SQL_C_SLONG,
                SQL_INTEGER,
                (&raw mut old).cast(),
                0,
                std::ptr::null_mut(),
            );
            bind(&h, old_param);
            let stale = snapshot_bound_params(stmt).unwrap().records;
            {
                let mut state = stmt.inner.lock().unwrap();
                state.bound_params = stale;
                state.batch_exhausted = true;
                state.pending_output_params =
                    Some((vec![returned("@P1", 0, ColumnValues::Int(73))], None));
            }
            {
                let mut state = dbc.inner.lock().unwrap();
                state.active_stmt = Some(other);
                state.client = Some(tds_client_from_tokens(Vec::new()));
            }
            if reset {
                assert_eq!(unsafe { sql_free_stmt_reset_params(h.stmt) }, SQL_SUCCESS);
            } else {
                bind(
                    &h,
                    BoundParam {
                        parameter_value_ptr: (&raw mut new).cast(),
                        ..old_param
                    },
                );
            }
            assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_NO_DATA);
            assert_eq!(old, -1);
            assert_eq!(new, if reset { -2 } else { 73 });
            assert_eq!(dbc.inner.lock().unwrap().active_stmt, Some(other));
            new = -3;
            assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_NO_DATA);
            assert_eq!(new, -3);
            assert!(stmt.inner.lock().unwrap().pending_output_params.is_none());
        }
    }

    #[test]
    fn pending_output_writes_hold_binding_leases_until_delivery_finishes() {
        use crate::api::bind_col::sql_bind_col;
        use crate::api::odbc_types::{
            SQL_ATTR_APP_PARAM_DESC, SQL_ATTR_APP_ROW_DESC, SQL_ATTR_PARAM_BIND_OFFSET_PTR,
            SQL_DESC_DATA_PTR, SQL_SUCCESS,
        };
        use crate::api::set_desc_field::sql_set_desc_field_w;
        use crate::api::set_stmt_attr::sql_set_stmt_attr_w;
        use crate::handles::DescHandle;
        use crate::handles::bindings::snapshot_test_hook::{self, Phase};
        use std::sync::mpsc;
        use std::time::Duration;

        let mut h = TestHandles::with_env_dbc_stmt();
        let other = h.alloc_extra_stmt();
        let desc_raw = h.alloc_explicit_desc();
        let stmt = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let desc = handle_from_raw::<DescHandle>(desc_raw).unwrap().into_arc();
        assert_eq!(
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_APP_PARAM_DESC, desc_raw, 0) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe { sql_set_stmt_attr_w(other, SQL_ATTR_APP_ROW_DESC, desc_raw, 0) },
            SQL_SUCCESS
        );
        let mut values = [91_i32, -1, 92];
        let mut indicators = [93_isize, -1, 94];
        let mut replacement = -2_i32;
        bind(
            &h,
            output_param(
                SQL_C_SLONG,
                SQL_INTEGER,
                (&raw mut values[1]).cast(),
                4,
                &raw mut indicators[1],
            ),
        );
        {
            let mut state = stmt.inner.lock().unwrap();
            state.batch_exhausted = true;
            state.pending_output_params =
                Some((vec![returned("@P1", 0, ColumnValues::Int(73))], None));
        }
        let (arrived, ready) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        let _hook = snapshot_test_hook::install(&stmt, Phase::Parameters, move || {
            arrived.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        let id = h.stmt.addr();
        std::thread::scope(|scope| {
            let delivery = scope
                .spawn(move || unsafe { sql_more_results(std::ptr::without_provenance_mut(id)) });
            ready.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(stmt.param_binding_use.is_active());
            assert!(desc.binding_use.is_active());
            assert!(stmt.parent_dbc().inner.try_lock().is_ok());
            assert!(stmt.inner.try_lock().is_ok());
            assert!(desc.inner.try_lock().is_ok());

            let rebind = unsafe {
                sql_bind_parameter(
                    h.stmt,
                    1,
                    SQL_PARAM_OUTPUT,
                    SQL_C_SLONG,
                    SQL_INTEGER,
                    8,
                    0,
                    (&raw mut replacement).cast(),
                    4,
                    std::ptr::null_mut(),
                )
            };
            let reset = unsafe { sql_free_stmt_reset_params(h.stmt) };
            let reassociate = unsafe {
                sql_set_stmt_attr_w(h.stmt, SQL_ATTR_APP_PARAM_DESC, std::ptr::null_mut(), 0)
            };
            let offset = unsafe {
                sql_set_stmt_attr_w(
                    h.stmt,
                    SQL_ATTR_PARAM_BIND_OFFSET_PTR,
                    std::ptr::null_mut(),
                    0,
                )
            };
            let shared_bind = unsafe {
                sql_bind_col(
                    other,
                    1,
                    SQL_C_SLONG,
                    (&raw mut replacement).cast(),
                    4,
                    std::ptr::null_mut(),
                )
            };
            let field = unsafe {
                sql_set_desc_field_w(
                    desc_raw,
                    1,
                    SQL_DESC_DATA_PTR.try_into().unwrap(),
                    (&raw mut replacement).cast(),
                    0,
                )
            };
            let stmt_diag = stmt.inner.lock().unwrap().diag_records[0].sql_state;
            let desc_diag = desc.inner.lock().unwrap().diag_records[0].sql_state;
            let free = h.free_explicit_desc(desc_raw);
            resume.send(()).unwrap();
            let result = delivery.join().unwrap();
            assert_eq!(
                [rebind, reset, reassociate, offset, shared_bind, field, free],
                [SQL_ERROR; 7]
            );
            assert_eq!(stmt_diag, *b"HY010");
            assert_eq!(desc_diag, *b"HY010");
            assert_eq!(result, SQL_NO_DATA);
        });

        assert_eq!(values, [91, 73, 92]);
        assert_eq!(indicators, [93, 4, 94]);
        assert_eq!(replacement, -2);
        assert!(!stmt.param_binding_use.is_active());
        assert!(!desc.binding_use.is_active());
        assert_eq!(unsafe { sql_free_stmt_reset_params(h.stmt) }, SQL_SUCCESS);
        assert_eq!(h.free_explicit_desc(desc_raw), SQL_SUCCESS);
    }

    #[test]
    fn pending_output_diagnostics_preserve_severity_and_fractional_sqlstate() {
        for (value, c_type, expected_rc, expected_state) in [
            (
                ColumnValues::String(SqlString::new(b"abcdefgh".to_vec(), EncodingType::Utf8)),
                SQL_C_CHAR,
                SQL_SUCCESS_WITH_INFO,
                *b"01004",
            ),
            (
                ColumnValues::Float(12.75),
                SQL_C_SLONG,
                SQL_SUCCESS_WITH_INFO,
                *b"01S07",
            ),
            (
                ColumnValues::String(SqlString::new(b"invalid".to_vec(), EncodingType::Utf8)),
                SQL_C_SLONG,
                SQL_ERROR,
                *b"22018",
            ),
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
            let stmt = &*stmt_owner;
            let mut value_buffer = [0u8; 4];
            bind(
                &h,
                output_param(
                    c_type,
                    SQL_VARCHAR,
                    value_buffer.as_mut_ptr().cast(),
                    4,
                    std::ptr::null_mut(),
                ),
            );
            {
                let mut state = stmt.inner.lock().unwrap();
                state.batch_exhausted = true;
                state.pending_output_params = Some((vec![returned("@P1", 0, value)], None));
            }
            assert_eq!(unsafe { sql_more_results(h.stmt) }, expected_rc);
            assert_eq!(
                stmt.inner.lock().unwrap().diag_records[0].sql_state,
                expected_state
            );
            assert_eq!(unsafe { sql_more_results(h.stmt) }, SQL_NO_DATA);
            assert!(stmt.inner.lock().unwrap().diag_records.is_empty());
        }
    }

    #[test]
    fn return_status_requires_the_direct_rpc_route_even_for_return_value_direction() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        let mut value = -1i32;
        let mut param = output_param(
            SQL_C_SLONG,
            SQL_INTEGER,
            (&raw mut value).cast(),
            0,
            std::ptr::null_mut(),
        );
        param.input_output_type = SQL_RETURN_VALUE;
        let values = [returned("@P1", 0, ColumnValues::Int(37))];
        let mut state = stmt.inner.lock().unwrap();
        for direct_rpc in [false, true] {
            state.call_returns_status = direct_rpc;
            assert_eq!(
                unsafe { write_back_output_params(&mut state, &[Some(param)], &values, Some(19)) },
                SQL_SUCCESS
            );
            assert_eq!(value, if direct_rpc { 19 } else { 37 });
        }
    }

    #[test]
    fn unmatched_named_output_never_falls_back_to_another_binding() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        let mut value = -1i32;
        let param = output_param(
            SQL_C_SLONG,
            SQL_INTEGER,
            (&raw mut value).cast(),
            0,
            std::ptr::null_mut(),
        );
        let mut state = stmt.inner.lock().unwrap();
        assert_eq!(
            unsafe {
                write_back_output_params(
                    &mut state,
                    &[None, Some(param)],
                    &[returned("@P1", 0, ColumnValues::Int(7))],
                    None,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, -1);
        assert_eq!(
            unsafe {
                write_back_output_params(
                    &mut state,
                    &[None, Some(param)],
                    &[returned("", 1, ColumnValues::Int(9))],
                    None,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 9);
    }

    #[test]
    fn output_error_dominates_warnings_without_losing_diagnostics() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_owner = handle_from_raw::<StmtHandle>(h.stmt).unwrap().into_arc();
        let stmt = &*stmt_owner;
        let mut invalid = -1i32;
        let mut truncated = [0u8; 4];
        let mut fractional = -1i32;
        let bindings = [
            Some(output_param(
                SQL_C_SLONG,
                SQL_INTEGER,
                (&raw mut invalid).cast(),
                0,
                std::ptr::null_mut(),
            )),
            Some(output_param(
                SQL_C_CHAR,
                SQL_VARCHAR,
                truncated.as_mut_ptr().cast(),
                4,
                std::ptr::null_mut(),
            )),
            Some(output_param(
                SQL_C_SLONG,
                SQL_INTEGER,
                (&raw mut fractional).cast(),
                0,
                std::ptr::null_mut(),
            )),
        ];
        let values = [
            returned("@P1", 0, ColumnValues::Null),
            returned(
                "@P2",
                1,
                ColumnValues::String(SqlString::new(b"abcdefgh".to_vec(), EncodingType::Utf8)),
            ),
            returned("@P3", 2, ColumnValues::Float(12.75)),
        ];
        let mut state = stmt.inner.lock().unwrap();
        assert_eq!(
            unsafe { write_back_output_params(&mut state, &bindings, &values, None) },
            SQL_ERROR
        );
        let states: Vec<_> = state.diag_records.iter().map(|r| r.sql_state).collect();
        assert_eq!(states, [*b"22002", *b"01004", *b"01S07"]);
        assert_eq!(fractional, 12);
        assert_eq!(&truncated, b"abc\0");
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
        let outcome = unsafe { write_value(&param, &ColumnValues::Int(4711)) };
        assert_eq!(outcome, RowOutcome::Success);
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
        assert_eq!(
            unsafe { write_value(&param, &ColumnValues::Null) },
            RowOutcome::Success
        );
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
        assert!(matches!(
            unsafe { write_value(&param, &ColumnValues::Null) },
            RowOutcome::Error(_)
        ));
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
        assert_eq!(
            unsafe { write_value(&param, &value) },
            RowOutcome::Info(crate::api::fetch_scroll::RowIssue::StringTruncated)
        );
    }
}
