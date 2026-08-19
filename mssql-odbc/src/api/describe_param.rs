// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDescribeParam.

use std::ffi::c_void;

use tracing::{debug, error};

use mssql_tds::connection::tds_client::ResultSet;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

use super::exec_common::{
    claim_connection, fail_with_tds, flush_pending_unprepare, return_client_idle,
};
use super::odbc_types::*;
use super::sqlstate::*;
use super::txn::begin_transaction_if_manual;
use super::util::write_if_some;
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{ParameterDescription, STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_STARTED};
use crate::handles::{HandleType, OdbcVersion, StmtHandle, handle_from_raw};

const DESCRIBE_PARAMETERS_PROC: &str = "sp_describe_undeclared_parameters";

const PARAMETER_ORDINAL: usize = 0;
const SUGGESTED_PRECISION: usize = 5;
const SUGGESTED_SCALE: usize = 6;
const SUGGESTED_TDS_TYPE_ID: usize = 22;
const SUGGESTED_TDS_LENGTH: usize = 23;

#[allow(clippy::too_many_arguments)]
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
        parameter_number,
        ?data_type_ptr,
        ?parameter_size_ptr,
        ?decimal_digits_ptr,
        ?nullable_ptr,
        "SQLDescribeParam called",
    );

    crate::ffi_entry!("SQLDescribeParam", unsafe {
        sql_describe_param_impl(
            statement_handle,
            parameter_number,
            data_type_ptr,
            parameter_size_ptr,
            decimal_digits_ptr,
            nullable_ptr,
        )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_describe_param_impl(
    statement_handle: SqlHandle,
    parameter_number: SqlUSmallInt,
    data_type_ptr: *mut SqlSmallInt,
    parameter_size_ptr: *mut SqlULen,
    decimal_digits_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLDescribeParam: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLDescribeParam: handle is not a STMT"
    );

    sql_describe_param_safe(
        statement_handle,
        stmt,
        parameter_number,
        data_type_ptr,
        parameter_size_ptr,
        decimal_digits_ptr,
        nullable_ptr,
    )
}

#[allow(clippy::too_many_arguments)]
fn sql_describe_param_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    parameter_number: SqlUSmallInt,
    data_type_ptr: *mut SqlSmallInt,
    parameter_size_ptr: *mut SqlULen,
    decimal_digits_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();
    let is_odbc3 = {
        let env = dbc.parent_env();
        let Ok(env_state) = env.inner.lock() else {
            error!("SQLDescribeParam: env mutex poisoned");
            return SQL_ERROR;
        };
        env_state.odbc_version != OdbcVersion::Odbc2
    };

    let (sql, marker_count) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLDescribeParam: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        let Some(plan) = stmt_state.prepared.as_ref() else {
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        };
        let marker_count = plan.marker_count;

        if parameter_number == 0 || usize::from(parameter_number) > marker_count {
            post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
            return SQL_ERROR;
        }

        if stmt_state.parameter_metadata.len() == marker_count {
            let Some(description) = stmt_state
                .parameter_metadata
                .get(usize::from(parameter_number) - 1)
                .copied()
            else {
                post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
                return SQL_ERROR;
            };
            write_description(
                description,
                data_type_ptr,
                parameter_size_ptr,
                decimal_digits_ptr,
                nullable_ptr,
            );
            return SQL_SUCCESS;
        }

        if stmt_state.has_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN) {
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }

        let sql = plan.stmt.sql().to_string();
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
        (sql, marker_count)
    };

    let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLDescribeParam") {
        Ok(client) => client,
        Err(rc) => return rc,
    };
    flush_pending_unprepare(dbc, stmt, &mut client, "SQLDescribeParam");

    if let Err(e) = begin_transaction_if_manual(dbc, &mut client, "SQLDescribeParam") {
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    let command = RpcParameter::new(
        None,
        StatusFlags::NONE,
        SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql))),
    );
    let execute_result = dbc.runtime.block_on(client.execute_stored_procedure(
        DESCRIBE_PARAMETERS_PROC.to_string(),
        Some(vec![command]),
        None,
        (),
    ));
    if let Err(e) = execute_result {
        error!(%e, "SQLDescribeParam: metadata RPC failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    if !client.on_rows() && client.has_open_batch() {
        match dbc.runtime.block_on(client.advance_to_rows()) {
            Ok(true) => {}
            Ok(false) => {
                return fail_metadata_response(
                    dbc,
                    stmt,
                    statement_handle,
                    client,
                    "metadata RPC returned no result set",
                );
            }
            Err(e) => return fail_with_tds(dbc, stmt, statement_handle, client, &e),
        }
    }

    let mut descriptions = vec![None; marker_count];
    let parse_result = loop {
        match dbc.runtime.block_on(client.next_row()) {
            Ok(Some(row)) => match parse_parameter_row(&row, marker_count, is_odbc3) {
                Ok((index, description)) => {
                    let Some(slot) = descriptions.get_mut(index) else {
                        break Err("parameter ordinal is out of range".to_string());
                    };
                    if slot.replace(description).is_some() {
                        break Err(format!("duplicate parameter ordinal {}", index + 1));
                    }
                }
                Err(e) => break Err(e),
            },
            Ok(None) => break Ok(()),
            Err(e) => return fail_with_tds(dbc, stmt, statement_handle, client, &e),
        }
    };

    if let Err(e) = dbc.runtime.block_on(client.close_query()) {
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    let descriptions = match parse_result.and_then(|()| {
        descriptions
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.ok_or_else(|| format!("missing metadata for parameter {}", index + 1))
            })
            .collect::<Result<Vec<_>, _>>()
    }) {
        Ok(descriptions) => descriptions,
        Err(e) => {
            return fail_metadata_response(dbc, stmt, statement_handle, client, &e);
        }
    };

    let info_messages = client.take_info_messages();
    return_client_idle(dbc, statement_handle, client);

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLDescribeParam: stmt mutex poisoned storing metadata");
        return SQL_ERROR;
    };
    stmt_state.parameter_metadata = descriptions;
    stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    let has_info = post_tds_info_messages(&mut stmt_state, &info_messages);

    let Some(description) = stmt_state
        .parameter_metadata
        .get(usize::from(parameter_number) - 1)
        .copied()
    else {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    };
    write_description(
        description,
        data_type_ptr,
        parameter_size_ptr,
        decimal_digits_ptr,
        nullable_ptr,
    );

    if has_info {
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

fn fail_metadata_response(
    dbc: &crate::handles::DbcHandle,
    stmt: &StmtHandle,
    statement_handle: SqlHandle,
    mut client: mssql_tds::connection::tds_client::TdsClient,
    message: &str,
) -> SqlReturn {
    let info_messages = client.take_info_messages();
    return_client_idle(dbc, statement_handle, client);
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_HY000,
            0,
            format!("Invalid parameter metadata returned by SQL Server: {message}"),
        );
        post_tds_info_messages(&mut stmt_state, &info_messages);
        stmt_state.clear_state(STMT_STATE_EXEC_STARTED);
    }
    SQL_ERROR
}

