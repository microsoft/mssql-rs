// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ODBC catalog functions: `SQLTables`, `SQLColumns`, `SQLPrimaryKeys`,
//! `SQLForeignKeys`, `SQLSpecialColumns`, `SQLStatistics`, `SQLProcedures`.
//!
//! SQL Server ships system stored procedures whose result sets already match
//! the ODBC catalog contract (`sp_tables`, `sp_columns_100`, `sp_pkeys`, ...),
//! and msodbcsql dispatches to them rather than hand-rolling `sys.*` queries
//! (`sqlcdd.cpp`, `DoDD()`). Doing the same here keeps column order, types,
//! and NULL semantics identical to the C++ driver for free; the remaining
//! work is call translation (argument order, catalog qualification, ODBC
//! 2.x/3.x column renaming) rather than parsing or reshaping rows — see
//! `mssql-odbc/plan.md` Phase 11.
//!
//! Every function shares one dispatch core, [`run_catalog`], because they all
//! reduce to the same shape: build positional (and occasionally named) RPC
//! parameters, call the catalog proc through the ordinary execute path so
//! cursor/metadata state is managed exactly as for a user query, then rename
//! columns and clear NOT NULL flags to match the ODBC 3.x contract
//! (`SetColNames` / `ClearNullable` in `DoDD()`).
//!
//! Deliberately out of scope, matching this driver's target of SQL Server
//! 2016+ (Katmai and later) with no Driver-Manager-mediated linked-server
//! support:
//! - Distributed/linked-server catalog queries (`@table_name` scoped to a
//!   remote `server`, the `sp_tables_ex`/`sp_columns_ex_100`/... procs, and
//!   the `sp_cursoropen`-wrapped dispatch `DoDD()` uses for them). None of the
//!   seven ODBC catalog functions even expose a `Server` argument, and
//!   mssql-python (this crate's motivating consumer) never triggers this path.
//! - `SQL_SOPT_SS_NAME_SCOPE` (table-type-scoped catalog queries) — an
//!   unimplemented statement option, so its non-default branch is unreachable.
//! - `SQL_ATTR_METADATA_ID = SQL_TRUE` (identifier mode: literal `%`/`_` in a
//!   pattern argument). This statement attribute isn't tracked anywhere in
//!   this driver yet, so it is always effectively `SQL_FALSE` (pattern mode) —
//!   the only reachable behavior, and the default/near-universal one
//!   (`@fUsePattern = 1` unconditionally below).

use tracing::{debug, error};

use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::error::Error as TdsError;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

use super::exec_common::{
    claim_connection, fail_with_tds, finish_execute, flush_pending_unprepare,
};
use super::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use super::sqlstate::{ERR_INVALID_CURSOR_STATE, post_diag};
use super::txn::begin_transaction_if_manual;
use super::util::{COLMETA_NULLABLE_FLAG, read_utf16};
use crate::error::free_errors;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, STMT_STATE_PREPARED,
};
use crate::handles::{HandleType, OdbcVersion, StmtHandle, handle_from_raw};

/// Maximum SQL Server identifier length (`SYSNAMELEN` in msodbcsql
/// `sqlcdd.cpp`). Declared length for every catalog/schema/table/column/
/// procedure-name RPC parameter.
const SYSNAME_LEN: u16 = 128;

/// Declared length for the `SQLTables` `TableType` argument, which is a
/// comma-separated list of quoted values rather than a single identifier.
const TABLE_TYPE_LEN: u16 = 4000;

/// A value that cannot match any real identifier or procedure name, used to
/// force an empty (but correctly shaped) result set. Matches msodbcsql's
/// filler value on the generic nonexistent-catalog retry path (`g_szSpace`,
/// `sqlcdd.cpp` lines 21, 1886-1889).
const UNMATCHABLE_NAME: &str = " ";

/// A decoded catalog-function argument. `None` is SQL NULL / not supplied;
/// `Some(String::new())` is a genuine zero-length value, which for the
/// ordinary (non-pattern) arguments below is an exact match against `''`
/// (matches no real identifier) rather than "no filter".
type Arg = Option<String>;

/// # Safety
/// `ptr` must be null or point to `len` readable UTF-16 code units (or be
/// NUL-terminated when `len` is `SQL_NTS`).
unsafe fn opt_arg(ptr: *const SqlWChar, len: SqlSmallInt) -> Arg {
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { read_utf16(ptr, len) })
    }
}

/// Builds an `NVARCHAR(len)` positional RPC parameter; `None` encodes SQL
/// NULL.
fn nvarchar(value: Option<&str>, len: u16) -> RpcParameter {
    let sql_value = value.map(|v| SqlString::from_utf8_string(v.to_string()));
    RpcParameter::new(None, StatusFlags::NONE, SqlType::NVarchar(sql_value, len))
}

