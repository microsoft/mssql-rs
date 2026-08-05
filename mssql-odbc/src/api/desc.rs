// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Descriptor and parameter-metadata entry points.
//!
//! These are the minimum viable implementations required by applications that
//! bind `SQL_C_NUMERIC` parameters (which must set precision/scale on the APD)
//! or probe parameter types before binding.

use tracing::{debug, error};

use super::odbc_types::*;
use super::sqlstate::{SQLSTATE_07009, SQLSTATE_HY091};
use crate::error::{free_errors, post_sql_error};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Implements `SQLSetDescFieldW`.
///
/// The driver exposes implicit descriptors only, and the APD fields an
/// application sets for `SQL_C_NUMERIC` binding (type, precision, scale, data
/// pointer) are already captured by `SQLBindParameter`. Accepting them keeps
/// numeric binding working; anything else is reported as an unknown field so
/// callers are not silently misled.
///
/// # Safety
/// `descriptor_handle` must be a valid handle produced by
/// `SQLGetStmtAttr(SQL_ATTR_APP_PARAM_DESC)` — which this driver reports as the
/// statement handle itself — or null.
pub(crate) unsafe fn sql_set_desc_field_w(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    _value_ptr: SqlPointer,
    _buffer_length: SqlInteger,
) -> SqlReturn {
    debug!(
        ?descriptor_handle,
        record_number, field_identifier, "SQLSetDescFieldW called"
    );
    crate::ffi_entry!("SQLSetDescFieldW", unsafe {
        if descriptor_handle.is_null() {
            error!("SQLSetDescFieldW: descriptor_handle is null");
            return SQL_INVALID_HANDLE;
        }
        let field = field_identifier as SqlUSmallInt;
        if matches!(
            field,
            SQL_DESC_TYPE
                | SQL_DESC_CONCISE_TYPE
                | SQL_DESC_PRECISION
                | SQL_DESC_SCALE
                | SQL_DESC_DATA_PTR
                | SQL_DESC_LENGTH
                | SQL_DESC_OCTET_LENGTH
        ) {
            return SQL_SUCCESS;
        }

        let stmt = handle_from_raw::<StmtHandle>(descriptor_handle);
        if stmt.object_type != HandleType::Stmt {
            return SQL_INVALID_HANDLE;
        }
        let Ok(mut state) = stmt.inner.lock() else {
            error!("SQLSetDescFieldW: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        post_sql_error(
            &mut state,
            SQLSTATE_HY091,
            0,
            "Invalid descriptor field identifier",
        );
        SQL_ERROR
    })
}

/// Server-reported parameter metadata, cached per prepared statement.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DescribedParam {
    pub(crate) data_type: SqlSmallInt,
    pub(crate) parameter_size: SqlULen,
    pub(crate) decimal_digits: SqlSmallInt,
    pub(crate) nullable: SqlSmallInt,
}

/// Maps a SQL Server `system_type_id` (plus max length) onto the ODBC SQL type
/// msodbcsql reports for the same column.
fn odbc_type_for_system_type(system_type_id: i64, max_length: i64) -> SqlSmallInt {
    match system_type_id {
        34 => SQL_LONGVARBINARY,
        35 => SQL_LONGVARCHAR,
        36 => SQL_GUID,
        40 => SQL_TYPE_DATE,
        41 => SQL_SS_TIME2,
        42 | 58 | 61 => SQL_TYPE_TIMESTAMP,
        43 => SQL_SS_TIMESTAMPOFFSET,
        48 => SQL_TINYINT,
        52 => SQL_SMALLINT,
        56 => SQL_INTEGER,
        59 => SQL_REAL,
        62 => SQL_DOUBLE,
        98 => SQL_SS_VARIANT,
        99 => SQL_WLONGVARCHAR,
        104 => SQL_BIT,
        // money and smallmoney surface as fixed-scale decimals.
        60 | 106 | 122 => SQL_DECIMAL,
        108 => SQL_NUMERIC,
        127 => SQL_BIGINT,
        // `varbinary(max)` / `varchar(max)` / `nvarchar(max)` report a max
        // length of -1 and are the long variants.
        165 if max_length < 0 => SQL_LONGVARBINARY,
        165 => SQL_VARBINARY,
        167 if max_length < 0 => SQL_LONGVARCHAR,
        167 => SQL_VARCHAR,
        173 => SQL_BINARY,
        175 => SQL_CHAR,
        231 if max_length < 0 => SQL_WLONGVARCHAR,
        231 => SQL_WVARCHAR,
        239 => SQL_WCHAR,
        240 => SQL_SS_UDT,
        241 => SQL_WLONGVARCHAR,
        _ => SQL_WVARCHAR,
    }
}

