// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLColAttributeW.
//!
//! Field values come from the same `ColumnMetadata` mapping `SQLDescribeColW`
//! uses, so the two APIs cannot report different types for the same column.

use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::query::metadata::ColumnMetadata;
use tracing::{debug, error};

use crate::api::describe_col::{column_size, decimal_digits, odbc_sql_type};
use crate::api::odbc_types::{
    SQL_ATTR_READWRITE_UNKNOWN, SQL_C_BINARY, SQL_C_BIT, SQL_C_CHAR, SQL_C_DOUBLE, SQL_C_FLOAT,
    SQL_C_GUID, SQL_C_SBIGINT, SQL_C_SLONG, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT,
    SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIMESTAMP, SQL_C_WCHAR, SQL_CA_SS_VARIANT_TYPE,
    SQL_DESC_AUTO_UNIQUE_VALUE, SQL_DESC_BASE_COLUMN_NAME, SQL_DESC_CASE_SENSITIVE,
    SQL_DESC_CONCISE_TYPE, SQL_DESC_COUNT, SQL_DESC_DISPLAY_SIZE, SQL_DESC_FIXED_PREC_SCALE,
    SQL_DESC_LABEL, SQL_DESC_LENGTH, SQL_DESC_NAME, SQL_DESC_NULLABLE, SQL_DESC_NUM_PREC_RADIX,
    SQL_DESC_OCTET_LENGTH, SQL_DESC_PRECISION, SQL_DESC_SCALE, SQL_DESC_SEARCHABLE, SQL_DESC_TYPE,
    SQL_DESC_TYPE_NAME, SQL_DESC_UNNAMED, SQL_DESC_UNSIGNED, SQL_DESC_UPDATABLE, SQL_ERROR,
    SQL_INVALID_HANDLE, SQL_NAMED, SQL_NO_NULLS, SQL_NULLABLE, SQL_PRED_SEARCHABLE, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SQL_UNNAMED, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt,
    SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_INVALID_DESCRIPTOR_FIELD, ERR_INVALID_DESCRIPTOR_INDEX,
    ERR_NOT_VARIANT_COLUMN, ERR_STRING_RIGHT_TRUNCATION, post_diag,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::free_errors;
use crate::handles::stmt::STMT_STATE_EXEC_CONTEXT;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_col_attribute_w(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    field_identifier: SqlUSmallInt,
    character_attribute_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    numeric_attribute_ptr: *mut SqlLen,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number,
        field_identifier,
        ?character_attribute_ptr,
        buffer_length,
        ?string_length_ptr,
        ?numeric_attribute_ptr,
        "SQLColAttributeW called",
    );

    crate::ffi_entry!("SQLColAttributeW", unsafe {
        sql_col_attribute_w_impl(
            statement_handle,
            column_number,
            field_identifier,
            character_attribute_ptr,
            buffer_length,
            string_length_ptr,
            numeric_attribute_ptr,
        )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_col_attribute_w_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    field_identifier: SqlUSmallInt,
    character_attribute_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    numeric_attribute_ptr: *mut SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLColAttributeW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLColAttributeW: handle is not a STMT"
    );

    sql_col_attribute_w_safe(
        stmt,
        column_number,
        field_identifier,
        character_attribute_ptr,
        buffer_length,
        string_length_ptr,
        numeric_attribute_ptr,
    )
}

/// Which output parameter a field identifier writes to.
enum Attr {
    Numeric(SqlLen),
    Text(String),
}

#[allow(clippy::too_many_arguments)]
fn sql_col_attribute_w_safe(
    stmt: &StmtHandle,
    column_number: SqlUSmallInt,
    field_identifier: SqlUSmallInt,
    character_attribute_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    numeric_attribute_ptr: *mut SqlLen,
) -> SqlReturn {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLColAttributeW: stmt mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut stmt_state);

    if !stmt_state.has_state(STMT_STATE_EXEC_CONTEXT) {
        post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
        return SQL_ERROR;
    }

    // SQL_DESC_COUNT describes the result set, not a column, so it is answered
    // before the column number is validated.
    if field_identifier == SQL_DESC_COUNT {
        let count = SqlLen::try_from(stmt_state.column_metadata.len()).unwrap_or(SqlLen::MAX);
        unsafe { write_if_some(numeric_attribute_ptr, count) };
        return SQL_SUCCESS;
    }

    if column_number == 0 || column_number as usize > stmt_state.column_metadata.len() {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    // The underlying type of a `sql_variant` is a property of the value, not the
    // column, so it comes from the row that was read rather than the metadata.
    if field_identifier == SQL_CA_SS_VARIANT_TYPE {
        let is_variant = stmt_state.column_metadata[(column_number - 1) as usize].data_type
            == TdsDataType::SsVariant;
        if !is_variant {
            post_diag(&mut stmt_state, ERR_NOT_VARIANT_COLUMN);
            return SQL_ERROR;
        }
        // The base type belongs to the value that was probed, so it only answers
        // for the column it came from.
        let base = stmt_state
            .last_variant_base
            .filter(|(col, _)| *col == column_number as usize)
            .map(|(_, base)| base);
        let Some(base) = base else {
            // Callers probe the column with SQLGetData first; that read is what
            // supplies the base type.
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        };
        unsafe { write_if_some(numeric_attribute_ptr, SqlLen::from(variant_c_type(base))) };
        return SQL_SUCCESS;
    }

    let meta = &stmt_state.column_metadata[(column_number - 1) as usize];
    let Some(attr) = column_attribute(meta, field_identifier) else {
        post_diag(&mut stmt_state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    };

    match attr {
        Attr::Numeric(v) => {
            unsafe { write_if_some(numeric_attribute_ptr, v) };
            SQL_SUCCESS
        }
        Attr::Text(s) => {
            let utf16: Vec<u16> = s.encode_utf16().collect();
            // StringLengthPtr is in bytes for the wide entry point, and excludes
            // the terminator.
            let byte_len = SqlSmallInt::try_from(utf16.len() * std::mem::size_of::<SqlWChar>())
                .unwrap_or(SqlSmallInt::MAX);
            unsafe { write_if_some(string_length_ptr, byte_len) };

            let buf_elements = if buffer_length > 0 {
                (buffer_length as usize) / std::mem::size_of::<SqlWChar>()
            } else {
                0
            };
            let truncated = unsafe {
                copy_with_nul(
                    character_attribute_ptr as *mut SqlWChar,
                    buf_elements,
                    &utf16,
                )
            };
            if truncated {
                post_diag(&mut stmt_state, ERR_STRING_RIGHT_TRUNCATION);
                SQL_SUCCESS_WITH_INFO
            } else {
                SQL_SUCCESS
            }
        }
    }
}

/// Maps a field identifier to its value, or `None` when the field is not one
/// this driver reports.
fn column_attribute(meta: &ColumnMetadata, field_identifier: SqlUSmallInt) -> Option<Attr> {
    let sql_type = odbc_sql_type(meta);
    let attr = match field_identifier {
        // `SQL_DESC_TYPE` and `SQL_DESC_CONCISE_TYPE` differ only for the
        // datetime/interval types, which this driver reports as concise types.
        SQL_DESC_TYPE | SQL_DESC_CONCISE_TYPE => Attr::Numeric(SqlLen::from(sql_type)),
        SQL_DESC_LENGTH | SQL_DESC_DISPLAY_SIZE => {
            Attr::Numeric(SqlLen::try_from(column_size(meta)).unwrap_or(SqlLen::MAX))
        }
        SQL_DESC_OCTET_LENGTH => Attr::Numeric(octet_length(meta)),
        SQL_DESC_PRECISION => Attr::Numeric(SqlLen::from(precision(meta))),
        SQL_DESC_SCALE => Attr::Numeric(SqlLen::from(decimal_digits(meta))),
        SQL_DESC_NULLABLE => Attr::Numeric(SqlLen::from(if meta.is_nullable() {
            SQL_NULLABLE
        } else {
            SQL_NO_NULLS
        })),
        // The boolean attributes are SQL_TRUE (1) / SQL_FALSE (0), which is what
        // `bool` converts to.
        SQL_DESC_UNSIGNED => Attr::Numeric(SqlLen::from(is_unsigned(meta))),
        SQL_DESC_CASE_SENSITIVE => Attr::Numeric(SqlLen::from(meta.is_case_sensitive())),
        SQL_DESC_FIXED_PREC_SCALE => Attr::Numeric(SqlLen::from(matches!(
            meta.data_type,
            TdsDataType::Money | TdsDataType::Money4 | TdsDataType::MoneyN
        ))),
        SQL_DESC_NUM_PREC_RADIX => Attr::Numeric(num_prec_radix(meta)),
        SQL_DESC_UNNAMED => Attr::Numeric(if meta.column_name.is_empty() {
            SQL_UNNAMED
        } else {
            SQL_NAMED
        }),
        // The result set is not known to be updatable, and no column here is a
        // known auto-increment column; report the "unknown"/false forms rather
        // than claiming either way.
        SQL_DESC_UPDATABLE => Attr::Numeric(SQL_ATTR_READWRITE_UNKNOWN),
        SQL_DESC_AUTO_UNIQUE_VALUE => Attr::Numeric(SqlLen::from(false)),
        SQL_DESC_SEARCHABLE => Attr::Numeric(SQL_PRED_SEARCHABLE),
        SQL_DESC_NAME | SQL_DESC_LABEL | SQL_DESC_BASE_COLUMN_NAME => {
            Attr::Text(meta.column_name.clone())
        }
        SQL_DESC_TYPE_NAME => Attr::Text(type_name(meta).to_string()),
        _ => return None,
    };
    Some(attr)
}

/// Storage size in bytes of the column's value on the wire.
fn octet_length(meta: &ColumnMetadata) -> SqlLen {
    if meta.is_plp() {
        return 0;
    }
    // `type_info.length` is already a byte count for every type, including the
    // national character types.
    SqlLen::try_from(meta.type_info.length).unwrap_or(SqlLen::MAX)
}

/// `SQL_DESC_PRECISION`: the number of significant digits for the exact and
/// approximate numeric types, otherwise the column size.
fn precision(meta: &ColumnMetadata) -> SqlSmallInt {
    if let Some(p) = meta.get_precision() {
        return SqlSmallInt::from(p);
    }
    SqlSmallInt::try_from(column_size(meta)).unwrap_or(SqlSmallInt::MAX)
}

fn num_prec_radix(meta: &ColumnMetadata) -> SqlLen {
    match meta.data_type {
        TdsDataType::Flt4 | TdsDataType::Flt8 | TdsDataType::FltN => 2,
        TdsDataType::Int1
        | TdsDataType::Int2
        | TdsDataType::Int4
        | TdsDataType::Int8
        | TdsDataType::IntN
        | TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN
        | TdsDataType::Money
        | TdsDataType::Money4
        | TdsDataType::MoneyN => 10,
        // Non-numeric columns have no radix.
        _ => 0,
    }
}

/// The C type a `sql_variant` value reports for `SQL_CA_SS_VARIANT_TYPE`.
///
/// msodbcsql answers this from its per-row column info, so the value's base type
/// decides it rather than the column's declared type.
fn variant_c_type(base: TdsDataType) -> SqlSmallInt {
    match base {
        TdsDataType::Int1 => SQL_C_TINYINT,
        TdsDataType::Int2 => SQL_C_SSHORT,
        TdsDataType::Int4 => SQL_C_SLONG,
        TdsDataType::Int8 => SQL_C_SBIGINT,
        TdsDataType::Bit | TdsDataType::BitN => SQL_C_BIT,
        TdsDataType::Flt4 => SQL_C_FLOAT,
        TdsDataType::Flt8 | TdsDataType::FltN => SQL_C_DOUBLE,
        // msodbcsql reports SQL_C_NUMERIC here, but emitting SQL_NUMERIC_STRUCT
        // is a permanent non-goal for this driver (see the divergence table), so
        // the exact numerics are advertised as character data, which is how they
        // are actually delivered.
        TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN
        | TdsDataType::Money
        | TdsDataType::Money4
        | TdsDataType::MoneyN => SQL_C_CHAR,
        TdsDataType::DateN => SQL_C_TYPE_DATE,
        TdsDataType::TimeN => SQL_C_SS_TIME2,
        TdsDataType::DateTime | TdsDataType::DateTim4 | TdsDataType::DateTimeN => {
            SQL_C_TYPE_TIMESTAMP
        }
        TdsDataType::DateTime2N => SQL_C_TYPE_TIMESTAMP,
        TdsDataType::DateTimeOffsetN => SQL_C_SS_TIMESTAMPOFFSET,
        TdsDataType::Char
        | TdsDataType::BigChar
        | TdsDataType::VarChar
        | TdsDataType::BigVarChar => SQL_C_CHAR,
        TdsDataType::NChar | TdsDataType::NVarChar => SQL_C_WCHAR,
        TdsDataType::Binary
        | TdsDataType::BigBinary
        | TdsDataType::VarBinary
        | TdsDataType::BigVarBinary => SQL_C_BINARY,
        TdsDataType::Guid => SQL_C_GUID,
        // SQL Server rejects the remaining types at insert time, so a variant
        // cannot actually carry them; character is the safe fallback.
        _ => SQL_C_CHAR,
    }
}

/// `tinyint` is the only unsigned integer SQL Server exposes.
fn is_unsigned(meta: &ColumnMetadata) -> bool {
    match meta.data_type {
        TdsDataType::Int1 => true,
        TdsDataType::IntN => meta.type_info.length == 1,
        _ => false,
    }
}

fn type_name(meta: &ColumnMetadata) -> &'static str {
    match meta.data_type {
        TdsDataType::Int1 => "tinyint",
        TdsDataType::Int2 => "smallint",
        TdsDataType::Int4 => "int",
        TdsDataType::Int8 => "bigint",
        TdsDataType::IntN => match meta.type_info.length {
            1 => "tinyint",
            2 => "smallint",
            4 => "int",
            8 => "bigint",
            _ => "int",
        },
        TdsDataType::Bit | TdsDataType::BitN => "bit",
        TdsDataType::Flt4 => "real",
        TdsDataType::Flt8 => "float",
        TdsDataType::FltN => {
            if meta.type_info.length == 4 {
                "real"
            } else {
                "float"
            }
        }
        TdsDataType::Decimal | TdsDataType::DecimalN => "decimal",
        TdsDataType::Numeric | TdsDataType::NumericN => "numeric",
        TdsDataType::Money | TdsDataType::MoneyN => "money",
        TdsDataType::Money4 => "smallmoney",
        TdsDataType::DateN => "date",
        TdsDataType::TimeN => "time",
        TdsDataType::DateTime | TdsDataType::DateTimeN => "datetime",
        TdsDataType::DateTim4 => "smalldatetime",
        TdsDataType::DateTime2N => "datetime2",
        TdsDataType::DateTimeOffsetN => "datetimeoffset",
        TdsDataType::Char | TdsDataType::BigChar => "char",
        TdsDataType::VarChar | TdsDataType::BigVarChar => "varchar",
        TdsDataType::Text => "text",
        TdsDataType::NChar => "nchar",
        TdsDataType::NVarChar => "nvarchar",
        TdsDataType::NText => "ntext",
        TdsDataType::Binary | TdsDataType::BigBinary => "binary",
        TdsDataType::VarBinary | TdsDataType::BigVarBinary => "varbinary",
        TdsDataType::Image => "image",
        TdsDataType::Guid => "uniqueidentifier",
        TdsDataType::Xml => "xml",
        TdsDataType::Json => "json",
        TdsDataType::Vector => "vector",
        TdsDataType::SsVariant => "sql_variant",
        TdsDataType::Udt => "udt",
        _ => "unknown",
    }
}

// Only `int` column metadata can be built outside the decoder (`int_columns`),
// so the per-type mapping tables are covered end-to-end by
// `tests/e2e/tests/col_attribute_test.cpp` against a live SQL Server.
#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{SQL_INTEGER, SQL_NULLABLE};
    use crate::api::sqlstate::ERR_INVALID_DESCRIPTOR_FIELD;
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sqldatatypes::TypeInfo;
    use mssql_tds::test_client_support::int_columns;

    /// A statement positioned on a result set of `n` nullable `int` columns.
    fn stmt_with_int_columns(h: &TestHandles, n: usize) {
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut s = stmt_handle.inner.lock().unwrap();
        s.set_state(STMT_STATE_EXEC_CONTEXT);
        s.column_metadata = int_columns(n);
    }

    /// Reads a numeric attribute, asserting the call succeeded.
    fn numeric(h: &TestHandles, col: SqlUSmallInt, field: SqlUSmallInt) -> SqlLen {
        let mut out: SqlLen = -1;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                col,
                field,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_SUCCESS, "field {field}");
        out
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                ptr::null_mut(),
                1,
                SQL_DESC_CONCISE_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn fresh_stmt_returns_sequence_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_CONCISE_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_FUNCTION_SEQUENCE.state
        );
    }

    #[test]
    fn column_out_of_range_is_invalid_descriptor_index() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 2);
        for col in [0, 3] {
            let mut out: SqlLen = 0;
            let rc = unsafe {
                sql_col_attribute_w(
                    h.stmt,
                    col,
                    SQL_DESC_CONCISE_TYPE,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut out,
                )
            };
            assert_eq!(rc, SQL_ERROR, "column {col}");
            let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let s = sh.inner.lock().unwrap();
            assert_eq!(
                s.diag_records.last().unwrap().sql_state,
                ERR_INVALID_DESCRIPTOR_INDEX.state
            );
        }
    }

    /// An identifier this driver does not report is HY091, not a silent zero.
    #[test]
    fn unknown_field_identifier_is_invalid_descriptor_field() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                9999,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_INVALID_DESCRIPTOR_FIELD.state
        );
    }

    /// SQL_DESC_COUNT describes the result set, so it answers even for a column
    /// number that would otherwise be out of range.
    #[test]
    fn desc_count_ignores_column_number() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 3);
        assert_eq!(numeric(&h, 0, SQL_DESC_COUNT), 3);
        assert_eq!(numeric(&h, 99, SQL_DESC_COUNT), 3);
    }

    #[test]
    fn int_column_numeric_attributes() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 2);
        assert_eq!(
            numeric(&h, 1, SQL_DESC_CONCISE_TYPE),
            SqlLen::from(SQL_INTEGER)
        );
        assert_eq!(numeric(&h, 1, SQL_DESC_TYPE), SqlLen::from(SQL_INTEGER));
        assert_eq!(
            numeric(&h, 1, SQL_DESC_NULLABLE),
            SqlLen::from(SQL_NULLABLE)
        );
        // `int` is signed, and base 10.
        assert_eq!(numeric(&h, 1, SQL_DESC_UNSIGNED), 0);
        assert_eq!(numeric(&h, 1, SQL_DESC_NUM_PREC_RADIX), 10);
        assert_eq!(numeric(&h, 1, SQL_DESC_UNNAMED), SQL_NAMED);
    }

    /// The wide entry point reports the name length in bytes, and a short buffer
    /// truncates with 01004 rather than failing.
    #[test]
    fn name_is_written_as_utf16_with_byte_length() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let mut buf = [0u16; 16];
        let mut len: SqlSmallInt = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_NAME,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * 2) as SqlSmallInt,
                &mut len,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        // "c1" is two characters, so four bytes.
        assert_eq!(len, 4);
        let name = String::from_utf16_lossy(&buf[..2]);
        assert_eq!(name, "c1");

        let mut small = [0u16; 2];
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_NAME,
                small.as_mut_ptr() as SqlPointer,
                (small.len() * 2) as SqlSmallInt,
                &mut len,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_STRING_RIGHT_TRUNCATION.state
        );
    }

    /// The variant attribute is rejected outright on a column that is not a
    /// `sql_variant`, rather than reporting a type the caller would then trust.
    #[test]
    fn variant_type_on_non_variant_column_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_CA_SS_VARIANT_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_NOT_VARIANT_COLUMN.state
        );
    }

    /// Retypes column `col` (1-based) in place. `int_columns` is the only
    /// metadata constructor available here, and the fields are public, so this
    /// is how the per-type mapping tables get exercised without a live server.
    fn retype_column(h: &TestHandles, col: usize, data_type: TdsDataType, length: usize) {
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut s = stmt_handle.inner.lock().unwrap();
        let meta = &mut s.column_metadata[col - 1];
        meta.data_type = data_type;
        meta.type_info.tds_type = data_type;
        meta.type_info.length = length;
    }

    /// Every numeric attribute this driver reports, on one `int` column.
    #[test]
    fn every_numeric_attribute_answers_for_an_int_column() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        assert_eq!(numeric(&h, 1, SQL_DESC_LENGTH), 10);
        assert_eq!(numeric(&h, 1, SQL_DESC_DISPLAY_SIZE), 10);
        assert_eq!(numeric(&h, 1, SQL_DESC_OCTET_LENGTH), 4);
        assert_eq!(numeric(&h, 1, SQL_DESC_PRECISION), 10);
        assert_eq!(numeric(&h, 1, SQL_DESC_SCALE), 0);
        assert_eq!(numeric(&h, 1, SQL_DESC_CASE_SENSITIVE), 0);
        assert_eq!(numeric(&h, 1, SQL_DESC_FIXED_PREC_SCALE), 0);
        assert_eq!(
            numeric(&h, 1, SQL_DESC_UPDATABLE),
            SQL_ATTR_READWRITE_UNKNOWN
        );
        assert_eq!(numeric(&h, 1, SQL_DESC_AUTO_UNIQUE_VALUE), 0);
        assert_eq!(numeric(&h, 1, SQL_DESC_SEARCHABLE), SQL_PRED_SEARCHABLE);
    }

    /// A column with no name reports `SQL_UNNAMED`; `int_columns` names them.
    #[test]
    fn unnamed_column_is_reported() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.column_metadata[0].column_name.clear();
        }
        assert_eq!(numeric(&h, 1, SQL_DESC_UNNAMED), SQL_UNNAMED);
    }

    /// A non-nullable column reports `SQL_NO_NULLS`. `int_columns` sets the
    /// nullable flag, so clear it.
    #[test]
    fn not_nullable_column_is_reported() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.column_metadata[0].flags &= !0x01;
        }
        assert_eq!(
            numeric(&h, 1, SQL_DESC_NULLABLE),
            SqlLen::from(SQL_NO_NULLS)
        );
    }

    #[test]
    fn type_name_and_radix_track_the_column_type() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        // (type, wire length, expected type name, expected radix)
        let cases: &[(TdsDataType, usize, &str, SqlLen)] = &[
            (TdsDataType::Int1, 1, "tinyint", 10),
            (TdsDataType::Int2, 2, "smallint", 10),
            (TdsDataType::Int8, 8, "bigint", 10),
            (TdsDataType::Flt4, 4, "real", 2),
            (TdsDataType::Flt8, 8, "float", 2),
            (TdsDataType::MoneyN, 8, "money", 10),
            (TdsDataType::Guid, 16, "uniqueidentifier", 0),
            (TdsDataType::BigVarChar, 10, "varchar", 0),
            (TdsDataType::NVarChar, 20, "nvarchar", 0),
            (TdsDataType::SsVariant, 8, "sql_variant", 0),
        ];
        for (ty, len, name, radix) in cases {
            retype_column(&h, 1, *ty, *len);
            assert_eq!(numeric(&h, 1, SQL_DESC_NUM_PREC_RADIX), *radix, "{ty:?}");
            assert_eq!(
                numeric(&h, 1, SQL_DESC_OCTET_LENGTH),
                *len as SqlLen,
                "{ty:?}"
            );

            let mut buf = [0u16; 32];
            let mut written: SqlSmallInt = 0;
            let rc = unsafe {
                sql_col_attribute_w(
                    h.stmt,
                    1,
                    SQL_DESC_TYPE_NAME,
                    buf.as_mut_ptr() as SqlPointer,
                    (buf.len() * 2) as SqlSmallInt,
                    &mut written,
                    ptr::null_mut(),
                )
            };
            assert_eq!(rc, SQL_SUCCESS, "{ty:?}");
            let got = String::from_utf16_lossy(&buf[..(written as usize) / 2]);
            assert_eq!(got, *name, "{ty:?}");
        }
    }

    /// `tinyint` is the only unsigned integer, and it is the one type this
    /// driver deliberately reports as unsigned.
    #[test]
    fn unsigned_is_reported_only_for_tinyint() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        retype_column(&h, 1, TdsDataType::Int1, 1);
        assert_eq!(numeric(&h, 1, SQL_DESC_UNSIGNED), 1);
        retype_column(&h, 1, TdsDataType::IntN, 1);
        assert_eq!(numeric(&h, 1, SQL_DESC_UNSIGNED), 1);
        retype_column(&h, 1, TdsDataType::IntN, 4);
        assert_eq!(numeric(&h, 1, SQL_DESC_UNSIGNED), 0);
    }

    /// The C type reported for each base type a `sql_variant` can carry.
    /// Exercised directly because a variant's base type is a property of the
    /// value, which unit tests cannot produce.
    #[test]
    fn variant_c_type_covers_the_base_types() {
        let cases: &[(TdsDataType, SqlSmallInt)] = &[
            (TdsDataType::Int1, SQL_C_TINYINT),
            (TdsDataType::Int2, SQL_C_SSHORT),
            (TdsDataType::Int4, SQL_C_SLONG),
            (TdsDataType::Int8, SQL_C_SBIGINT),
            (TdsDataType::Bit, SQL_C_BIT),
            (TdsDataType::Flt4, SQL_C_FLOAT),
            (TdsDataType::Flt8, SQL_C_DOUBLE),
            // The exact numerics are advertised as character data because
            // SQL_NUMERIC_STRUCT is a permanent non-goal.
            (TdsDataType::Numeric, SQL_C_CHAR),
            (TdsDataType::MoneyN, SQL_C_CHAR),
            (TdsDataType::DateN, SQL_C_TYPE_DATE),
            (TdsDataType::TimeN, SQL_C_SS_TIME2),
            (TdsDataType::DateTimeN, SQL_C_TYPE_TIMESTAMP),
            (TdsDataType::DateTime2N, SQL_C_TYPE_TIMESTAMP),
            (TdsDataType::DateTimeOffsetN, SQL_C_SS_TIMESTAMPOFFSET),
            (TdsDataType::BigVarChar, SQL_C_CHAR),
            (TdsDataType::NVarChar, SQL_C_WCHAR),
            (TdsDataType::BigVarBinary, SQL_C_BINARY),
            (TdsDataType::Guid, SQL_C_GUID),
            // A variant cannot carry these, so character is the fallback.
            (TdsDataType::Xml, SQL_C_CHAR),
        ];
        for (base, expected) in cases {
            assert_eq!(variant_c_type(*base), *expected, "{base:?}");
        }
    }

    /// The success path: a variant column whose value has been probed reports
    /// that value's underlying C type.
    #[test]
    fn variant_type_is_reported_after_the_value_is_probed() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 2);
        retype_column(&h, 1, TdsDataType::SsVariant, 8);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.last_variant_base = Some((1, TdsDataType::NVarChar));
        }
        assert_eq!(
            numeric(&h, 1, SQL_CA_SS_VARIANT_TYPE),
            SqlLen::from(SQL_C_WCHAR)
        );
    }

    /// A base type captured for one column must not answer for another.
    #[test]
    fn variant_type_does_not_leak_across_columns() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 2);
        retype_column(&h, 1, TdsDataType::SsVariant, 8);
        retype_column(&h, 2, TdsDataType::SsVariant, 8);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.last_variant_base = Some((1, TdsDataType::Int4));
        }
        let mut out: SqlLen = 0;
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                2,
                SQL_CA_SS_VARIANT_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut out,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let sh = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = sh.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_FUNCTION_SEQUENCE.state
        );
    }

    /// Every arm of the type-name table. Driven directly because a name is a
    /// pure function of the metadata and needs no live result set.
    #[test]
    fn type_name_covers_every_supported_type() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);

        let cases: &[(TdsDataType, usize, &str)] = &[
            (TdsDataType::Int1, 1, "tinyint"),
            (TdsDataType::Int2, 2, "smallint"),
            (TdsDataType::Int4, 4, "int"),
            (TdsDataType::Int8, 8, "bigint"),
            (TdsDataType::IntN, 1, "tinyint"),
            (TdsDataType::IntN, 2, "smallint"),
            (TdsDataType::IntN, 4, "int"),
            (TdsDataType::IntN, 8, "bigint"),
            // A length the server never sends still has to name something.
            (TdsDataType::IntN, 3, "int"),
            (TdsDataType::Bit, 1, "bit"),
            (TdsDataType::BitN, 1, "bit"),
            (TdsDataType::Flt4, 4, "real"),
            (TdsDataType::Flt8, 8, "float"),
            (TdsDataType::FltN, 4, "real"),
            (TdsDataType::FltN, 8, "float"),
            (TdsDataType::Decimal, 9, "decimal"),
            (TdsDataType::DecimalN, 9, "decimal"),
            (TdsDataType::Numeric, 9, "numeric"),
            (TdsDataType::NumericN, 9, "numeric"),
            (TdsDataType::Money, 8, "money"),
            (TdsDataType::MoneyN, 8, "money"),
            (TdsDataType::Money4, 4, "smallmoney"),
            (TdsDataType::DateN, 3, "date"),
            (TdsDataType::TimeN, 5, "time"),
            (TdsDataType::DateTime, 8, "datetime"),
            (TdsDataType::DateTimeN, 8, "datetime"),
            (TdsDataType::DateTim4, 4, "smalldatetime"),
            (TdsDataType::DateTime2N, 8, "datetime2"),
            (TdsDataType::DateTimeOffsetN, 10, "datetimeoffset"),
            (TdsDataType::Char, 10, "char"),
            (TdsDataType::BigChar, 10, "char"),
            (TdsDataType::VarChar, 10, "varchar"),
            (TdsDataType::BigVarChar, 10, "varchar"),
            (TdsDataType::Text, 16, "text"),
            (TdsDataType::NChar, 20, "nchar"),
            (TdsDataType::NVarChar, 20, "nvarchar"),
            (TdsDataType::NText, 16, "ntext"),
            (TdsDataType::Binary, 8, "binary"),
            (TdsDataType::BigBinary, 8, "binary"),
            (TdsDataType::VarBinary, 8, "varbinary"),
            (TdsDataType::BigVarBinary, 8, "varbinary"),
            (TdsDataType::Image, 16, "image"),
            (TdsDataType::Guid, 16, "uniqueidentifier"),
            (TdsDataType::Xml, 0, "xml"),
            (TdsDataType::Json, 0, "json"),
            (TdsDataType::Vector, 0, "vector"),
            (TdsDataType::SsVariant, 8, "sql_variant"),
            (TdsDataType::Udt, 0, "udt"),
            (TdsDataType::Void, 0, "unknown"),
        ];
        for (ty, len, name) in cases {
            retype_column(&h, 1, *ty, *len);
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let s = stmt_handle.inner.lock().unwrap();
            assert_eq!(type_name(&s.column_metadata[0]), *name, "{ty:?} len {len}");
        }
    }

    /// A `varchar(max)` streams as PLP, which has no fixed octet length, so
    /// the driver reports zero rather than the sentinel wire length.
    #[test]
    fn plp_column_reports_zero_octet_length() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            let meta = &mut s.column_metadata[0];
            meta.data_type = TdsDataType::BigVarChar;
            meta.type_info = TypeInfo::partial_len(TdsDataType::BigVarChar, 0xFFFF, None)
                .expect("varchar(max) is a PLP type");
        }
        assert_eq!(numeric(&h, 1, SQL_DESC_OCTET_LENGTH), 0);
    }

    /// A `decimal` carries its own precision on the wire, which takes
    /// precedence over the display size fallback.
    #[test]
    fn decimal_reports_its_declared_precision() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            let meta = &mut s.column_metadata[0];
            meta.data_type = TdsDataType::DecimalN;
            meta.type_info = TypeInfo::var_len_precision_scale(TdsDataType::DecimalN, 9, 18, 4)
                .expect("decimal carries precision and scale");
        }
        assert_eq!(numeric(&h, 1, SQL_DESC_PRECISION), 18);
        assert_eq!(numeric(&h, 1, SQL_DESC_SCALE), 4);
    }

    /// mssql-python passes a null string buffer and reads only the numeric
    /// attribute, so a null `character_attribute_ptr` must not fault.
    #[test]
    fn null_output_pointers_are_tolerated() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_with_int_columns(&h, 1);
        let rc = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_NAME,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
    }
}