/// Builds a named `BIT` RPC parameter (`@fUsePattern`). `name` is the bare
/// parameter name; the leading `@` TDS RPC requires is added here so callers
/// can't accidentally omit it.
fn named_bit(name: &str, value: bool) -> RpcParameter {
    RpcParameter::new(
        Some(format!("@{name}")),
        StatusFlags::NONE,
        SqlType::Bit(Some(value)),
    )
}

/// Builds the named `@ODBCVer` `TINYINT` RPC parameter msodbcsql sends for
/// `SQLColumns`/`SQLSpecialColumns` on a 3.x application (`sqlcdd.cpp` lines
/// 1809-1825); omitted for 2.x apps, matching `!IS2xAPP(lpdbc)`.
const ODBC_VER_KATMAI: u8 = 3;
fn odbc_ver_param() -> RpcParameter {
    RpcParameter::new(
        Some("@ODBCVer".to_string()),
        StatusFlags::NONE,
        SqlType::TinyInt(Some(ODBC_VER_KATMAI)),
    )
}

/// Builds the three-part qualified procedure name msodbcsql sends as the RPC
/// target when a catalog was given (`[db].sys.proc`; `sqlcdd.cpp` lines
/// 1564-1576, `fQualifierIsDB`). SQL Server's system catalog procedures are
/// visible as `sys.<name>` in every database via the Resource database, so
/// this needs no `USE` statement — matching the bare `[sys].proc` form this
/// crate already uses for `SQLGetTypeInfo` (`get_type_info::DATATYPE_INFO_PROC`)
/// when no catalog is given.
fn qualified_proc_name(catalog: &Arg, proc: &str) -> String {
    match catalog.as_deref().filter(|c| !c.is_empty()) {
        Some(db) => format!("[{}].sys.{proc}", db.replace(']', "]]")),
        None => format!("[sys].{proc}"),
    }
}

/// Renders the `SQLTables` `TableType` argument the way `sp_tables` expects:
/// a comma-separated list with each element individually single-quoted, so
/// `TABLE,VIEW` becomes `'TABLE','VIEW'` (msodbcsql `ValidateTableType`,
/// `sqlcdd.cpp` lines 448-552). `None`, blank, or a bare `%` pass through
/// unchanged so the proc's own wildcard/NULL handling applies; unlike the
/// dynamic-SQL approach this replaces, no T-SQL string-literal escaping is
/// needed here — this is RPC parameter *data*, not SQL text.
fn table_type_value(arg: &Arg) -> Arg {
    match arg {
        None => None,
        Some(v) if v.trim().is_empty() || v.trim() == "%" => arg.clone(),
        Some(v) => Some(
            v.split(',')
                .map(|t| format!("'{}'", t.trim().trim_matches('\'')))
                .collect::<Vec<_>>()
                .join(","),
        ),
    }
}

/// Whether the application declared ODBC 2.x (`SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION,
/// SQL_OV_ODBC2)`), read from the parent ENV. Selects `@ODBCVer` / `@fUsePattern`
/// inclusion exactly as `get_type_info::sql_get_type_info_w_safe` does for
/// `SQLGetTypeInfo`.
fn is_2x_app(stmt: &StmtHandle) -> bool {
    let env = stmt.parent_dbc().parent_env();
    let Ok(env_state) = env.inner.lock() else {
        error!("catalog function: env mutex poisoned reading ODBC version");
        return false;
    };
    env_state.odbc_version == OdbcVersion::Odbc2
}