fn write_description(
    description: ParameterDescription,
    data_type_ptr: *mut SqlSmallInt,
    parameter_size_ptr: *mut SqlULen,
    decimal_digits_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) {
    unsafe { write_if_some(data_type_ptr, description.data_type) };
    unsafe { write_if_some(parameter_size_ptr, description.parameter_size) };
    unsafe { write_if_some(decimal_digits_ptr, description.decimal_digits) };
    unsafe { write_if_some(nullable_ptr, description.nullable) };
}

fn parse_parameter_row(
    row: &[ColumnValues],
    marker_count: usize,
    is_odbc3: bool,
) -> Result<(usize, ParameterDescription), String> {
    let ordinal = read_i32(row, PARAMETER_ORDINAL, "parameter_ordinal")?;
    let index = usize::try_from(ordinal)
        .ok()
        .and_then(|value| value.checked_sub(1))
        .filter(|index| *index < marker_count)
        .ok_or_else(|| format!("invalid parameter ordinal {ordinal}"))?;

    let type_id = read_i32(row, SUGGESTED_TDS_TYPE_ID, "suggested_tds_type_id")?;
    let type_id = u8::try_from(type_id).map_err(|_| format!("invalid TDS type id {type_id}"))?;
    let data_type =
        TdsDataType::try_from(type_id).map_err(|_| format!("unknown TDS type id {type_id:#x}"))?;
    let length = read_i32(row, SUGGESTED_TDS_LENGTH, "suggested_tds_length")?;
    let precision = read_optional_u8(row, SUGGESTED_PRECISION, "suggested_precision")?;
    let scale = read_optional_u8(row, SUGGESTED_SCALE, "suggested_scale")?;

    Ok((
        index,
        describe_tds_type(data_type, length, precision, scale, is_odbc3)?,
    ))
}

