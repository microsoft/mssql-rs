// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ODBC catalog functions.
//!
//! SQL Server ships stored procedures whose result sets already match the ODBC
//! catalog contract (`sp_tables`, `sp_columns_100`, `sp_pkeys`, ...), and
//! msodbcsql dispatches to them rather than hand-rolling `sys.*` queries.
//! Doing the same here keeps column order, types, and NULL semantics identical
//! to the C++ driver for free.

use tracing::{debug, error};

use super::exec_common::{
    claim_connection, fail_with_tds, finish_execute, flush_pending_unprepare,
};
use super::odbc_types::{
    SQL_ERROR, SQL_INVALID_HANDLE, SqlHandle, SqlReturn, SqlSmallInt, SqlWChar,
};
use super::sqlstate::{ERR_INVALID_CURSOR_STATE, post_diag};
use super::util::read_utf16;
use crate::error::free_errors;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, STMT_STATE_PREPARED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// A catalog argument: absent (`NULL`) or a literal value.
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

/// Renders the `SQLTables` table-type list the way `sp_tables` expects it:
/// each element of the comma-separated list individually quoted, so `TABLE,VIEW`
/// becomes `N'''TABLE'',''VIEW'''`. Mirrors `ValidateTableType` in msodbcsql.
fn table_type_literal(arg: &Arg) -> String {
    match arg {
        None => "NULL".to_string(),
        Some(v) if v.trim().is_empty() => "NULL".to_string(),
        Some(v) if v.trim() == "%" => "N'%'".to_string(),
        Some(v) => {
            let quoted = v
                .split(',')
                .map(|t| format!("''{}''", t.trim().trim_matches('\'').replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");
            format!("N'{quoted}'")
        }
    }
}

/// Renders a catalog argument as a T-SQL literal, escaping embedded quotes.
fn literal(arg: &Arg) -> String {
    match arg {
        None => "NULL".to_string(),
        Some(v) => format!("N'{}'", v.replace('\'', "''")),
    }
}

/// Builds `EXEC [catalog].sys.<proc> <args>`.
///
/// Catalog scoping matters: `sp_tables` only sees the current database, so a
/// non-empty qualifier has to be turned into a three-part procedure name.
///
/// A qualifier naming a database that does not exist must produce an empty
/// result set rather than an error, so the batch is guarded by `DB_ID` and
/// falls back to running the same procedure locally with an unmatchable object
/// name. That keeps the column shape identical while returning no rows.
fn build_exec(catalog: &Arg, proc_name: &str, args: &[String]) -> String {
    let Some(db) = catalog.as_deref().filter(|db| !db.is_empty()) else {
        return format!("EXEC sys.{} {}", proc_name, args.join(", "));
    };
    let qualified = format!("[{}].sys.{}", db.replace(']', "]]"), proc_name);
    let mut empty_args = args.to_vec();
    if let Some(first) = empty_args.first_mut() {
        let prefix = first
            .find('=')
            .filter(|_| first.starts_with('@'))
            .map(|i| first[..=i].to_string())
            .unwrap_or_default();
        *first = format!("{prefix}N'\u{1}no such object\u{1}'");
    }
    format!(
        "IF DB_ID(N'{}') IS NULL EXEC sys.{} {} ELSE EXEC {} {}",
        db.replace('\'', "''"),
        proc_name,
        empty_args.join(", "),
        qualified,
        args.join(", ")
    )
}

fn build_exec_named(catalog: &Arg, proc_name: &str, args: &[(&str, String)]) -> String {
    let named = args
        .iter()
        .map(|(name, value)| format!("@{name} = {value}"))
        .collect::<Vec<_>>();
    build_exec(catalog, proc_name, &named)
}

/// Shared entry: validate the handle, then run the generated catalog batch
/// through the ordinary direct-execution path so cursor/metadata state is
/// managed exactly as for a user query.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
unsafe fn run_catalog(
    statement_handle: SqlHandle,
    name: &str,
    sql: String,
    renames: &[(usize, &str)],
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("{name}: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    debug!(%sql, "{name}: executing catalog query");
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
        stmt_state.clear_state(STMT_STATE_EXEC_CONTEXT);
        stmt_state.column_metadata.clear();
        stmt_state.reset_rows();
        stmt_state.row_count = -1;
        stmt_state.pending_row_counts.clear();
        stmt_state.prepared_sql = None;
        stmt_state.described_params = None;
        stmt_state.orphan_prepared_handle();
        stmt_state.clear_state(STMT_STATE_PREPARED);
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
    }

    let mut client = match claim_connection(dbc, stmt, statement_handle, name) {
        Ok(client) => client,
        Err(rc) => return rc,
    };

    flush_pending_unprepare(dbc, stmt, &mut client, name);

    if let Err(e) = dbc.runtime.block_on(client.execute(sql, ())).map(|_| ()) {
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

    // The sp_* procedures still emit the ODBC 2.x column names. msodbcsql
    // rewrites them to the ODBC 3.x names the spec mandates, and callers bind
    // result attributes straight from SQLDescribeCol, so the rename is what
    // makes `row.table_cat` and friends exist.
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        for (index, new_name) in renames {
            if let Some(meta) = stmt_state.column_metadata.get_mut(*index) {
                meta.column_name = (*new_name).to_string();
            }
        }
    }

    rc
}

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
    crate::ffi_entry!("SQLTablesW", unsafe {
        let catalog = opt_arg(catalog_name, name_length_1);
        let schema = opt_arg(schema_name, name_length_2);
        let table = opt_arg(table_name, name_length_3);
        let types = opt_arg(table_type, name_length_4);
        let sql = build_exec(
            &catalog,
            "sp_tables",
            &[
                literal(&table),
                literal(&schema),
                // The qualifier argument is redundant once the proc is
                // three-part qualified, but sp_tables validates it against the
                // current database, so pass NULL.
                "NULL".to_string(),
                table_type_literal(&types),
                "1".to_string(),
            ],
        );
        run_catalog(
            statement_handle,
            "SQLTablesW",
            sql,
            &[(0, "TABLE_CAT"), (1, "TABLE_SCHEM")],
        )
    })
}

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
    crate::ffi_entry!("SQLColumnsW", unsafe {
        let catalog = opt_arg(catalog_name, name_length_1);
        let schema = opt_arg(schema_name, name_length_2);
        let table = opt_arg(table_name, name_length_3);
        let column = opt_arg(column_name, name_length_4);
        let sql = build_exec_named(
            &catalog,
            "sp_columns_100",
            &[
                ("table_name", literal(&table)),
                ("table_owner", literal(&schema)),
                ("table_qualifier", "NULL".to_string()),
                ("column_name", literal(&column)),
                ("ODBCVer", "3".to_string()),
                ("fUsePattern", "1".to_string()),
            ],
        );
        run_catalog(
            statement_handle,
            "SQLColumnsW",
            sql,
            &[
                (0, "TABLE_CAT"),
                (1, "TABLE_SCHEM"),
                (6, "COLUMN_SIZE"),
                (7, "BUFFER_LENGTH"),
                (8, "DECIMAL_DIGITS"),
                (9, "NUM_PREC_RADIX"),
            ],
        )
    })
}

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
    crate::ffi_entry!("SQLPrimaryKeysW", unsafe {
        let catalog = opt_arg(catalog_name, name_length_1);
        let schema = opt_arg(schema_name, name_length_2);
        let table = opt_arg(table_name, name_length_3);
        let sql = build_exec(
            &catalog,
            "sp_pkeys",
            &[literal(&table), literal(&schema), "NULL".to_string()],
        );
        run_catalog(
            statement_handle,
            "SQLPrimaryKeysW",
            sql,
            &[(0, "TABLE_CAT"), (1, "TABLE_SCHEM")],
        )
    })
}

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
    crate::ffi_entry!("SQLForeignKeysW", unsafe {
        let pk_catalog = opt_arg(pk_catalog_name, name_length_1);
        let pk_schema = opt_arg(pk_schema_name, name_length_2);
        let pk_table = opt_arg(pk_table_name, name_length_3);
        let fk_catalog = opt_arg(fk_catalog_name, name_length_4);
        let fk_schema = opt_arg(fk_schema_name, name_length_5);
        let fk_table = opt_arg(fk_table_name, name_length_6);
        // Both sides must live in one database; prefer whichever qualifier the
        // caller supplied.
        let catalog = match (&pk_catalog, &fk_catalog) {
            (Some(c), _) if !c.is_empty() => pk_catalog.clone(),
            (_, Some(c)) if !c.is_empty() => fk_catalog.clone(),
            _ => None,
        };
        let sql = build_exec(
            &catalog,
            "sp_fkeys",
            &[
                literal(&pk_table),
                literal(&pk_schema),
                "NULL".to_string(),
                literal(&fk_table),
                literal(&fk_schema),
                "NULL".to_string(),
            ],
        );
        run_catalog(
            statement_handle,
            "SQLForeignKeysW",
            sql,
            &[
                (0, "PKTABLE_CAT"),
                (1, "PKTABLE_SCHEM"),
                (4, "FKTABLE_CAT"),
                (5, "FKTABLE_SCHEM"),
            ],
        )
    })
}

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
    unique: u16,
    reserved: u16,
) -> SqlReturn {
    crate::ffi_entry!("SQLStatisticsW", unsafe {
        let catalog = opt_arg(catalog_name, name_length_1);
        let schema = opt_arg(schema_name, name_length_2);
        let table = opt_arg(table_name, name_length_3);
        // SQL_INDEX_UNIQUE == 0 selects unique indexes only.
        let is_unique = if unique == 0 { "'Y'" } else { "'N'" };
        // SQL_QUICK == 0 permits cached cardinality; SQL_ENSURE == 1 forces a scan.
        let accuracy = if reserved == 0 { "'Q'" } else { "'E'" };
        let sql = build_exec(
            &catalog,
            "sp_statistics",
            &[
                literal(&table),
                literal(&schema),
                "NULL".to_string(),
                // msodbcsql always passes '%' here: sp_statistics filters
                // `index_name LIKE @index_name`, so NULL returns no index rows.
                "N'%'".to_string(),
                is_unique.to_string(),
                accuracy.to_string(),
            ],
        );
        run_catalog(
            statement_handle,
            "SQLStatisticsW",
            sql,
            &[
                (0, "TABLE_CAT"),
                (1, "TABLE_SCHEM"),
                (7, "ORDINAL_POSITION"),
                (9, "ASC_OR_DESC"),
            ],
        )
    })
}