/// Runs a catalog-function stored procedure and leaves its result set open
/// for `SQLFetch`, then applies the ODBC 3.x column renames and NOT NULL
/// flags msodbcsql applies via `SetColNames`/`ClearNullable` (`DoDD()`,
/// `sqlcdd.cpp` lines 1910-1913).
///
/// `catalog` scopes the call to a specific database via a three-part
/// qualified procedure name (see [`qualified_proc_name`]). If a catalog was
/// given and that qualified call fails for any reason, msodbcsql (`DoDD()`,
/// lines 1883-1895) recovers by re-running the same procedure unqualified
/// (i.e. against the *current* database) with its primary name filter forced
/// to [`UNMATCHABLE_NAME`], producing a correctly-shaped empty result set
/// instead of surfacing the object-resolution error. `build_params(true)`
/// must build that "unmatchable" parameter set. Only a server-reported SQL
/// error triggers the retry — a transport-level failure (connection drop,
/// timeout) is propagated immediately, since retrying on a dead connection
/// cannot succeed.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` allocated by
/// `SQLAllocHandle`, and `stmt` must be the handle it was decoded from.
#[allow(clippy::too_many_arguments)]
fn run_catalog(
    statement_handle: SqlHandle,
    name: &'static str,
    stmt: &StmtHandle,
    proc: &str,
    catalog: &Arg,
    build_params: impl Fn(bool) -> (Vec<RpcParameter>, Option<Vec<RpcParameter>>),
    not_null_cols: &[usize],
    renames: &[(usize, &'static str)],
) -> SqlReturn {
    let dbc = stmt.parent_dbc();

    {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("{name}: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        if stmt_state.has_state(STMT_STATE_EXEC_STARTED | STMT_STATE_CURSOR_OPEN) {
            error!("{name}: statement has an active execute or open cursor");
            post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
            return SQL_ERROR;
        }
        // A new query invalidates prior metadata/context immediately, matching
        // SQLGetTypeInfo/SQLExecDirect: a later failure cannot expose stale
        // SQLNumResultCols/DescribeCol state.
        stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.column_metadata.clear();
        stmt_state.reset_row_stream();
        stmt_state.orphan_prepared_handle();
        stmt_state.prepared = None;
        stmt_state.clear_state(STMT_STATE_PREPARED);
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
    }

    let mut client = match claim_connection(dbc, stmt, statement_handle, name) {
        Ok(client) => client,
        Err(rc) => return rc,
    };
    flush_pending_unprepare(dbc, stmt, &mut client, name);

    if let Err(e) = begin_transaction_if_manual(dbc, &mut client, name) {
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    let has_catalog = catalog.as_deref().is_some_and(|c| !c.is_empty());
    let (positional, named) = build_params(false);
    let mut exec_result = dbc.runtime.block_on(client.execute_stored_procedure(
        qualified_proc_name(catalog, proc),
        Some(positional),
        named,
        (),
    ));

    if has_catalog && matches!(exec_result, Err(TdsError::SqlServerError { .. })) {
        debug!(%proc, "{name}: qualified catalog call failed, retrying unqualified");
        let (retry_positional, retry_named) = build_params(true);
        exec_result = dbc.runtime.block_on(client.execute_stored_procedure(
            qualified_proc_name(&None, proc),
            Some(retry_positional),
            retry_named,
            (),
        ));
    }

    if let Err(e) = exec_result {
        error!(%e, "{name}: execution failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    if !client.on_rows()
        && client.has_open_batch()
        && let Err(e) = dbc.runtime.block_on(client.advance_to_rows())
    {
        error!(%e, "{name}: advancing to catalog rows failed");
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    let rc = finish_execute(dbc, stmt, statement_handle, client, name);
    if rc == SQL_ERROR {
        return rc;
    }
    apply_catalog_metadata(stmt, not_null_cols, renames);
    rc
}

/// Applies the post-execution column-metadata fixups every catalog function
/// needs: renaming the ODBC 2.x names the system procedures emit
/// (`TABLE_QUALIFIER`, `TABLE_OWNER`, ...) to their ODBC 3.x equivalents, and
/// clearing the nullable flag on the columns the ODBC specification guarantees
/// are NOT NULL — mirrors `SetColNames`/`ClearNullable` (`sqlcdd.cpp` lines
/// 1910-1913, 2412-2472).
fn apply_catalog_metadata(stmt: &StmtHandle, not_null_cols: &[usize], renames: &[(usize, &str)]) {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("catalog function: stmt mutex poisoned applying column metadata");
        return;
    };
    let cols = &mut stmt_state.column_metadata;
    for (index, new_name) in renames {
        if let Some(col) = cols.get_mut(*index) {
            col.column_name = (*new_name).to_string();
        }
    }
    for index in not_null_cols {
        if let Some(col) = cols.get_mut(*index) {
            col.flags &= !COLMETA_NULLABLE_FLAG;
        }
    }
}

// ---------------------------------------------------------------------------
// SQLTables
// ---------------------------------------------------------------------------

/// Implements `SQLTablesW`.
///
/// # Safety
/// Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_tables_w(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    table_type: *const SqlWChar,
    name_length_4: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?catalog_name,
        name_length_1,
        ?schema_name,
        name_length_2,
        ?table_name,
        name_length_3,
        ?table_type,
        name_length_4,
        "SQLTablesW called"
    );
    crate::ffi_entry!("SQLTablesW", unsafe {
        sql_tables_w_impl(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
            table_type,
            name_length_4,
        )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_tables_w_impl(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    table_type: *const SqlWChar,
    name_length_4: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLTablesW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLTablesW: handle is not a STMT"
    );

    let catalog = unsafe { opt_arg(catalog_name, name_length_1) };
    let schema = unsafe { opt_arg(schema_name, name_length_2) };
    let table = unsafe { opt_arg(table_name, name_length_3) };
    let table_type = unsafe { opt_arg(table_type, name_length_4) };

    sql_tables_w_safe(statement_handle, stmt, catalog, schema, table, table_type)
}

fn sql_tables_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    catalog: Arg,
    schema: Arg,
    table: Arg,
    table_type: Arg,
) -> SqlReturn {
    let table_type = table_type_value(&table_type);
    let build_params = |unmatchable: bool| {
        let table_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(None, SYSNAME_LEN), // table_qualifier: redundant once qualified via proc name
            nvarchar(table_type.as_deref(), TABLE_TYPE_LEN),
        ];
        // `@fUsePattern` is Yukon+ only, sent for every 2.x/3.x app since this
        // driver targets Katmai+ (`g_fYukonPatternAsParamArr[fSQLTABLES] == TRUE`,
        // `sqlcdd.cpp` line 113); `SQL_ATTR_METADATA_ID` is never TRUE (see
        // module docs), so the value is always pattern mode.
        let named = vec![named_bit("fUsePattern", true)];
        (positional, Some(named))
    };

    run_catalog(
        statement_handle,
        "SQLTablesW",
        stmt,
        "sp_tables",
        &catalog,
        build_params,
        &[],
        &[(0, "TABLE_CAT"), (1, "TABLE_SCHEM")],
    )
}

// ---------------------------------------------------------------------------
// SQLColumns
// ---------------------------------------------------------------------------

/// Implements `SQLColumnsW`.
///
/// # Safety
/// Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_columns_w(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    column_name: *const SqlWChar,
    name_length_4: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?catalog_name,
        name_length_1,
        ?schema_name,
        name_length_2,
        ?table_name,
        name_length_3,
        ?column_name,
        name_length_4,
        "SQLColumnsW called"
    );
    crate::ffi_entry!("SQLColumnsW", unsafe {
        sql_columns_w_impl(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
            column_name,
            name_length_4,
        )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_columns_w_impl(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    column_name: *const SqlWChar,
    name_length_4: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLColumnsW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLColumnsW: handle is not a STMT"
    );

    let catalog = unsafe { opt_arg(catalog_name, name_length_1) };
    let schema = unsafe { opt_arg(schema_name, name_length_2) };
    let table = unsafe { opt_arg(table_name, name_length_3) };
    let column = unsafe { opt_arg(column_name, name_length_4) };

    sql_columns_w_safe(statement_handle, stmt, catalog, schema, table, column)
}

fn sql_columns_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    catalog: Arg,
    schema: Arg,
    table: Arg,
    column: Arg,
) -> SqlReturn {
    let is_2x = is_2x_app(stmt);
    let build_params = |unmatchable: bool| {
        let table_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(None, SYSNAME_LEN),
            nvarchar(column.as_deref(), SYSNAME_LEN),
        ];
        // `@ODBCVer` / `@fUsePattern` are both sent only for 3.x apps
        // (`sqlcdd.cpp` lines 1809-1812, 1827-1843); `sp_columns_100` runs the
        // ODBC 2.x column-name/type behavior unless told otherwise.
        let named = if is_2x {
            None
        } else {
            Some(vec![odbc_ver_param(), named_bit("fUsePattern", true)])
        };
        (positional, named)
    };

    run_catalog(
        statement_handle,
        "SQLColumnsW",
        stmt,
        "sp_columns_100",
        &catalog,
        build_params,
        &[2, 3, 4, 5, 10, 13, 16],
        &[
            (0, "TABLE_CAT"),
            (1, "TABLE_SCHEM"),
            (6, "COLUMN_SIZE"),
            (7, "BUFFER_LENGTH"),
            (8, "DECIMAL_DIGITS"),
            (9, "NUM_PREC_RADIX"),
        ],
    )
}

// ---------------------------------------------------------------------------
// SQLPrimaryKeys
// ---------------------------------------------------------------------------

/// Implements `SQLPrimaryKeysW`.
///
/// # Safety
/// Each name pointer must be null or reference `*_len` readable UTF-16 units.
pub(crate) unsafe fn sql_primary_keys_w(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?catalog_name,
        name_length_1,
        ?schema_name,
        name_length_2,
        ?table_name,
        name_length_3,
        "SQLPrimaryKeysW called"
    );
    crate::ffi_entry!("SQLPrimaryKeysW", unsafe {
        sql_primary_keys_w_impl(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
        )
    })
}

