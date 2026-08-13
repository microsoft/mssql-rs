// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetTypeInfoW — report the data types supported by the
//! data source.
//!
//! Mirrors msodbcsql: the type-info result set is produced by executing the
//! `sp_datatype_info_*` catalog procedure as an RPC and leaving the cursor open
//! for `SQLFetch`/`SQLGetData`. The requested SQL type is validated client-side
//! first (so an invalid type yields HY004 before any I/O), matching the
//! reference driver so this crate is a drop-in replacement behind the same
//! Driver Manager.

use tracing::{debug, error};

use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

use super::exec_common::{
    claim_connection, fail_with_tds, finish_execute, flush_pending_unprepare,
};
use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ALL_TYPES, SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_CHAR, SQL_DATETIME, SQL_DECIMAL,
    SQL_DOUBLE, SQL_ERROR, SQL_FLOAT, SQL_GUID, SQL_INTEGER, SQL_INVALID_HANDLE, SQL_LONGVARBINARY,
    SQL_LONGVARCHAR, SQL_NUMERIC, SQL_REAL, SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET,
    SQL_SS_UDT, SQL_SS_VARIANT, SQL_SS_XML, SQL_TIME, SQL_TIMESTAMP, SQL_TINYINT, SQL_TYPE_DATE,
    SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR,
    SQL_WVARCHAR, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::error::free_errors;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, STMT_STATE_PREPARED,
};
use crate::handles::{HandleType, OdbcVersion, StmtHandle, handle_from_raw};

/// Catalog procedure returning the ODBC `SQLGetTypeInfo` result set. This
/// driver targets SQL Server 2016+, so the Katmai (`_100`) form is always
/// available; selecting `_90`/`_170` by negotiated server version and vector
/// support is deferred until that version is surfaced to the ODBC layer.
const DATATYPE_INFO_PROC: &str = "[sys].sp_datatype_info_100";

/// `@ODBCVer` value sent for ODBC 3.x applications against a Katmai+ server.
const ODBC_VER_KATMAI: u8 = 3;

/// Offset between the ODBC 3.x concise date/time type ids (91–93) and their
/// ODBC 2.x equivalents (9–11); msodbcsql sends the 2.x form to the catalog
/// proc for 2.x applications.
const ODBC2_DATETIME_OFFSET: SqlSmallInt = SQL_TYPE_DATE - SQL_DATETIME;

/// Bit 0 of the COLMETADATA flags word marks a column nullable (`fNullable`).
const COLMETA_NULLABLE_FLAG: u16 = 0x01;

/// 1-based ODBC ordinals of the `SQLGetTypeInfo` columns the ODBC specification
/// defines as NOT NULL. msodbcsql clears their nullable flag so `SQLDescribeCol`
/// reports `SQL_NO_NULLS` for them.
const TYPE_INFO_NOT_NULL_COLUMNS: [usize; 7] = [1, 2, 7, 8, 9, 11, 16];

/// Implementation of `SQLGetTypeInfoW`.
///
/// # Safety
/// - `statement_handle` must be a valid `StmtHandle` allocated by `SQLAllocHandle`.
pub(crate) unsafe fn sql_get_type_info_w(
    statement_handle: SqlHandle,
    data_type: SqlSmallInt,
) -> SqlReturn {
    debug!(?statement_handle, data_type, "SQLGetTypeInfoW called");

    crate::ffi_entry!("SQLGetTypeInfoW", unsafe {
        sql_get_type_info_w_impl(statement_handle, data_type)
    })
}

unsafe fn sql_get_type_info_w_impl(
    statement_handle: SqlHandle,
    data_type: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLGetTypeInfoW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLGetTypeInfoW: handle is not a STMT"
    );

    sql_get_type_info_w_safe(statement_handle, stmt, data_type)
}