/// Implements `SQLSpecialColumnsW`.
///
/// # Safety
/// Each name pointer must be null or reference `*_len` readable UTF-16 units.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn sql_special_columns_w(
    statement_handle: SqlHandle,
    identifier_type: u16,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    scope: u16,
    nullable: u16,
) -> SqlReturn {
    crate::ffi_entry!("SQLSpecialColumnsW", unsafe {
        let catalog = opt_arg(catalog_name, name_length_1);
        let schema = opt_arg(schema_name, name_length_2);
        let table = opt_arg(table_name, name_length_3);
        // SQL_BEST_ROWID == 1 maps to 'R', SQL_ROWVER == 2 to 'V'.
        let col_type = if identifier_type == 1 { "'R'" } else { "'V'" };
        // SQL_NULLABLE == 1 permits nullable columns in the result.
        let nullable_arg = if nullable == 1 { "'U'" } else { "'O'" };
        let sql = build_exec(
            &catalog,
            "sp_special_columns_100",
            &[
                literal(&table),
                literal(&schema),
                "NULL".to_string(),
                col_type.to_string(),
                format!("'{}'", if scope == 0 { "C" } else { "T" }),
                nullable_arg.to_string(),
                "3".to_string(),
            ],
        );
        run_catalog(
            statement_handle,
            "SQLSpecialColumnsW",
            sql,
            &[
                (4, "COLUMN_SIZE"),
                (5, "BUFFER_LENGTH"),
                (6, "DECIMAL_DIGITS"),
            ],
        )
    })
}

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
    crate::ffi_entry!("SQLProceduresW", unsafe {
        let catalog = opt_arg(catalog_name, name_length_1);
        let schema = opt_arg(schema_name, name_length_2);
        let proc = opt_arg(proc_name, name_length_3);
        let sql = build_exec(
            &catalog,
            "sp_stored_procedures",
            &[
                literal(&proc),
                literal(&schema),
                "NULL".to_string(),
                "1".to_string(),
            ],
        );
        run_catalog(
            statement_handle,
            "SQLProceduresW",
            sql,
            &[(0, "PROCEDURE_CAT"), (1, "PROCEDURE_SCHEM")],
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_ERROR, SQL_NTS, SQL_NULL_HANDLE};
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn literal_escapes_quotes() {
        assert_eq!(literal(&None), "NULL");
        assert_eq!(literal(&Some("O'Brien".into())), "N'O''Brien'");
    }

    #[test]
    fn build_exec_qualifies_with_catalog() {
        let sql = build_exec(&Some("mydb".into()), "sp_tables", &["NULL".into()]);
        assert!(sql.ends_with("ELSE EXEC [mydb].sys.sp_tables NULL"));
    }

    #[test]
    fn build_exec_without_catalog_uses_current_database() {
        let sql = build_exec(&None, "sp_pkeys", &["N't'".into()]);
        assert_eq!(sql, "EXEC sys.sp_pkeys N't'");
    }

    #[test]
    fn build_exec_escapes_bracket_in_catalog() {
        let sql = build_exec(&Some("we]ird".into()), "sp_tables", &[]);
        assert!(sql.contains("EXEC [we]]ird].sys.sp_tables"));
    }

    #[test]
    fn build_exec_named_renders_named_arguments() {
        let sql = build_exec_named(
            &None,
            "sp_columns_100",
            &[("table_name", "N't'".into()), ("ODBCVer", "3".into())],
        );
        assert_eq!(
            sql,
            "EXEC sys.sp_columns_100 @table_name = N't', @ODBCVer = 3"
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
                SQL_NTS,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn table_type_quotes_each_element_individually() {
        assert_eq!(
            table_type_literal(&Some("TABLE,VIEW".to_string())),
            "N'''TABLE'',''VIEW'''"
        );
    }

    #[test]
    fn table_type_passes_wildcard_through() {
        assert_eq!(table_type_literal(&Some("%".to_string())), "N'%'");
    }

    #[test]
    fn table_type_treats_blank_as_null() {
        assert_eq!(table_type_literal(&Some("   ".to_string())), "NULL");
        assert_eq!(table_type_literal(&None), "NULL");
    }

    #[test]
    fn build_exec_without_catalog_runs_locally() {
        let sql = build_exec(&None, "sp_tables", &["NULL".to_string()]);
        assert_eq!(sql, "EXEC sys.sp_tables NULL");
    }

    #[test]
    fn build_exec_with_catalog_guards_on_db_id() {
        let sql = build_exec(
            &Some("other".to_string()),
            "sp_tables",
            &["@table_name = N'%'".to_string(), "NULL".to_string()],
        );
        assert!(sql.starts_with("IF DB_ID(N'other') IS NULL EXEC sys.sp_tables "));
        assert!(sql.contains("ELSE EXEC [other].sys.sp_tables @table_name = N'%', NULL"));
        // The fallback keeps the named-argument prefix so the call still binds.
        assert!(sql.contains("@table_name =N'\u{1}no such object\u{1}'"));
    }

    #[test]
    fn build_exec_escapes_bracketed_catalog_name() {
        let sql = build_exec(&Some("we]ird".to_string()), "sp_tables", &[]);
        assert!(sql.contains("[we]]ird].sys.sp_tables"));
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
    fn catalog_execution_resets_stale_statement_state_before_claiming_connection() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_EXEC_CONTEXT);
            state.row_count = 7;
            state.pending_row_counts.push_back(3);
        }

        let ret = unsafe {
            run_catalog(
                h.stmt,
                "SQLTablesW",
                "EXEC sys.sp_tables NULL".to_string(),
                &[],
            )
        };
        assert_eq!(ret, SQL_ERROR);

        let state = stmt.inner.lock().unwrap();
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
        assert!(!state.has_state(STMT_STATE_EXEC_CONTEXT));
        assert_eq!(state.row_count, -1);
        assert!(state.pending_row_counts.is_empty());
    }
}