unsafe fn sql_primary_keys_w_impl(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLPrimaryKeysW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLPrimaryKeysW: handle is not a STMT"
    );

    let catalog = unsafe { opt_arg(catalog_name, name_length_1) };
    let schema = unsafe { opt_arg(schema_name, name_length_2) };
    let table = unsafe { opt_arg(table_name, name_length_3) };

    sql_primary_keys_w_safe(statement_handle, stmt, catalog, schema, table)
}

fn sql_primary_keys_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    catalog: Arg,
    schema: Arg,
    table: Arg,
) -> SqlReturn {
    let build_params = |unmatchable: bool| {
        let table_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(None, SYSNAME_LEN),
        ];
        (positional, None)
    };

    run_catalog(
        statement_handle,
        "SQLPrimaryKeysW",
        stmt,
        "sp_pkeys",
        &catalog,
        build_params,
        &[2, 3, 4],
        &[(0, "TABLE_CAT"), (1, "TABLE_SCHEM")],
    )
}

// ---------------------------------------------------------------------------
// SQLForeignKeys
// ---------------------------------------------------------------------------

/// Implements `SQLForeignKeysW`.
///
/// # Safety
/// Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_foreign_keys_w(
    statement_handle: SqlHandle,
    pk_catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    pk_schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    pk_table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    fk_catalog_name: *const SqlWChar,
    name_length_4: SqlSmallInt,
    fk_schema_name: *const SqlWChar,
    name_length_5: SqlSmallInt,
    fk_table_name: *const SqlWChar,
    name_length_6: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?pk_catalog_name,
        name_length_1,
        ?pk_schema_name,
        name_length_2,
        ?pk_table_name,
        name_length_3,
        ?fk_catalog_name,
        name_length_4,
        ?fk_schema_name,
        name_length_5,
        ?fk_table_name,
        name_length_6,
        "SQLForeignKeysW called"
    );
    crate::ffi_entry!("SQLForeignKeysW", unsafe {
        sql_foreign_keys_w_impl(
            statement_handle,
            pk_catalog_name,
            name_length_1,
            pk_schema_name,
            name_length_2,
            pk_table_name,
            name_length_3,
            fk_catalog_name,
            name_length_4,
            fk_schema_name,
            name_length_5,
            fk_table_name,
            name_length_6,
        )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_foreign_keys_w_impl(
    statement_handle: SqlHandle,
    pk_catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    pk_schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    pk_table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    fk_catalog_name: *const SqlWChar,
    name_length_4: SqlSmallInt,
    fk_schema_name: *const SqlWChar,
    name_length_5: SqlSmallInt,
    fk_table_name: *const SqlWChar,
    name_length_6: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLForeignKeysW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLForeignKeysW: handle is not a STMT"
    );

    let pk_catalog = unsafe { opt_arg(pk_catalog_name, name_length_1) };
    let pk_schema = unsafe { opt_arg(pk_schema_name, name_length_2) };
    let pk_table = unsafe { opt_arg(pk_table_name, name_length_3) };
    let fk_catalog = unsafe { opt_arg(fk_catalog_name, name_length_4) };
    let fk_schema = unsafe { opt_arg(fk_schema_name, name_length_5) };
    let fk_table = unsafe { opt_arg(fk_table_name, name_length_6) };

    sql_foreign_keys_w_safe(
        statement_handle,
        stmt,
        pk_catalog,
        pk_schema,
        pk_table,
        fk_catalog,
        fk_schema,
        fk_table,
    )
}

#[allow(clippy::too_many_arguments)]
fn sql_foreign_keys_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    pk_catalog: Arg,
    pk_schema: Arg,
    pk_table: Arg,
    fk_catalog: Arg,
    fk_schema: Arg,
    fk_table: Arg,
) -> SqlReturn {
    // Both sides must resolve to one database. If only one qualifier was
    // supplied, use it for both; if both were supplied and disagree, force an
    // empty result set by making the PK table name unmatchable rather than
    // guessing which side the caller meant (msodbcsql `SQLForeignKeysW`,
    // `sqlcdd.cpp` lines 984-1008).
    let pk_has = pk_catalog.as_deref().is_some_and(|c| !c.is_empty());
    let fk_has = fk_catalog.as_deref().is_some_and(|c| !c.is_empty());
    let (catalog, catalogs_conflict) = match (pk_has, fk_has) {
        (true, true) if pk_catalog != fk_catalog => (pk_catalog.clone(), true),
        (true, _) => (pk_catalog.clone(), false),
        (false, true) => (fk_catalog.clone(), false),
        (false, false) => (None, false),
    };

    let build_params = |unmatchable: bool| {
        let pk_table_name = if unmatchable || catalogs_conflict {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            pk_table.clone()
        };
        let positional = vec![
            nvarchar(pk_table_name.as_deref(), SYSNAME_LEN),
            nvarchar(pk_schema.as_deref(), SYSNAME_LEN),
            nvarchar(None, SYSNAME_LEN),
            nvarchar(fk_table.as_deref(), SYSNAME_LEN),
            nvarchar(fk_schema.as_deref(), SYSNAME_LEN),
            nvarchar(None, SYSNAME_LEN),
        ];
        (positional, None)
    };

    run_catalog(
        statement_handle,
        "SQLForeignKeysW",
        stmt,
        "sp_fkeys",
        &catalog,
        build_params,
        &[2, 3, 6, 7],
        &[
            (0, "PKTABLE_CAT"),
            (1, "PKTABLE_SCHEM"),
            (4, "FKTABLE_CAT"),
            (5, "FKTABLE_SCHEM"),
        ],
    )
}

