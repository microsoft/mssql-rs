// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLDescribeColW.

use std::panic;

use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_CHAR, SQL_DECIMAL, SQL_DOUBLE, SQL_ERROR, SQL_GUID,
    SQL_INTEGER, SQL_INVALID_HANDLE, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NO_NULLS,
    SQL_NULLABLE, SQL_REAL, SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SQL_TINYINT, SQL_TYPE_DATE, SQL_TYPE_TIMESTAMP, SQL_UNKNOWN_TYPE,
    SQL_VARBINARY, SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR, SqlHandle, SqlReturn,
    SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{SQLSTATE_01004, SQLSTATE_07009, SQLSTATE_HY010};
use crate::api::util::copy_with_nul;
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_EXEC_CONTEXT;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_describe_col_w(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    column_name: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    name_length_ptr: *mut SqlSmallInt,
    data_type_ptr: *mut SqlSmallInt,
    column_size_ptr: *mut u64,
    decimal_digits_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        column_number, buffer_length, "SQLDescribeColW called"
    );

    let result = panic::catch_unwind(|| unsafe {
        sql_describe_col_w_impl(
            statement_handle,
            column_number,
            column_name,
            buffer_length,
            name_length_ptr,
            data_type_ptr,
            column_size_ptr,
            decimal_digits_ptr,
            nullable_ptr,
        )
    });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLDescribeColW: panic caught at FFI boundary");
        SQL_ERROR
    });

    trace!(?ret, "SQLDescribeColW returning");
    ret
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_describe_col_w_impl(
    statement_handle: SqlHandle,
    column_number: SqlUSmallInt,
    column_name: *mut SqlWChar,
    buffer_length: SqlSmallInt,
    name_length_ptr: *mut SqlSmallInt,
    data_type_ptr: *mut SqlSmallInt,
    column_size_ptr: *mut u64,
    decimal_digits_ptr: *mut SqlSmallInt,
    nullable_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLDescribeColW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    // BufferLength is validated by the DM (SQLSTATE HY090). See:
    // https://learn.microsoft.com/en-us/sql/odbc/reference/syntax/sqldescribecol-function
    debug_assert!(
        buffer_length >= 0,
        "SQLDescribeColW: DM should reject negative buffer_length (HY090)"
    );

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLDescribeColW: handle is not a STMT"
    );

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLDescribeColW: stmt mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut stmt_state);

    if !stmt_state.has_state(STMT_STATE_EXEC_CONTEXT) {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_HY010,
            0,
            "Function sequence error",
        );
        return SQL_ERROR;
    }

    if column_number == 0 || column_number as usize > stmt_state.column_metadata.len() {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_07009,
            0,
            "Invalid descriptor index",
        );
        return SQL_ERROR;
    }

    let meta = &stmt_state.column_metadata[(column_number - 1) as usize];

    let name_utf16: Vec<u16> = meta.column_name.encode_utf16().collect();
    if !name_length_ptr.is_null() {
        let len = SqlSmallInt::try_from(name_utf16.len()).unwrap_or(SqlSmallInt::MAX);
        unsafe { name_length_ptr.write(len) };
    }

    let truncated = unsafe { copy_with_nul(column_name, buffer_length as usize, &name_utf16) };

    if !data_type_ptr.is_null() {
        unsafe { data_type_ptr.write(odbc_sql_type(meta)) };
    }
    if !column_size_ptr.is_null() {
        unsafe { column_size_ptr.write(column_size(meta)) };
    }
    if !decimal_digits_ptr.is_null() {
        unsafe { decimal_digits_ptr.write(decimal_digits(meta)) };
    }
    if !nullable_ptr.is_null() {
        let nullable = if meta.is_nullable() {
            SQL_NULLABLE
        } else {
            SQL_NO_NULLS
        };
        unsafe { nullable_ptr.write(nullable) };
    }

    if truncated {
        post_sql_error(
            &mut stmt_state,
            SQLSTATE_01004,
            0,
            "String data, right truncation",
        );
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

fn odbc_sql_type(meta: &mssql_tds::query::metadata::ColumnMetadata) -> SqlSmallInt {
    match meta.data_type {
        TdsDataType::Int1 => SQL_TINYINT,
        TdsDataType::Int2 => SQL_SMALLINT,
        TdsDataType::Int4 => SQL_INTEGER,
        TdsDataType::Int8 => SQL_BIGINT,
        TdsDataType::IntN => match meta.type_info.length {
            1 => SQL_TINYINT,
            2 => SQL_SMALLINT,
            4 => SQL_INTEGER,
            8 => SQL_BIGINT,
            _ => SQL_UNKNOWN_TYPE,
        },
        TdsDataType::Bit | TdsDataType::BitN => SQL_BIT,
        TdsDataType::Flt4 => SQL_REAL,
        TdsDataType::Flt8 => SQL_DOUBLE,
        TdsDataType::FltN => match meta.type_info.length {
            4 => SQL_REAL,
            8 => SQL_DOUBLE,
            _ => SQL_UNKNOWN_TYPE,
        },
        TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN => SQL_DECIMAL,
        TdsDataType::Money | TdsDataType::Money4 | TdsDataType::MoneyN => SQL_DECIMAL,
        TdsDataType::DateN => SQL_TYPE_DATE,
        // SQL Server's `time` supports up to 7-digit fractional seconds; SQL_TYPE_TIME
        // is limited to second precision. msodbcsql reports SQL_SS_TIME2 (-154).
        TdsDataType::TimeN => SQL_SS_TIME2,
        TdsDataType::DateTime
        | TdsDataType::DateTim4
        | TdsDataType::DateTimeN
        | TdsDataType::DateTime2N => SQL_TYPE_TIMESTAMP,
        // datetimeoffset is a SQL Server-specific type with no ODBC core equivalent;
        // msodbcsql reports SQL_SS_TIMESTAMPOFFSET (-155). See:
        // https://learn.microsoft.com/en-us/sql/relational-databases/native-client-odbc-date-time/data-type-support-for-odbc-date-and-time-improvements
        TdsDataType::DateTimeOffsetN => SQL_SS_TIMESTAMPOFFSET,
        TdsDataType::Char | TdsDataType::BigChar => SQL_CHAR,
        TdsDataType::VarChar | TdsDataType::BigVarChar => SQL_VARCHAR,
        TdsDataType::Text => SQL_LONGVARCHAR,
        TdsDataType::NChar => SQL_WCHAR,
        TdsDataType::NVarChar => SQL_WVARCHAR,
        TdsDataType::NText => SQL_WLONGVARCHAR,
        TdsDataType::Binary | TdsDataType::BigBinary => SQL_BINARY,
        TdsDataType::VarBinary | TdsDataType::BigVarBinary => SQL_VARBINARY,
        TdsDataType::Image => SQL_LONGVARBINARY,
        TdsDataType::Guid => SQL_GUID,
        TdsDataType::Xml | TdsDataType::Json => SQL_WLONGVARCHAR,
        TdsDataType::Vector | TdsDataType::SsVariant | TdsDataType::Udt => SQL_VARCHAR,
        _ => SQL_UNKNOWN_TYPE,
    }
}

fn column_size(meta: &mssql_tds::query::metadata::ColumnMetadata) -> u64 {
    // PLP / `*(max)` / xml / json: ColumnSize is "unbounded". Report 0 per ODBC spec
    if meta.is_plp() {
        return 0;
    }
    match meta.data_type {
        TdsDataType::Int1 => 3,
        TdsDataType::Int2 => 5,
        TdsDataType::Int4 => 10,
        TdsDataType::Int8 => 19,
        // IntN: dispatch on wire length.
        TdsDataType::IntN => match meta.type_info.length {
            1 => 3,
            2 => 5,
            4 => 10,
            8 => 19,
            _ => 0,
        },
        TdsDataType::Bit | TdsDataType::BitN => 1,
        TdsDataType::Flt4 => 7,
        TdsDataType::Flt8 => 15,
        // FltN: 4=real (precision 7), 8=float (precision 15).
        TdsDataType::FltN => match meta.type_info.length {
            4 => 7,
            8 => 15,
            _ => 0,
        },
        TdsDataType::DateN => 10,
        TdsDataType::TimeN => {
            let scale = meta.get_scale().unwrap_or(0) as u64;
            if scale > 0 { 9 + scale } else { 8 }
        }
        // datetime: fixed scale 3, display "yyyy-mm-dd hh:mm:ss.fff" = 23 chars.
        TdsDataType::DateTime => 23,
        // smalldatetime: minute-resolution, fixed scale 0, display "yyyy-mm-dd hh:mm" = 16.
        TdsDataType::DateTim4 => 16,
        TdsDataType::DateTimeN => match meta.type_info.length {
            8 => 23,
            4 => 16,
            _ => 0,
        },
        // datetime2: "yyyy-mm-dd hh:mm:ss[.fffffff]". scale=0 → 19; scale>0 → 20 + scale.
        TdsDataType::DateTime2N => {
            let scale = meta.get_scale().unwrap_or(0) as u64;
            if scale > 0 { 20 + scale } else { 19 }
        }
        // datetimeoffset display: "yyyy-mm-dd hh:mm:ss[.fffffff] ±hh:mm".
        // scale=0 → 26; scale>0 → 27 + scale (extra '.' separator).
        TdsDataType::DateTimeOffsetN => {
            let scale = meta.get_scale().unwrap_or(0) as u64;
            if scale > 0 { 27 + scale } else { 26 }
        }
        // Decimal/Numeric/Money: ColumnSize is precision (max decimal digits).
        TdsDataType::Decimal
        | TdsDataType::DecimalN
        | TdsDataType::Numeric
        | TdsDataType::NumericN
        | TdsDataType::Money
        | TdsDataType::Money4
        | TdsDataType::MoneyN => meta.get_precision().unwrap_or(0) as u64,
        TdsDataType::NChar | TdsDataType::NVarChar | TdsDataType::NText => {
            (meta.type_info.length / 2) as u64
        }
        _ => meta.type_info.length as u64,
    }
}

fn decimal_digits(meta: &mssql_tds::query::metadata::ColumnMetadata) -> SqlSmallInt {
    match meta.data_type {
        // T-SQL `money` and `smallmoney` both have a fixed scale of 4. They are stored
        // as FixedLen/VarLen variants without a scale field, so `get_scale()` returns
        // None - hard-code the spec-mandated value here, mirroring `get_precision()`.
        TdsDataType::Money | TdsDataType::Money4 | TdsDataType::MoneyN => 4,
        TdsDataType::DateTime => 3,
        TdsDataType::DateTim4 => 0,
        TdsDataType::DateTimeN => match meta.type_info.length {
            8 => 3,
            4 => 0,
            _ => 0,
        },
        _ => meta.get_scale().unwrap_or(0) as SqlSmallInt,
    }
}

// Unit tests cover the validation/error paths only. The metadata-driven mapping
// helpers (`odbc_sql_type`, `column_size`, `decimal_digits`) cannot be exercised
// here because `mssql_tds::ColumnMetadata::type_info_variant` is `pub(crate)`
// and there is no public constructor — those branches are covered end-to-end by
// `tests/e2e/tests/describe_col_test.cpp` against a live SQL Server.
#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_HANDLE_STMT, SQL_NULL_HANDLE,
        SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;

    unsafe fn alloc_env_dbc_stmt() -> (SqlHandle, SqlHandle, SqlHandle) {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe {
                sql_set_env_attr(
                    env,
                    SQL_ATTR_ODBC_VERSION,
                    SQL_OV_ODBC3_80 as usize as *mut std::ffi::c_void,
                    0,
                )
            },
            SQL_SUCCESS
        );

        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) },
            SQL_SUCCESS
        );

        let mut stmt: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );

        (env, dbc, stmt)
    }

    unsafe fn free_all(env: SqlHandle, dbc: SqlHandle, stmt: SqlHandle) {
        unsafe {
            sql_free_handle(SQL_HANDLE_STMT, stmt);
            sql_free_handle(SQL_HANDLE_DBC, dbc);
            sql_free_handle(SQL_HANDLE_ENV, env);
        }
    }

    /// Calls `sql_describe_col_w` with default-ish out pointers. Intended for
    /// error-path tests where the values of the out params are irrelevant.
    unsafe fn describe(stmt: SqlHandle, column_number: SqlUSmallInt) -> SqlReturn {
        let mut data_type: SqlSmallInt = 0;
        let mut col_size: u64 = 0;
        let mut dec_digits: SqlSmallInt = 0;
        let mut nullable: SqlSmallInt = 0;
        unsafe {
            sql_describe_col_w(
                stmt,
                column_number,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                &mut data_type,
                &mut col_size,
                &mut dec_digits,
                &mut nullable,
            )
        }
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let rc = unsafe { describe(ptr::null_mut(), 1) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn fresh_stmt_returns_sequence_error() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        let rc = unsafe { describe(stmt, 1) };
        assert_eq!(rc, SQL_ERROR);

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert_eq!(stmt_state.diag_records.len(), 1);
        assert_eq!(stmt_state.diag_records[0].sql_state, SQLSTATE_HY010);
        drop(stmt_state);

        unsafe { free_all(env, dbc, stmt) };
    }

    #[test]
    fn column_number_zero_returns_invalid_descriptor_index() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        stmt_handle
            .inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_EXEC_CONTEXT);

        let rc = unsafe { describe(stmt, 0) };
        assert_eq!(rc, SQL_ERROR);

        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert_eq!(stmt_state.diag_records.len(), 1);
        assert_eq!(stmt_state.diag_records[0].sql_state, SQLSTATE_07009);
        drop(stmt_state);

        unsafe { free_all(env, dbc, stmt) };
    }

    #[test]
    fn column_number_past_end_returns_invalid_descriptor_index() {
        let (env, dbc, stmt) = unsafe { alloc_env_dbc_stmt() };

        // EXEC_CONTEXT is set but column_metadata is empty (e.g., the prior
        // statement was DML/DDL with zero result columns). Any column_number
        // >= 1 must yield 07009.
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        stmt_handle
            .inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_EXEC_CONTEXT);

        let rc = unsafe { describe(stmt, 1) };
        assert_eq!(rc, SQL_ERROR);

        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert_eq!(stmt_state.diag_records.len(), 1);
        assert_eq!(stmt_state.diag_records[0].sql_state, SQLSTATE_07009);
        drop(stmt_state);

        unsafe { free_all(env, dbc, stmt) };
    }
}