/// Derives the ODBC column size from the server's `max_length`/`precision`.
fn describe_size(data_type: SqlSmallInt, max_length: i64, precision: i64) -> SqlULen {
    // `max_length = -1` marks a MAX type, which ODBC reports as size 0.
    if max_length < 0 {
        return 0;
    }
    let size = match data_type {
        SQL_DECIMAL | SQL_NUMERIC => precision,
        // Wide types report bytes; ODBC column size counts characters.
        SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR => max_length / 2,
        _ => max_length,
    };
    SqlULen::try_from(size.max(0)).unwrap_or(0)
}

/// Runs `sp_describe_undeclared_parameters` for the statement's prepared text
/// and caches the result.
///
/// Returns `None` when the statement has no prepared text, when the connection
/// is not idle, or when the server cannot describe the batch — all of which are
/// legitimate (temp tables and table variables are not describable), and
/// callers fall back to their own type inference.
fn fetch_described_params(stmt: &StmtHandle) -> Option<Vec<DescribedParam>> {
    let sql = {
        let state = stmt.inner.lock().ok()?;
        state.prepared_sql.clone()?
    };
    let (rewritten, marker_count) = crate::api::util::rewrite_param_markers(&sql);
    if marker_count == 0 {
        return Some(Vec::new());
    }

    let dbc = stmt.parent_dbc();
    let stmt_ptr: SqlHandle = (stmt as *const StmtHandle as *mut StmtHandle).cast();
    let mut client = crate::api::exec_common::try_claim_idle_client(dbc, stmt_ptr)?;

    let probe = format!(
        "EXEC sys.sp_describe_undeclared_parameters @tsql = N'{}'",
        rewritten.replace('\'', "''")
    );

    let described = describe_with_client(dbc, &mut client, probe, marker_count);
    crate::api::exec_common::return_client_idle(dbc, stmt_ptr, client);
    described
}

/// Reads the probe batch to completion so the connection is left in sync.
///
/// A describe probe fails routinely — temp tables and table variables are not
/// describable — and the server still writes a full response for the batch.
/// Leaving those tokens unread desynchronises the TDS stream, so the next
/// statement on the connection would decode garbage. Returns whether the batch
/// drained cleanly.
fn drain_batch(
    dbc: &crate::handles::DbcHandle,
    client: &mut mssql_tds::connection::tds_client::TdsClient,
) -> bool {
    use mssql_tds::connection::tds_client::{ResultSet, StatementResult};

    // Consume any rows the current result set still holds before advancing.
    loop {
        match dbc.runtime.block_on(client.next_row()) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                debug!(%e, "SQLDescribeParam: draining probe rows failed");
                return false;
            }
        }
    }
    while client.has_open_batch() {
        match dbc.runtime.block_on(client.advance()) {
            Ok(StatementResult::End) => return true,
            Ok(_) => {}
            Err(e) => {
                debug!(%e, "SQLDescribeParam: draining the probe batch failed");
                return false;
            }
        }
    }
    true
}

/// Executes the probe batch and folds its rows into parameter metadata.
fn describe_with_client(
    dbc: &crate::handles::DbcHandle,
    client: &mut mssql_tds::connection::tds_client::TdsClient,
    probe: String,
    marker_count: usize,
) -> Option<Vec<DescribedParam>> {
    use crate::api::cdata::{Cell, to_cell};
    use mssql_tds::connection::tds_client::ResultSet;

    if let Err(e) = dbc.runtime.block_on(client.execute(probe, ())) {
        debug!(%e, "SQLDescribeParam: server could not describe the batch");
        drain_batch(dbc, client);
        return None;
    }
    if !client.on_rows()
        && client.has_open_batch()
        && let Err(e) = dbc.runtime.block_on(client.advance_to_rows())
    {
        debug!(%e, "SQLDescribeParam: no describe result set");
        drain_batch(dbc, client);
        return None;
    }

    let mut described = vec![
        DescribedParam {
            data_type: SQL_WVARCHAR,
            parameter_size: 0,
            decimal_digits: 0,
            nullable: SQL_NULLABLE,
        };
        marker_count
    ];

    let mut failed = false;
    loop {
        match dbc.runtime.block_on(client.next_row()) {
            Ok(Some(row)) => {
                let num = |idx: usize| -> i64 {
                    row.get(idx)
                        .and_then(to_cell)
                        .and_then(|c: Cell| c.as_i64())
                        .unwrap_or(0)
                };
                let ordinal = num(COL_PARAMETER_ORDINAL);
                let Ok(index) = usize::try_from(ordinal - 1) else {
                    continue;
                };
                let Some(slot) = described.get_mut(index) else {
                    continue;
                };
                let max_length = num(COL_MAX_LENGTH);
                let data_type = odbc_type_for_system_type(num(COL_SYSTEM_TYPE_ID), max_length);
                *slot = DescribedParam {
                    data_type,
                    parameter_size: describe_size(data_type, max_length, num(COL_PRECISION)),
                    decimal_digits: SqlSmallInt::try_from(num(COL_SCALE)).unwrap_or(0),
                    nullable: SQL_NULLABLE,
                };
            }
            Ok(None) => break,
            Err(e) => {
                debug!(%e, "SQLDescribeParam: reading describe rows failed");
                failed = true;
                break;
            }
        }
    }

    // Drain the rest of the batch so the connection is reusable.
    if !failed {
        failed = !drain_batch(dbc, client);
    }

    if failed { None } else { Some(described) }
}