// ---------------------------------------------------------------------------
// SQLStatistics
// ---------------------------------------------------------------------------

/// Implements `SQLStatisticsW`.
///
/// # Safety
/// Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_statistics_w(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    unique: SqlUSmallInt,
    reserved: SqlUSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?catalog_name,
        name_length_1,
        ?schema_name,
        name_length_2,
        ?table_name,
        name_length_3,
        unique,
        reserved,
        "SQLStatisticsW called"
    );
    crate::ffi_entry!("SQLStatisticsW", unsafe {
        sql_statistics_w_impl(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
            unique,
            reserved,
        )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_statistics_w_impl(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    unique: SqlUSmallInt,
    reserved: SqlUSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLStatisticsW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLStatisticsW: handle is not a STMT"
    );

    let catalog = unsafe { opt_arg(catalog_name, name_length_1) };
    let schema = unsafe { opt_arg(schema_name, name_length_2) };
    let table = unsafe { opt_arg(table_name, name_length_3) };

    sql_statistics_w_safe(
        statement_handle,
        stmt,
        catalog,
        schema,
        table,
        unique,
        reserved,
    )
}

fn sql_statistics_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    catalog: Arg,
    schema: Arg,
    table: Arg,
    unique: SqlUSmallInt,
    reserved: SqlUSmallInt,
) -> SqlReturn {
    // SQL_INDEX_UNIQUE (0) selects unique indexes only; SQL_QUICK (0) permits
    // cached cardinality, SQL_ENSURE (1) forces a scan.
    let is_unique = if unique == super::odbc_types::SQL_INDEX_UNIQUE {
        "Y"
    } else {
        "N"
    };
    let accuracy = if reserved == super::odbc_types::SQL_ENSURE {
        "E"
    } else {
        "Q"
    };

    let build_params = |unmatchable: bool| {
        let table_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(None, SYSNAME_LEN),
            // `SQLStatistics` has no per-index argument at the ODBC API level;
            // msodbcsql always passes '%' here since `sp_statistics_100` filters
            // `index_name LIKE @index_name`, which matches nothing on NULL
            // (`sqlcdd.cpp` line 737).
            nvarchar(Some("%"), 1),
            nvarchar(Some(is_unique), 1),
            nvarchar(Some(accuracy), 1),
        ];
        (positional, None)
    };

    run_catalog(
        statement_handle,
        "SQLStatisticsW",
        stmt,
        "sp_statistics_100",
        &catalog,
        build_params,
        &[],
        &[
            (0, "TABLE_CAT"),
            (1, "TABLE_SCHEM"),
            (7, "ORDINAL_POSITION"),
            (9, "ASC_OR_DESC"),
        ],
    )
}

