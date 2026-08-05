// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLColAttributeW — per-column descriptor field access.

use tracing::{debug, error};

use super::cdata::variant_c_type;
use super::describe_col::{column_size, decimal_digits, odbc_sql_type};
use super::odbc_types::*;
use super::sqlstate::{
    ERR_FUNCTION_SEQUENCE, ERR_INVALID_DESCRIPTOR_INDEX, ERR_STRING_RIGHT_TRUNCATION,
    SQLSTATE_HY091, post_diag,
};
use super::util::{copy_with_nul, write_if_some};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_EXEC_CONTEXT;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Implements `SQLColAttributeW`.
///
/// Numeric attributes are returned through `numeric_attribute_ptr`, character
/// attributes through `character_attribute_ptr`. Unknown identifiers produce
/// SQLSTATE HY091, matching msodbcsql.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null; the output
/// pointers, when non-null, must be writable for the sizes implied by
/// `buffer_length`.
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
        column_number, field_identifier, "SQLColAttributeW called"
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
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);

    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLColAttributeW: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    if !state.has_state(STMT_STATE_EXEC_CONTEXT) {
        post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
        return SQL_ERROR;
    }
    if field_identifier == SQL_DESC_COUNT {
        unsafe { write_if_some(numeric_attribute_ptr, state.column_metadata.len() as SqlLen) };
        return SQL_SUCCESS;
    }

    if column_number == 0 || usize::from(column_number) > state.column_metadata.len() {
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_INDEX);
        return SQL_ERROR;
    }

    let meta = &state.column_metadata[usize::from(column_number) - 1];
    let sql_type = odbc_sql_type(meta);

    // Character-valued fields.
    let text: Option<String> = match field_identifier {
        SQL_DESC_NAME | SQL_DESC_LABEL | SQL_DESC_BASE_COLUMN_NAME | SQL_COLUMN_NAME => {
            Some(meta.column_name.clone())
        }
        SQL_DESC_TYPE_NAME => Some(type_name(sql_type).to_string()),
        SQL_DESC_TABLE_NAME
        | SQL_DESC_SCHEMA_NAME
        | SQL_DESC_CATALOG_NAME
        | SQL_DESC_BASE_TABLE_NAME
        | SQL_DESC_LITERAL_PREFIX
        | SQL_DESC_LITERAL_SUFFIX
        | SQL_DESC_LOCAL_TYPE_NAME => Some(String::new()),
        _ => None,
    };

    if let Some(text) = text {
        let utf16: Vec<SqlWChar> = text.encode_utf16().collect();
        let byte_len = (utf16.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt;
        unsafe { write_if_some(string_length_ptr, byte_len) };
        let capacity = (buffer_length.max(0) as usize) / std::mem::size_of::<SqlWChar>();
        let truncated =
            unsafe { copy_with_nul(character_attribute_ptr as *mut SqlWChar, capacity, &utf16) };
        return if truncated {
            post_diag(&mut state, ERR_STRING_RIGHT_TRUNCATION);
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        };
    }

    let numeric: SqlLen = match field_identifier {
        // sql_variant columns report the C type of the value in the current row;
        // clients probe this to pick the right SQLGetData target type.
        SQL_CA_SS_VARIANT_TYPE => state
            .current_row
            .as_ref()
            .and_then(|row| row.get(usize::from(column_number) - 1))
            .map_or(SqlLen::from(SQL_C_WCHAR), |v| {
                SqlLen::from(variant_c_type(v))
            }),
        SQL_DESC_TYPE | SQL_DESC_CONCISE_TYPE => SqlLen::from(sql_type),
        SQL_DESC_LENGTH | SQL_DESC_DISPLAY_SIZE | SQL_DESC_OCTET_LENGTH | SQL_COLUMN_LENGTH => {
            column_size(meta) as SqlLen
        }
        SQL_DESC_PRECISION | SQL_COLUMN_PRECISION => column_size(meta) as SqlLen,
        SQL_DESC_SCALE | SQL_COLUMN_SCALE => SqlLen::from(decimal_digits(meta)),
        SQL_DESC_NULLABLE => SqlLen::from(if meta.is_nullable() {
            SQL_NULLABLE
        } else {
            SQL_NO_NULLS
        }),
        SQL_DESC_UNNAMED => SqlLen::from(meta.column_name.is_empty()),
        SQL_DESC_UNSIGNED => SqlLen::from(sql_type == SQL_TINYINT),
        SQL_DESC_CASE_SENSITIVE => 0,
        SQL_DESC_FIXED_PREC_SCALE => 0,
        SQL_DESC_AUTO_UNIQUE_VALUE => 0,
        SQL_DESC_UPDATABLE => 1,
        SQL_DESC_SEARCHABLE => 3,
        SQL_DESC_NUM_PREC_RADIX => {
            if matches!(sql_type, SQL_REAL | SQL_DOUBLE | SQL_FLOAT) {
                2
            } else {
                10
            }
        }
        _ => {
            post_sql_error(
                &mut state,
                SQLSTATE_HY091,
                0,
                "Invalid descriptor field identifier",
            );
            return SQL_ERROR;
        }
    };

    unsafe { write_if_some(numeric_attribute_ptr, numeric) };
    SQL_SUCCESS
}

fn type_name(sql_type: SqlSmallInt) -> &'static str {
    match sql_type {
        SQL_TINYINT => "tinyint",
        SQL_SMALLINT => "smallint",
        SQL_INTEGER => "int",
        SQL_BIGINT => "bigint",
        SQL_BIT => "bit",
        SQL_REAL => "real",
        SQL_DOUBLE | SQL_FLOAT => "float",
        SQL_DECIMAL => "decimal",
        SQL_NUMERIC => "numeric",
        SQL_GUID => "uniqueidentifier",
        SQL_BINARY => "binary",
        SQL_VARBINARY => "varbinary",
        SQL_LONGVARBINARY => "image",
        SQL_CHAR => "char",
        SQL_VARCHAR => "varchar",
        SQL_LONGVARCHAR => "text",
        SQL_WCHAR => "nchar",
        SQL_WVARCHAR => "nvarchar",
        SQL_WLONGVARCHAR => "ntext",
        SQL_TYPE_DATE => "date",
        SQL_TYPE_TIME => "time",
        SQL_TYPE_TIMESTAMP => "datetime2",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::TestHandles;

    #[test]
    fn col_attribute_null_handle() {
        let ret = unsafe {
            sql_col_attribute_w(
                SQL_NULL_HANDLE,
                1,
                SQL_DESC_COUNT,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn col_attribute_without_exec_context_is_sequence_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut num: SqlLen = 0;
        let ret = unsafe {
            sql_col_attribute_w(
                h.stmt,
                1,
                SQL_DESC_COUNT,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut num,
            )
        };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn col_attribute_count_allows_header_record() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_EXEC_CONTEXT);

        let mut num: SqlLen = -1;
        let ret = unsafe {
            sql_col_attribute_w(
                h.stmt,
                0,
                SQL_DESC_COUNT,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut num,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(num, 0);
    }
}
