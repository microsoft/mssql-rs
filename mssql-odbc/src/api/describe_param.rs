// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDescribeParam.

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

        // Serving from the cache needs no connection, so it stays available
        // while a cursor is open - matching msodbcsql, which also answers
        // `SQLDescribeParam` from cached parameter metadata. The state check
        // below only guards the path that has to run the metadata RPC.
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

    let command = RpcParameter::new(None, StatusFlags::NONE, metadata_request_value(sql));
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

    let mut collector = DescriptionCollector::new(marker_count);
    // INVARIANT: a row that cannot be mapped must not leave this loop early.
    // The result set has to be drained and `close_query()` called below, or the
    // connection is left mid-result and every later operation on it fails. That
    // is why the mapping error is carried out in `parse_result` and inspected
    // *after* the drain, rather than propagated with `?` or an early `return`.
    let parse_result = loop {
        match dbc.runtime.block_on(client.next_row()) {
            Ok(Some(row)) => match parse_parameter_row(&row, marker_count, is_odbc3) {
                Ok((index, description)) => {
                    if let Err(e) = collector.accept(index, description) {
                        break Err(e);
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

    let descriptions = match parse_result.and_then(|()| collector.finish()) {
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

/// The `@tsql` argument of `sp_describe_undeclared_parameters`, which is
/// declared `nvarchar(max)` — a sized `nvarchar` would truncate a long
/// statement.
fn metadata_request_value(sql: String) -> SqlType {
    SqlType::NVarcharMax(Some(SqlString::from_utf8_string(sql)))
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
    precision: Option<u8>,
    scale: Option<u8>,
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
            let (precision, scale) = required_precision_scale(precision, scale)?;
            (
                SQL_DECIMAL,
                SqlULen::from(precision),
                SqlSmallInt::from(scale),
            )
        }
        TdsDataType::Numeric | TdsDataType::NumericN => {
            let (precision, scale) = required_precision_scale(precision, scale)?;
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
            let scale = required_temporal_scale(scale)?;
            let size = if scale == 0 {
                8
            } else {
                9 + SqlULen::from(scale)
            };
            (SQL_SS_TIME2, size, SqlSmallInt::from(scale))
        }
        // datetime and smalldatetime both surface as `SQL_TYPE_TIMESTAMP` with
        // the fixed precision/scale msodbcsql reports
        // (`Sql/Ntdbms/sqlncli/sqlctokn.cpp`). Neither driver records the
        // original server type name, so re-binding this description declares
        // `datetime2(3)`/`datetime2(0)` rather than the legacy type.
        TdsDataType::DateTime => (SQL_TYPE_TIMESTAMP, 23, 3),
        TdsDataType::DateTim4 => (SQL_TYPE_TIMESTAMP, 16, 0),
        TdsDataType::DateTimeN => match length {
            8 => (SQL_TYPE_TIMESTAMP, 23, 3),
            4 => (SQL_TYPE_TIMESTAMP, 16, 0),
            _ => return Err(format!("invalid DATETIMN length {length}")),
        },
        TdsDataType::DateTime2N => {
            let scale = required_temporal_scale(scale)?;
            let size = if scale == 0 {
                19
            } else {
                20 + SqlULen::from(scale)
            };
            (SQL_TYPE_TIMESTAMP, size, SqlSmallInt::from(scale))
        }
        TdsDataType::DateTimeOffsetN => {
            let scale = required_temporal_scale(scale)?;
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
        // Unbounded types report a size of 0, the same "unbounded" convention
        // `describe_col::column_size` already uses for PLP. A table type has no
        // meaningful column size at all.
        TdsDataType::Udt => (SQL_SS_UDT, 0, 0),
        TdsDataType::Xml => (SQL_SS_XML, 0, 0),
        TdsDataType::SqlTable => (SQL_SS_TABLE, 0, 0),
        // Neither of the next two arms fires against a shipping server. On SQL
        // Server 2025 RTM-CU1 (17.0.4006) `sp_describe_undeclared_parameters`
        // reports both `json` and `vector(n)` as `varchar(max)` (TDS id 167),
        // whether inferred from a column or from an explicit `CAST`; only `xml`
        // comes back as its own id (241). These arms are forward-looking, which
        // is also why there is no e2e case for either -- and it means the
        // `ColumnSize` <-> dimension round trip between `vector_user_size` here
        // and `vector_metadata` in `conversion::param_convert` has not been
        // checked against real server output. Do not "fix" the mapping against
        // an assumption about what the server emits.
        //
        // msodbcsql has no dedicated `json` ODBC type, so `json` surfaces as an
        // unbounded wide character type, matching how the value is exchanged.
        TdsDataType::Json => (SQL_WLONGVARCHAR, 0, 0),
        TdsDataType::Vector => {
            let base_type = scale.ok_or("suggested_scale is NULL for a vector")?;
            (
                SQL_SS_VECTOR,
                vector_user_size(length, base_type)?,
                SqlSmallInt::from(base_type),
            )
        }
        TdsDataType::Void | TdsDataType::None => {
            return Err(format!("unsupported inferred TDS type {data_type:?}"));
        }
    };

    Ok(ParameterDescription {
        data_type,
        parameter_size,
        decimal_digits,
        // `sp_describe_undeclared_parameters` never reports an inferred
        // parameter as NOT NULL, and msodbcsql hard-codes `SQL_NULLABLE` for
        // every row it processes (`Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp`,
        // `CImpODBCIObtainParameterMetadata::ProcessRow`).
        nullable: SQL_NULLABLE,
    })
}

/// ODBC 3.x reports binary precision for the approximate numeric types, ODBC 2.x
/// decimal digits.
fn float_precision(data_type: SqlSmallInt, is_odbc3: bool) -> SqlULen {
    match (data_type, is_odbc3) {
        (SQL_REAL, true) => 24,
        (SQL_FLOAT, true) => 53,
        (SQL_REAL, false) => 7,
        (SQL_FLOAT, false) => 15,
        _ => {
            debug_assert!(false, "float_precision called with {data_type}");
            15
        }
    }
}

/// A decimal always carries a precision and scale; a NULL in either column means
/// the row does not describe the type it claims to, which must not silently
/// become `decimal(0,0)`.
fn required_precision_scale(precision: Option<u8>, scale: Option<u8>) -> Result<(u8, u8), String> {
    let precision = precision.ok_or("suggested_precision is NULL for a decimal")?;
    let scale = scale.ok_or("suggested_scale is NULL for a decimal")?;
    if !(1..=38).contains(&precision) || scale > precision {
        return Err(format!("invalid precision/scale {precision},{scale}"));
    }
    Ok((precision, scale))
}

/// Likewise, a scale-bearing temporal type without a scale would silently
/// report `time(0)`/`datetime2(0)` and the wrong parameter size.
fn required_temporal_scale(scale: Option<u8>) -> Result<u8, String> {
    let scale = scale.ok_or("suggested_scale is NULL for a temporal type")?;
    if scale > 7 {
        return Err(format!("invalid temporal scale {scale}"));
    }
    Ok(scale)
}

/// Converts a TDS wire length into the ODBC `ParameterSize`.
///
/// A PLP length (`0xFFFF`, or `-1` once widened) means `*(max)`. msodbcsql
/// reports 0 for these, verified against msodbcsql 18.6.2.1 in the e2e parity
/// run; `SQL_PREC_UNLIMITED` in `sqlcdesc.cpp` (`GetIPDRec`) only passes through
/// a precision that is already unlimited and is not reached from a PLP wire
/// length. This matches `describe_col::column_size`.
/// `unicode` lengths are byte counts, so they halve into characters.
fn parameter_length(length: i32, unicode: bool) -> Result<SqlULen, String> {
    if length == -1 || length == i32::from(u16::MAX) {
        return Ok(0);
    }
    let length = SqlULen::try_from(length).map_err(|_| format!("invalid TDS length {length}"))?;
    Ok(if unicode { length / 2 } else { length })
}

/// Converts a vector's TDS payload length into the client buffer size ODBC
/// reports: a [`SqlSsVectorLayout`] header plus one
/// [`SQL_SS_VECTOR_ELEMENT_SIZE`] element per dimension.
///
/// The TDS element width follows the server-side base type (4 bytes for
/// float32, 2 for float16) while the client always exchanges 4-byte floats, so
/// the two widths are deliberately different.
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
    Ok((payload / tds_element_size) * SQL_SS_VECTOR_ELEMENT_SIZE
        + std::mem::size_of::<SqlSsVectorLayout>())
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

/// Reads a precision/scale column.
///
/// `sp_describe_undeclared_parameters` returns NULL for types that have no
/// precision or scale, so the NULL is preserved rather than flattened to `0`:
/// the arms that genuinely need one reject a missing value instead of
/// reporting a plausible-looking `decimal(0,0)` or `time(0)`.
fn read_optional_u8(row: &[ColumnValues], index: usize, name: &str) -> Result<Option<u8>, String> {
    match row.get(index) {
        Some(ColumnValues::Null) => Ok(None),
        _ => u8::try_from(read_i32(row, index, name)?)
            .map(Some)
            .map_err(|_| format!("{name} is out of range")),
    }
}

/// Places parsed metadata rows into their ordinal slots.
///
/// `sp_describe_undeclared_parameters` is expected to return exactly one row per
/// marker; a missing, duplicated, or out-of-range ordinal means the result set
/// does not describe the statement that was prepared, which must fail rather
/// than leave a parameter silently undescribed.
struct DescriptionCollector {
    slots: Vec<Option<ParameterDescription>>,
}

impl DescriptionCollector {
    fn new(marker_count: usize) -> Self {
        Self {
            slots: vec![None; marker_count],
        }
    }

    fn accept(&mut self, index: usize, description: ParameterDescription) -> Result<(), String> {
        // Unreachable in production: `parse_parameter_row` already filters on
        // the same `marker_count` this collector was built with. Kept so
        // `accept` is total rather than panicking if that filter ever moves.
        let Some(slot) = self.slots.get_mut(index) else {
            return Err(format!("parameter ordinal {} is out of range", index + 1));
        };
        if slot.replace(description).is_some() {
            return Err(format!("duplicate parameter ordinal {}", index + 1));
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<ParameterDescription>, String> {
        self.slots
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.ok_or_else(|| format!("missing metadata for parameter {}", index + 1))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestHandles;
    use mssql_tds::connection::tds_client::PreparedStatement;

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

    /// A metadata result set that carries no rows exercises the whole error
    /// tail against a scripted server: `advance_to_rows`, the drain loop,
    /// `close_query()`, `collector.finish()` reporting the absent marker, and
    /// `fail_metadata_response`. The connection assertions matter most - a
    /// regression that skips the drain or the client hand-back strands the
    /// connection mid-result and wedges every later operation on it, rather
    /// than merely returning a wrong answer for this call.
    #[test]
    fn empty_metadata_result_set_reports_hy000_and_returns_connection() {
        use crate::handles::dbc::DbcHandle;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_no_more, info, tds_client_from_tokens,
        };

        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let client = tds_client_from_tokens(vec![
            info(50000, 0, "a server message"),
            col_metadata_empty(),
            done_no_more(),
        ]);
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
        }

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
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);

        let ss = stmt.inner.lock().unwrap();
        assert_eq!(ss.diag_records[0].sql_state, SQLSTATE_HY000);
        assert!(
            ss.diag_records[0]
                .message
                .contains("missing metadata for parameter 1")
        );
        assert!(
            ss.diag_records
                .iter()
                .any(|d| d.message.contains("a server message"))
        );
        assert!(!ss.has_state(STMT_STATE_EXEC_STARTED));
        drop(ss);

        let ds = dbc.inner.lock().unwrap();
        assert!(ds.client.is_some());
        assert_eq!(ds.active_stmt, None);
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
                    // `varbinary(max)`: unbounded reports 0, matching msodbcsql.
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

    /// A scale-bearing type whose scale column is NULL describes something other
    /// than what its type id claims; reporting `time(0)` would hide that.
    #[test]
    fn rejects_missing_scale_for_scale_bearing_types() {
        let mut time_row = row(1, TdsDataType::TimeN, 5, 0, 3);
        time_row[SUGGESTED_SCALE] = ColumnValues::Null;
        assert!(parse_parameter_row(&time_row, 1, true).is_err());

        let mut decimal_row = row(1, TdsDataType::DecimalN, 17, 12, 3);
        decimal_row[SUGGESTED_PRECISION] = ColumnValues::Null;
        assert!(parse_parameter_row(&decimal_row, 1, true).is_err());

        // A type with no scale is unaffected by the NULL its row already carries.
        assert!(parse_parameter_row(&row(1, TdsDataType::Int4, 4, 0, 0), 1, true).is_ok());
    }

    /// The metadata RPC must send the statement text as `nvarchar(max)`:
    /// `sp_describe_undeclared_parameters`' `@tsql` argument is `nvarchar(max)`,
    /// and a sized `nvarchar` would silently truncate a long statement.
    #[test]
    fn request_uses_nvarchar_max() {
        let sql = "SELECT @P1".to_string();
        match metadata_request_value(sql.clone()) {
            SqlType::NVarcharMax(Some(text)) => assert_eq!(text.to_utf8_string(), sql),
            other => panic!("expected NVarcharMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn collector_requires_one_row_per_marker() {
        let description = describe_tds_type(TdsDataType::Int4, 4, None, None, true).unwrap();

        let mut collector = DescriptionCollector::new(2);
        collector.accept(0, description).unwrap();
        assert_eq!(
            collector.finish().unwrap_err(),
            "missing metadata for parameter 2"
        );

        let mut collector = DescriptionCollector::new(2);
        collector.accept(0, description).unwrap();
        assert_eq!(
            collector.accept(0, description).unwrap_err(),
            "duplicate parameter ordinal 1"
        );

        let mut collector = DescriptionCollector::new(1);
        assert_eq!(
            collector.accept(5, description).unwrap_err(),
            "parameter ordinal 6 is out of range"
        );

        let mut collector = DescriptionCollector::new(2);
        collector.accept(1, description).unwrap();
        collector.accept(0, description).unwrap();
        assert_eq!(collector.finish().unwrap().len(), 2);
    }
}