// ---------------------------------------------------------------------------
// SQLSpecialColumns
// ---------------------------------------------------------------------------

/// Implements `SQLSpecialColumnsW`.
///
/// # Safety
/// Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_special_columns_w(
    statement_handle: SqlHandle,
    identifier_type: SqlSmallInt,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    scope: SqlSmallInt,
    nullable: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        identifier_type,
        ?catalog_name,
        name_length_1,
        ?schema_name,
        name_length_2,
        ?table_name,
        name_length_3,
        scope,
        nullable,
        "SQLSpecialColumnsW called"
    );
    crate::ffi_entry!("SQLSpecialColumnsW", unsafe {
        sql_special_columns_w_impl(
            statement_handle,
            identifier_type,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            table_name,
            name_length_3,
            scope,
            nullable,
        )
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn sql_special_columns_w_impl(
    statement_handle: SqlHandle,
    identifier_type: SqlSmallInt,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    scope: SqlSmallInt,
    nullable: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLSpecialColumnsW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLSpecialColumnsW: handle is not a STMT"
    );

    let catalog = unsafe { opt_arg(catalog_name, name_length_1) };
    let schema = unsafe { opt_arg(schema_name, name_length_2) };
    let table = unsafe { opt_arg(table_name, name_length_3) };

    sql_special_columns_w_safe(
        statement_handle,
        stmt,
        identifier_type,
        catalog,
        schema,
        table,
        scope,
        nullable,
    )
}

#[allow(clippy::too_many_arguments)]
fn sql_special_columns_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    identifier_type: SqlSmallInt,
    catalog: Arg,
    schema: Arg,
    table: Arg,
    scope: SqlSmallInt,
    nullable: SqlSmallInt,
) -> SqlReturn {
    use super::odbc_types::{
        SQL_BEST_ROWID, SQL_NO_NULLS, SQL_SCOPE_CURROW, SQL_SCOPE_TRANSACTION, SQL_TXN_SERIALIZABLE,
    };

    let col_type = if identifier_type == SQL_BEST_ROWID {
        "R"
    } else {
        "V"
    };
    let scope_char = if scope == SQL_SCOPE_CURROW { "C" } else { "T" };
    let nullable_char = if nullable == SQL_NO_NULLS { "O" } else { "U" };

    // A ROWID's uniqueness cannot be guaranteed beyond the current row unless
    // the requested scope is a serializable transaction, so the ODBC
    // specification requires an empty result set for any wider scope outside
    // one (msodbcsql `SQLSpecialColumnsW`, `sqlcdd.cpp` lines 828-837).
    let txn_isolation = {
        let Ok(dbc_state) = stmt.parent_dbc().inner.lock() else {
            error!("SQLSpecialColumnsW: dbc mutex poisoned reading isolation level");
            return SQL_ERROR;
        };
        dbc_state.txn_isolation
    };
    let force_empty = identifier_type == SQL_BEST_ROWID
        && scope != SQL_SCOPE_CURROW
        && (scope != SQL_SCOPE_TRANSACTION || txn_isolation != SQL_TXN_SERIALIZABLE);

    let is_2x = is_2x_app(stmt);
    let build_params = move |unmatchable: bool| {
        let table_name = if unmatchable || force_empty {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(None, SYSNAME_LEN),
            nvarchar(Some(col_type), 1),
            nvarchar(Some(scope_char), 1),
            nvarchar(Some(nullable_char), 1),
        ];
        let named = if is_2x {
            None
        } else {
            Some(vec![odbc_ver_param()])
        };
        (positional, named)
    };

    run_catalog(
        statement_handle,
        "SQLSpecialColumnsW",
        stmt,
        "sp_special_columns_100",
        &catalog,
        build_params,
        &[1, 2, 3],
        &[
            (4, "COLUMN_SIZE"),
            (5, "BUFFER_LENGTH"),
            (6, "DECIMAL_DIGITS"),
        ],
    )
}