fn sql_get_type_info_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    data_type: SqlSmallInt,
) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    // The ODBC version selects `@ODBCVer` and the 2.x date/time remap. Read it
    // up front (env lock released immediately) to preserve parent-before-child
    // lock ordering.
    let odbc_version = {
        let env = dbc.parent_env();
        let Ok(env_state) = env.inner.lock() else {
            error!("SQLGetTypeInfoW: env mutex poisoned");
            return SQL_ERROR;
        };
        env_state.odbc_version
    };
    let is_2x_app = odbc_version == OdbcVersion::Odbc2;

    // Validate the requested type and reset prior context under the stmt lock.
    // Validation runs before any state mutation so an invalid type leaves the
    // statement unchanged, matching msodbcsql.
    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLGetTypeInfoW: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        // The cursor/exec state is checked before the data type, matching
        // msodbcsql (sqlcdd.cpp): an open cursor yields 24000 even for an invalid
        // type. The Driver Manager likewise rejects an open cursor with 24000
        // before the call reaches the driver.
        if stmt_state.has_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN) {
            error!("SQLGetTypeInfoW: statement has an active execute or open cursor");
            post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
            return SQL_ERROR;
        }

        match classify_sql_type(data_type) {
            TypeClass::Valid => {}
            TypeClass::Udt => {
                error!(data_type, "SQLGetTypeInfoW: UDT types are not reported");
                post_diag(&mut stmt_state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
                return SQL_ERROR;
            }
            TypeClass::Invalid => {
                error!(data_type, "SQLGetTypeInfoW: invalid SQL data type");
                post_diag(&mut stmt_state, ERR_INVALID_SQL_DATA_TYPE);
                return SQL_ERROR;
            }
        }

        // A new query invalidates prior metadata/context immediately, so a later
        // failure cannot expose stale SQLNumResultCols/DescribeCol state.
        stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.column_metadata.clear();
        stmt_state.reset_row_stream();
        // A cached prepared plan is superseded; release its server handle
        // (deferred) once we hold the client below.
        stmt_state.orphan_prepared_handle();
        stmt_state.prepared = None;
        stmt_state.clear_state(STMT_STATE_PREPARED);
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
    }

    // `@data_type` is positional; 2.x applications receive the 2.x date/time id.
    let positional = vec![RpcParameter::new(
        None,
        StatusFlags::NONE,
        SqlType::SmallInt(Some(datatype_info_arg(data_type, is_2x_app))),
    )];
    // `@ODBCVer` is named and sent only for 3.x applications (matching
    // msodbcsql's `!IS2xAPP` guard).
    let named = if is_2x_app {
        None
    } else {
        Some(vec![RpcParameter::new(
            Some("@ODBCVer".to_string()),
            StatusFlags::NONE,
            SqlType::TinyInt(Some(ODBC_VER_KATMAI)),
        )])
    };

    let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLGetTypeInfoW") {
        Ok(client) => client,
        Err(rc) => return rc,
    };

    // Release any handle orphaned by the reset above before running the RPC.
    flush_pending_unprepare(dbc, stmt, &mut client, "SQLGetTypeInfoW");

    let exec_result = dbc.runtime.block_on(client.execute_stored_procedure(
        DATATYPE_INFO_PROC.to_string(),
        Some(positional),
        named,
        (),
    ));
    if let Err(e) = exec_result {
        error!(%e, "SQLGetTypeInfoW: execution failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    // The catalog proc builds its output through internal statements, so in
    // statement-wise navigation the type-info SELECT can be preceded by no-row
    // results (e.g. an internal DML count). Collapse them to the first
    // row-returning result so `SQLGetTypeInfo` exposes the single type-info
    // result set, matching msodbcsql.
    if !client.on_rows()
        && client.has_open_batch()
        && let Err(e) = dbc.runtime.block_on(client.advance_to_rows())
    {
        error!(%e, "SQLGetTypeInfoW: advancing to type-info rows failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    let rc = finish_execute(dbc, stmt, statement_handle, client, "SQLGetTypeInfoW");
    if rc == SQL_ERROR {
        return rc;
    }
    rename_type_info_columns(stmt, is_2x_app);
    clear_type_info_nullable(stmt);
    rc
}

/// The `@data_type` argument sent to the catalog proc. ODBC 2.x applications
/// receive the legacy 2.x date/time id (msodbcsql remaps the concise 3.x forms
/// 91–93 down to 9–11); every other case is passed through unchanged.
fn datatype_info_arg(data_type: SqlSmallInt, is_2x_app: bool) -> SqlSmallInt {
    if is_2x_app && (SQL_TYPE_DATE..=SQL_TYPE_TIMESTAMP).contains(&data_type) {
        data_type - ODBC2_DATETIME_OFFSET
    } else {
        data_type
    }
}

/// Outcome of validating a caller-supplied `SQLGetTypeInfo` `DataType`.
enum TypeClass {
    /// A supported SQL type, or `SQL_ALL_TYPES` — run the catalog proc.
    Valid,
    /// A user-defined type — reported as HYC00 (not surfaced as an ODBC type).
    Udt,
    /// Not a recognized SQL type — reported as HY004.
    Invalid,
}

/// Classifies a `DataType` argument the same way msodbcsql does before issuing
/// the catalog RPC: `SQL_ALL_TYPES` and every base/SS type the driver reports
/// (including `sql_variant`) are valid, the CLR user-defined type id
/// (`SQL_SS_UDT`) is HYC00, and anything else is HY004. Both the ODBC 2.x
/// (`9`/`10`/`11`) and 3.x (`91`/`92`/`93`) date/time forms are accepted so 2.x
/// and 3.x applications are handled uniformly.
fn classify_sql_type(data_type: SqlSmallInt) -> TypeClass {
    match data_type {
        SQL_ALL_TYPES
        | SQL_CHAR
        | SQL_NUMERIC
        | SQL_DECIMAL
        | SQL_INTEGER
        | SQL_SMALLINT
        | SQL_FLOAT
        | SQL_REAL
        | SQL_DOUBLE
        | SQL_VARCHAR
        | SQL_LONGVARCHAR
        | SQL_BINARY
        | SQL_VARBINARY
        | SQL_LONGVARBINARY
        | SQL_BIGINT
        | SQL_TINYINT
        | SQL_BIT
        | SQL_WCHAR
        | SQL_WVARCHAR
        | SQL_WLONGVARCHAR
        | SQL_GUID
        | SQL_DATETIME
        | SQL_TIME
        | SQL_TIMESTAMP
        | SQL_TYPE_DATE
        | SQL_TYPE_TIME
        | SQL_TYPE_TIMESTAMP
        | SQL_SS_TIME2
        | SQL_SS_TIMESTAMPOFFSET
        | SQL_SS_VARIANT
        | SQL_SS_XML => TypeClass::Valid,
        // Unlike the SS types above, SQL_SS_UDT has no internal "MAPPED" form,
        // so msodbcsql's `fSqlTypeT <= SQL_TYPE_DRIVER_START` guard sends it to
        // HYC00 — UDTs are not surfaced as ODBC data types.
        SQL_SS_UDT => TypeClass::Udt,
        _ => TypeClass::Invalid,
    }
}

/// ODBC column names for the three type-info ordinals (3, 11, 12) that
/// `sp_datatype_info_*` emits under generic names. ODBC 2.x and 3.x applications
/// expect different names for these columns, so the choice mirrors the
/// application's declared ODBC version — matching msodbcsql's version-aware
/// `SetColNames` post-processing.
fn type_info_column_names(is_2x_app: bool) -> [&'static str; 3] {
    if is_2x_app {
        ["PRECISION", "MONEY", "AUTO_INCREMENT"]
    } else {
        ["COLUMN_SIZE", "FIXED_PREC_SCALE", "AUTO_UNIQUE_VALUE"]
    }
}

/// Zero-based column indices (for the 1-based ODBC ordinals 3, 11, 12) paired
/// with the version-appropriate name each should take.
fn type_info_column_renames(is_2x_app: bool) -> [(usize, &'static str); 3] {
    let [col3, col11, col12] = type_info_column_names(is_2x_app);
    [(2, col3), (10, col11), (11, col12)]
}

/// Renames the three catalog-proc columns (ODBC ordinals 3, 11, 12) to the names
/// the application's ODBC version expects, matching msodbcsql's
/// `SetColNames(COL(3)|COL(11)|COL(12), ...)` post-processing so `SQLDescribeCol`
/// reports identical column names.
fn rename_type_info_columns(stmt: &StmtHandle, is_2x_app: bool) {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetTypeInfoW: stmt mutex poisoned renaming columns");
        return;
    };
    let cols = &mut stmt_state.column_metadata;
    for (idx, name) in type_info_column_renames(is_2x_app) {
        if let Some(col) = cols.get_mut(idx) {
            col.column_name = name.to_string();
        }
    }
}

/// Clears the nullable flag on the type-info columns the ODBC spec guarantees
/// are NOT NULL, matching msodbcsql's `ClearNullable` post-processing so
/// `SQLDescribeCol` reports `SQL_NO_NULLS` for them.
fn clear_type_info_nullable(stmt: &StmtHandle) {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetTypeInfoW: stmt mutex poisoned clearing nullable");
        return;
    };
    let cols = &mut stmt_state.column_metadata;
    for ordinal in TYPE_INFO_NOT_NULL_COLUMNS {
        if let Some(col) = cols.get_mut(ordinal - 1) {
            col.flags &= !COLMETA_NULLABLE_FLAG;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe { sql_get_type_info_w(SQL_NULL_HANDLE, SQL_ALL_TYPES) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn exported_wrapper_forwards_to_impl() {
        // Exercise the extern "C" entrypoint (init_tracing + delegation) rather
        // than the inner impl the other tests call directly.
        let null = unsafe { crate::api::exports::SQLGetTypeInfoW(SQL_NULL_HANDLE, SQL_ALL_TYPES) };
        assert_eq!(null, SQL_INVALID_HANDLE);

        // An invalid type is rejected before any I/O, so no connection is needed.
        let h = TestHandles::with_env_dbc_stmt();
        let invalid = unsafe { crate::api::exports::SQLGetTypeInfoW(h.stmt, 999) };
        assert_eq!(invalid, SQL_ERROR);
    }

    #[test]
    fn type_info_column_names_are_version_aware() {
        // ODBC 3.x names (the mssql-python swap target).
        assert_eq!(
            type_info_column_names(false),
            ["COLUMN_SIZE", "FIXED_PREC_SCALE", "AUTO_UNIQUE_VALUE"]
        );
        // ODBC 2.x apps expect the legacy names for the same ordinals.
        assert_eq!(
            type_info_column_names(true),
            ["PRECISION", "MONEY", "AUTO_INCREMENT"]
        );
    }

    #[test]
    fn type_info_column_renames_pair_ordinals_with_version_names() {
        // 3.x apps: the generic proc columns take the 3.x ODBC names at the
        // zero-based indices for ordinals 3/11/12.
        assert_eq!(
            type_info_column_renames(false),
            [
                (2, "COLUMN_SIZE"),
                (10, "FIXED_PREC_SCALE"),
                (11, "AUTO_UNIQUE_VALUE")
            ]
        );
        // 2.x apps keep the same ordinals but the legacy names.
        assert_eq!(
            type_info_column_renames(true),
            [(2, "PRECISION"), (10, "MONEY"), (11, "AUTO_INCREMENT")]
        );
    }

    #[test]
    fn datatype_info_arg_remaps_only_for_odbc2_dates() {
        // 3.x apps forward every id unchanged, including the concise date forms.
        assert_eq!(datatype_info_arg(SQL_TYPE_DATE, false), SQL_TYPE_DATE);
        // 2.x apps remap the concise 3.x date/time ids down to the legacy forms.
        assert_eq!(
            datatype_info_arg(SQL_TYPE_DATE, true),
            SQL_TYPE_DATE - ODBC2_DATETIME_OFFSET
        );
        assert_eq!(
            datatype_info_arg(SQL_TYPE_TIMESTAMP, true),
            SQL_TYPE_TIMESTAMP - ODBC2_DATETIME_OFFSET
        );
        // A non-date id is untouched even for a 2.x app.
        assert_eq!(datatype_info_arg(SQL_INTEGER, true), SQL_INTEGER);
    }

    #[test]
    fn rename_type_info_columns_is_a_noop_without_metadata() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        // With no result set, the rename walks the empty metadata and mutates
        // nothing, for both ODBC versions, without panicking.
        rename_type_info_columns(stmt, false);
        rename_type_info_columns(stmt, true);
        assert!(stmt.inner.lock().unwrap().column_metadata.is_empty());
    }

    #[test]
    fn clear_type_info_nullable_is_a_noop_without_metadata() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        // No result set: the walk over the not-null ordinals mutates nothing and
        // does not panic on the empty metadata.
        clear_type_info_nullable(stmt);
        assert!(stmt.inner.lock().unwrap().column_metadata.is_empty());
    }

    #[test]
    fn odbc2_app_omits_odbc_ver_and_remaps_date() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.parent_dbc()
            .parent_env()
            .inner
            .lock()
            .unwrap()
            .odbc_version = OdbcVersion::Odbc2;
        // A 2.x app requesting a concise date type exercises the `is_2x_app`
        // remap and the omitted `@ODBCVer` branch; disconnected so it stops at
        // claim_connection.
        let ret = unsafe { sql_get_type_info_w(h.stmt, SQL_TYPE_DATE) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn invalid_data_type_returns_hy004() {
        let h = TestHandles::with_env_dbc_stmt();
        // 999 is not a recognized SQL type; validation must reject it before I/O.
        let ret = unsafe { sql_get_type_info_w(h.stmt, 999) };
        assert_eq!(ret, SQL_ERROR);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HY004);
        // A rejected type must leave the statement unchanged.
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    #[test]
    fn udt_data_type_returns_hyc00() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_get_type_info_w(h.stmt, SQL_SS_UDT) };
        assert_eq!(ret, SQL_ERROR);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_HYC00);
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
    }

    #[test]
    fn disconnected_dbc_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        // Valid type, but the connection is not established.
        let ret = unsafe { sql_get_type_info_w(h.stmt, SQL_ALL_TYPES) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn open_cursor_returns_24000() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().set_state(STMT_STATE_CURSOR_OPEN);

        let ret = unsafe { sql_get_type_info_w(h.stmt, SQL_ALL_TYPES) };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_24000);
    }

    #[test]
    fn open_cursor_wins_over_invalid_type() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().set_state(STMT_STATE_CURSOR_OPEN);

        // msodbcsql checks cursor state before the data type, so an open cursor
        // reports 24000 even when the type is also invalid.
        let ret = unsafe { sql_get_type_info_w(h.stmt, 999) };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records[0].sql_state, SQLSTATE_24000);
    }
}