fn describe_tds_type(
    data_type: TdsDataType,
    length: i32,
    precision: u8,
    scale: u8,
    is_odbc3: bool,
) -> Result<ParameterDescription, String> {
    let (data_type, parameter_size, decimal_digits) = match data_type {
        TdsDataType::Bit | TdsDataType::BitN => (SQL_BIT, 1, 0),
        TdsDataType::Int1 => (SQL_TINYINT, 3, 0),
        TdsDataType::Int2 => (SQL_SMALLINT, 5, 0),
        TdsDataType::Int4 => (SQL_INTEGER, 10, 0),
        TdsDataType::Int8 => (SQL_BIGINT, 19, 0),
        TdsDataType::IntN => match length {
            1 => (SQL_TINYINT, 3, 0),
            2 => (SQL_SMALLINT, 5, 0),
            4 => (SQL_INTEGER, 10, 0),
            8 => (SQL_BIGINT, 19, 0),
            _ => return Err(format!("invalid INTN length {length}")),
        },
        TdsDataType::Flt4 => (SQL_REAL, float_precision(SQL_REAL, is_odbc3), 0),
        TdsDataType::Flt8 => (SQL_FLOAT, float_precision(SQL_FLOAT, is_odbc3), 0),
        TdsDataType::FltN => match length {
            4 => (SQL_REAL, float_precision(SQL_REAL, is_odbc3), 0),
            8 => (SQL_FLOAT, float_precision(SQL_FLOAT, is_odbc3), 0),
            _ => return Err(format!("invalid FLTN length {length}")),
        },
        TdsDataType::Decimal | TdsDataType::DecimalN => {
            validate_precision_scale(precision, scale)?;
            (
                SQL_DECIMAL,
                SqlULen::from(precision),
                SqlSmallInt::from(scale),
            )
        }
        TdsDataType::Numeric | TdsDataType::NumericN => {
            validate_precision_scale(precision, scale)?;
            (
                SQL_NUMERIC,
                SqlULen::from(precision),
                SqlSmallInt::from(scale),
            )
        }
        TdsDataType::Money | TdsDataType::MoneyN if length == 8 => (SQL_DECIMAL, 19, 4),
        TdsDataType::Money4 | TdsDataType::MoneyN if length == 4 => (SQL_DECIMAL, 10, 4),
        TdsDataType::Money | TdsDataType::Money4 | TdsDataType::MoneyN => {
            return Err(format!("invalid money length {length}"));
        }
        TdsDataType::DateN => (SQL_TYPE_DATE, 10, 0),
        TdsDataType::TimeN => {
            validate_temporal_scale(scale)?;
            let size = if scale == 0 {
                8
            } else {
                9 + SqlULen::from(scale)
            };
            (SQL_SS_TIME2, size, SqlSmallInt::from(scale))
        }
        TdsDataType::DateTime => (SQL_TYPE_TIMESTAMP, 23, 3),
        TdsDataType::DateTim4 => (SQL_TYPE_TIMESTAMP, 16, 0),
        TdsDataType::DateTimeN => match length {
            8 => (SQL_TYPE_TIMESTAMP, 23, 3),
            4 => (SQL_TYPE_TIMESTAMP, 16, 0),
            _ => return Err(format!("invalid DATETIMN length {length}")),
        },
        TdsDataType::DateTime2N => {
            validate_temporal_scale(scale)?;
            let size = if scale == 0 {
                19
            } else {
                20 + SqlULen::from(scale)
            };
            (SQL_TYPE_TIMESTAMP, size, SqlSmallInt::from(scale))
        }
        TdsDataType::DateTimeOffsetN => {
            validate_temporal_scale(scale)?;
            let size = if scale == 0 {
                26
            } else {
                27 + SqlULen::from(scale)
            };
            (SQL_SS_TIMESTAMPOFFSET, size, SqlSmallInt::from(scale))
        }
        TdsDataType::Guid => (SQL_GUID, 36, 0),
        TdsDataType::Char | TdsDataType::BigChar => (SQL_CHAR, parameter_length(length, false)?, 0),
        TdsDataType::VarChar | TdsDataType::BigVarChar => {
            (SQL_VARCHAR, parameter_length(length, false)?, 0)
        }
        TdsDataType::Text => (SQL_LONGVARCHAR, parameter_length(length, false)?, 0),
        TdsDataType::NChar => (SQL_WCHAR, parameter_length(length, true)?, 0),
        TdsDataType::NVarChar => (SQL_WVARCHAR, parameter_length(length, true)?, 0),
        TdsDataType::NText => (SQL_WLONGVARCHAR, parameter_length(length, true)?, 0),
        TdsDataType::Binary | TdsDataType::BigBinary => {
            (SQL_BINARY, parameter_length(length, false)?, 0)
        }
        TdsDataType::VarBinary | TdsDataType::BigVarBinary => {
            (SQL_VARBINARY, parameter_length(length, false)?, 0)
        }
        TdsDataType::Image => (SQL_LONGVARBINARY, parameter_length(length, false)?, 0),
        TdsDataType::SsVariant => (SQL_SS_VARIANT, 8000, 0),
        TdsDataType::Udt => (SQL_SS_UDT, 0, 0),
        TdsDataType::Xml => (SQL_SS_XML, 0, 0),
        TdsDataType::SqlTable => (SQL_SS_TABLE, 0, 0),
        TdsDataType::Json => (SQL_WLONGVARCHAR, 0, 0),
        TdsDataType::Vector => (
            SQL_SS_VECTOR,
            vector_user_size(length, scale)?,
            SqlSmallInt::from(scale),
        ),
        TdsDataType::Void | TdsDataType::None => {
            return Err(format!("unsupported inferred TDS type {data_type:?}"));
        }
    };

    Ok(ParameterDescription {
        data_type,
        parameter_size,
        decimal_digits,
        nullable: SQL_NULLABLE,
    })
}