// ---------------------------------------------------------------------------
// SQLProcedures
// ---------------------------------------------------------------------------

/// Implements `SQLProceduresW`.
///
/// # Safety
/// Each name pointer must be null or reference `*_len` readable UTF-16 units.
pub(crate) unsafe fn sql_procedures_w(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    proc_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        ?catalog_name,
        name_length_1,
        ?schema_name,
        name_length_2,
        ?proc_name,
        name_length_3,
        "SQLProceduresW called"
    );
    crate::ffi_entry!("SQLProceduresW", unsafe {
        sql_procedures_w_impl(
            statement_handle,
            catalog_name,
            name_length_1,
            schema_name,
            name_length_2,
            proc_name,
            name_length_3,
        )
    })
}

unsafe fn sql_procedures_w_impl(
    statement_handle: SqlHandle,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    proc_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLProceduresW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLProceduresW: handle is not a STMT"
    );

    let catalog = unsafe { opt_arg(catalog_name, name_length_1) };
    let schema = unsafe { opt_arg(schema_name, name_length_2) };
    let proc = unsafe { opt_arg(proc_name, name_length_3) };

    sql_procedures_w_safe(statement_handle, stmt, catalog, schema, proc)
}

fn sql_procedures_w_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    catalog: Arg,
    schema: Arg,
    proc: Arg,
) -> SqlReturn {
    let build_params = |unmatchable: bool| {
        let proc_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            proc.clone()
        };
        let positional = vec![
            nvarchar(proc_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(None, SYSNAME_LEN),
        ];
        // `SQLProcedures` supports pattern arguments Yukon+
        // (`g_fYukonPatternAsParamArr[fSQLPROCEDURES] == TRUE`, `sqlcdd.cpp`
        // line 122); see `SQLTablesW` for why `SQL_ATTR_METADATA_ID` never
        // changes this to `false`.
        let named = vec![named_bit("fUsePattern", true)];
        (positional, Some(named))
    };

    run_catalog(
        statement_handle,
        "SQLProceduresW",
        stmt,
        "sp_stored_procedures",
        &catalog,
        build_params,
        &[2],
        &[(0, "PROCEDURE_CAT"), (1, "PROCEDURE_SCHEM")],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::TestHandles;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn named_bit_includes_at_prefix() {
        // TDS RPC names are written to the wire verbatim (no automatic `@`),
        // so the helper must add it — a bare "fUsePattern" would silently
        // fail to bind server-side.
        let debug = format!("{:?}", named_bit("fUsePattern", true));
        assert!(
            debug.contains("\"@fUsePattern\""),
            "expected an @-prefixed parameter name, got: {debug}"
        );
    }

    #[test]
    fn odbc_ver_param_includes_at_prefix() {
        let debug = format!("{:?}", odbc_ver_param());
        assert!(
            debug.contains("\"@ODBCVer\""),
            "expected an @-prefixed parameter name, got: {debug}"
        );
    }

    #[test]
    fn qualified_proc_name_bare_when_no_catalog() {
        assert_eq!(qualified_proc_name(&None, "sp_tables"), "[sys].sp_tables");
        assert_eq!(
            qualified_proc_name(&Some(String::new()), "sp_tables"),
            "[sys].sp_tables"
        );
    }

    #[test]
    fn qualified_proc_name_qualifies_with_catalog() {
        assert_eq!(
            qualified_proc_name(&Some("MyDb".to_string()), "sp_tables"),
            "[MyDb].sys.sp_tables"
        );
    }

    #[test]
    fn qualified_proc_name_escapes_bracket_in_catalog() {
        assert_eq!(
            qualified_proc_name(&Some("we]ird".to_string()), "sp_tables"),
            "[we]]ird].sys.sp_tables"
        );
    }

    #[test]
    fn table_type_value_quotes_each_element_individually() {
        assert_eq!(
            table_type_value(&Some("TABLE,VIEW".to_string())),
            Some("'TABLE','VIEW'".to_string())
        );
    }

    #[test]
    fn table_type_value_passes_wildcard_through() {
        assert_eq!(
            table_type_value(&Some("%".to_string())),
            Some("%".to_string())
        );
    }

    #[test]
    fn table_type_value_treats_blank_as_passthrough() {
        assert_eq!(
            table_type_value(&Some("   ".to_string())),
            Some("   ".to_string())
        );
        assert_eq!(table_type_value(&None), None);
    }

    #[test]
    fn table_type_value_strips_pre_quoted_elements() {
        assert_eq!(
            table_type_value(&Some("'TABLE' , 'VIEW'".to_string())),
            Some("'TABLE','VIEW'".to_string())
        );
    }

    #[test]
    fn tables_null_handle_is_invalid_handle() {
        let name = w("t");
        let ret = unsafe {
            sql_tables_w(
                SQL_NULL_HANDLE,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                name.as_ptr(),
                SQL_NTS_TEST,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn columns_null_handle_is_invalid_handle() {
        let ret = unsafe {
            sql_columns_w(
                SQL_NULL_HANDLE,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn primary_keys_null_handle_is_invalid_handle() {
        let ret = unsafe {
            sql_primary_keys_w(
                SQL_NULL_HANDLE,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn foreign_keys_null_handle_is_invalid_handle() {
        let ret = unsafe {
            sql_foreign_keys_w(
                SQL_NULL_HANDLE,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn statistics_null_handle_is_invalid_handle() {
        let ret = unsafe {
            sql_statistics_w(
                SQL_NULL_HANDLE,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn special_columns_null_handle_is_invalid_handle() {
        let ret = unsafe {
            sql_special_columns_w(
                SQL_NULL_HANDLE,
                1,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn procedures_null_handle_is_invalid_handle() {
        let ret = unsafe {
            sql_procedures_w(
                SQL_NULL_HANDLE,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn disconnected_dbc_returns_error_for_each_function() {
        let h = TestHandles::with_env_dbc_stmt();
        let name = w("t");
        assert_eq!(
            unsafe {
                sql_tables_w(
                    h.stmt,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    name.as_ptr(),
                    SQL_NTS_TEST,
                    std::ptr::null(),
                    0,
                )
            },
            SQL_ERROR
        );
        assert_eq!(
            unsafe {
                sql_procedures_w(
                    h.stmt,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                )
            },
            SQL_ERROR
        );
    }

    #[test]
    fn open_cursor_returns_24000() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().set_state(STMT_STATE_CURSOR_OPEN);

        let ret = unsafe {
            sql_tables_w(
                h.stmt,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state,
            crate::api::sqlstate::SQLSTATE_24000
        );
    }

    /// `SQL_NTS` re-declared locally to avoid pulling in the full
    /// `odbc_types` glob just for this constant in tests.
    const SQL_NTS_TEST: SqlSmallInt = super::super::odbc_types::SQL_NTS;
}