/// Column ordinals in the `sp_describe_undeclared_parameters` result set.
const COL_PARAMETER_ORDINAL: usize = 0;
const COL_SYSTEM_TYPE_ID: usize = 2;
const COL_MAX_LENGTH: usize = 4;
const COL_PRECISION: usize = 5;
const COL_SCALE: usize = 6;

/// Implements `SQLDescribeParam`.
///
/// Parameter metadata comes from `sp_describe_undeclared_parameters`, the same
/// source msodbcsql uses. The result is cached on the statement because callers
/// describe every parameter in turn and the probe costs a round trip.
///
/// Batches the server cannot describe — temp tables and table variables, most
/// notably — report `HY000`, which callers treat as "fall back to your own type
/// inference" rather than as a fatal error.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null; the output pointers
/// must each be null or writable for one value of their type.
pub(crate) unsafe fn sql_describe_param(
    statement_handle: SqlHandle,
    parameter_number: SqlUSmallInt,
    data_type_ptr: *mut SqlSmallInt,
    parameter_size_ptr: *mut SqlULen,
    decimal_digits_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        parameter_number, "SQLDescribeParam called"
    );
    crate::ffi_entry!("SQLDescribeParam", unsafe {
        if statement_handle.is_null() {
            error!("SQLDescribeParam: statement_handle is null");
            return SQL_INVALID_HANDLE;
        }
        let stmt = handle_from_raw::<StmtHandle>(statement_handle);
        debug_assert_eq!(stmt.object_type, HandleType::Stmt);

        if let Ok(mut state) = stmt.inner.lock() {
            free_errors(&mut state);
        } else {
            error!("SQLDescribeParam: stmt mutex poisoned");
            return SQL_ERROR;
        }

        if parameter_number == 0 {
            let Ok(mut state) = stmt.inner.lock() else {
                return SQL_ERROR;
            };
            post_sql_error(&mut state, SQLSTATE_07009, 0, "Invalid descriptor index");
            return SQL_ERROR;
        }

        let cached = match stmt.inner.lock() {
            Ok(state) => state.described_params.clone(),
            Err(_) => return SQL_ERROR,
        };
        let described = match cached {
            Some(described) => described,
            None => {
                let Some(described) = fetch_described_params(stmt) else {
                    let Ok(mut state) = stmt.inner.lock() else {
                        return SQL_ERROR;
                    };
                    post_sql_error(
                        &mut state,
                        super::sqlstate::SQLSTATE_HY000,
                        0,
                        "The server could not describe the statement's parameters",
                    );
                    return SQL_ERROR;
                };
                if let Ok(mut state) = stmt.inner.lock() {
                    state.described_params = Some(described.clone());
                }
                described
            }
        };

        let Some(info) = described.get(parameter_number as usize - 1).copied() else {
            let Ok(mut state) = stmt.inner.lock() else {
                return SQL_ERROR;
            };
            post_sql_error(&mut state, SQLSTATE_07009, 0, "Invalid descriptor index");
            return SQL_ERROR;
        };

        if !data_type_ptr.is_null() {
            data_type_ptr.write(info.data_type);
        }
        if !parameter_size_ptr.is_null() {
            parameter_size_ptr.write(info.parameter_size);
        }
        if !decimal_digits_ptr.is_null() {
            decimal_digits_ptr.write(info.decimal_digits);
        }
        if !nullable_ptr.is_null() {
            nullable_ptr.write(info.nullable);
        }
        SQL_SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestHandles;

    #[test]
    fn set_desc_field_accepts_apd_numeric_fields() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.stmt,
                1,
                SQL_DESC_PRECISION as SqlSmallInt,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_desc_field_rejects_unknown_field() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_desc_field_w(h.stmt, 1, 9999, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn set_desc_field_null_handle() {
        let ret =
            unsafe { sql_set_desc_field_w(SQL_NULL_HANDLE, 1, 1002, std::ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn describe_param_reports_unsupported() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_describe_param(
                h.stmt,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }
}