fn float_precision(data_type: SqlSmallInt, is_odbc3: bool) -> SqlULen {
    match (data_type, is_odbc3) {
        (SQL_REAL, true) => 24,
        (SQL_FLOAT, true) => 53,
        (SQL_REAL, false) => 7,
        _ => 15,
    }
}

fn validate_precision_scale(precision: u8, scale: u8) -> Result<(), String> {
    if !(1..=38).contains(&precision) || scale > precision {
        return Err(format!("invalid precision/scale {precision},{scale}"));
    }
    Ok(())
}

fn validate_temporal_scale(scale: u8) -> Result<(), String> {
    if scale > 7 {
        return Err(format!("invalid temporal scale {scale}"));
    }
    Ok(())
}

fn parameter_length(length: i32, unicode: bool) -> Result<SqlULen, String> {
    if length == -1 || length == i32::from(u16::MAX) {
        return Ok(0);
    }
    let length = SqlULen::try_from(length).map_err(|_| format!("invalid TDS length {length}"))?;
    Ok(if unicode { length / 2 } else { length })
}

#[repr(C)]
struct SqlSsVectorLayout {
    dimension: SqlSmallInt,
    vector_type: i32,
    data: *mut c_void,
}

fn vector_user_size(length: i32, base_type: u8) -> Result<SqlULen, String> {
    const VECTOR_HEADER_SIZE: SqlULen = 8;
    let tds_element_size = match base_type {
        0 => 4,
        1 => 2,
        _ => return Err(format!("unsupported vector base type {base_type}")),
    };
    let length =
        SqlULen::try_from(length).map_err(|_| format!("invalid vector length {length}"))?;
    let payload = length
        .checked_sub(VECTOR_HEADER_SIZE)
        .ok_or_else(|| format!("invalid vector length {length}"))?;
    Ok((payload / tds_element_size) * 4 + std::mem::size_of::<SqlSsVectorLayout>())
}

fn read_i32(row: &[ColumnValues], index: usize, name: &str) -> Result<i32, String> {
    match row.get(index) {
        Some(ColumnValues::TinyInt(value)) => Ok(i32::from(*value)),
        Some(ColumnValues::SmallInt(value)) => Ok(i32::from(*value)),
        Some(ColumnValues::Int(value)) => Ok(*value),
        Some(ColumnValues::BigInt(value)) => {
            i32::try_from(*value).map_err(|_| format!("{name} is out of range"))
        }
        other => Err(format!("{name} must be an integer, got {other:?}")),
    }
}

fn read_optional_u8(row: &[ColumnValues], index: usize, name: &str) -> Result<u8, String> {
    match row.get(index) {
        Some(ColumnValues::Null) => Ok(0),
        _ => {
            u8::try_from(read_i32(row, index, name)?).map_err(|_| format!("{name} is out of range"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestHandles;
    use mssql_tds::connection::tds_client::PreparedStatement;
    use mssql_tds::datatypes::sql_string::SqlString;

    fn row(
        ordinal: i32,
        data_type: TdsDataType,
        length: i32,
        precision: u8,
        scale: u8,
    ) -> Vec<ColumnValues> {
        let mut row = vec![ColumnValues::Null; 24];
        row[PARAMETER_ORDINAL] = ColumnValues::Int(ordinal);
        row[SUGGESTED_PRECISION] = ColumnValues::TinyInt(precision);
        row[SUGGESTED_SCALE] = ColumnValues::TinyInt(scale);
        row[SUGGESTED_TDS_TYPE_ID] = ColumnValues::Int(i32::from(data_type as u8));
        row[SUGGESTED_TDS_LENGTH] = ColumnValues::Int(length);
        row
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let rc = unsafe {
            sql_describe_param(
                SQL_NULL_HANDLE,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn unprepared_statement_returns_hy010() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = unsafe {
            sql_describe_param(
                h.stmt,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            SQLSTATE_HY010
        );
    }

    #[test]
    fn invalid_ordinal_returns_07009_without_io() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared = Some(crate::handles::stmt::PreparedPlan {
                stmt: PreparedStatement::new("SELECT @P1".to_string()),
                marker_count: 1,
            });
        }

        let rc = unsafe {
            sql_describe_param(
                h.stmt,
                2,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(
            stmt.inner.lock().unwrap().diag_records[0].sql_state,
            SQLSTATE_07009
        );
    }

    #[test]
    fn cached_description_allows_null_output_pointers() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.prepared = Some(crate::handles::stmt::PreparedPlan {
                stmt: PreparedStatement::new("SELECT @P1".to_string()),
                marker_count: 1,
            });
            state.parameter_metadata.push(ParameterDescription {
                data_type: SQL_INTEGER,
                parameter_size: 10,
                decimal_digits: 0,
                nullable: SQL_NULLABLE,
            });
        }

        let rc = unsafe {
            sql_describe_param(
                h.stmt,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
    }

    #[test]
    fn parses_mssql_python_integer_metadata() {
        let (_, description) =
            parse_parameter_row(&row(1, TdsDataType::IntN, 4, 10, 0), 1, true).unwrap();
        assert_eq!(
            description,
            ParameterDescription {
                data_type: SQL_INTEGER,
                parameter_size: 10,
                decimal_digits: 0,
                nullable: SQL_NULLABLE,
            }
        );
    }

    #[test]
    fn maps_unicode_numeric_temporal_and_max_metadata() {
        let cases = [
            (
                row(1, TdsDataType::NVarChar, 80, 0, 0),
                ParameterDescription {
                    data_type: SQL_WVARCHAR,
                    parameter_size: 40,
                    decimal_digits: 0,
                    nullable: SQL_NULLABLE,
                },
            ),
            (
                row(1, TdsDataType::DecimalN, 17, 28, 6),
                ParameterDescription {
                    data_type: SQL_DECIMAL,
                    parameter_size: 28,
                    decimal_digits: 6,
                    nullable: SQL_NULLABLE,
                },
            ),
            (
                row(1, TdsDataType::DateTimeOffsetN, 10, 0, 7),
                ParameterDescription {
                    data_type: SQL_SS_TIMESTAMPOFFSET,
                    parameter_size: 34,
                    decimal_digits: 7,
                    nullable: SQL_NULLABLE,
                },
            ),
            (
                row(1, TdsDataType::BigVarBinary, -1, 0, 0),
                ParameterDescription {
                    data_type: SQL_VARBINARY,
                    parameter_size: 0,
                    decimal_digits: 0,
                    nullable: SQL_NULLABLE,
                },
            ),
        ];

        for (row, expected) in cases {
            assert_eq!(parse_parameter_row(&row, 1, true).unwrap().1, expected);
        }
    }

    #[test]
    fn rejects_invalid_server_metadata() {
        assert!(parse_parameter_row(&row(2, TdsDataType::IntN, 4, 10, 0), 1, true).is_err());
        assert!(parse_parameter_row(&row(1, TdsDataType::DecimalN, 17, 0, 0), 1, true).is_err());
        assert!(parse_parameter_row(&row(1, TdsDataType::TimeN, 5, 0, 8), 1, true).is_err());
    }

    #[test]
    fn request_uses_nvarchar_max() {
        let value =
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string("SELECT @P1".to_string())));
        assert!(matches!(value, SqlType::NVarcharMax(Some(_))));
    }
}
