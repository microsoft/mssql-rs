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
//! - Distributed/linked-server catalog queries (the `sp_tables_ex`/
//!   `sp_columns_ex_100`/... procs, and the `sp_cursoropen`-wrapped dispatch
//!   `DoDD()` uses for them). No ODBC catalog function exposes a dedicated
//!   `Server` *parameter*, but msodbcsql overloads `CatalogName` to carry one:
//!   `ValidateTableQualifier` (`sqlcdd.cpp` lines 264-278) splits the
//!   argument on its first `.` into server/database whenever
//!   `SQL_ATTR_METADATA_ID == SQL_FALSE` (always true here — see below), and
//!   that split is what selects the `_ex` procs for five of the seven
//!   functions (`SQLTables`, `SQLColumns`, `SQLStatistics`, `SQLPrimaryKeys`,
//!   `SQLForeignKeys`; `sqlcdd.cpp` line 1512). Not implementing `_ex`
//!   dispatch is a reasonable scoping decision — mssql-python (this crate's
//!   motivating consumer) never triggers this path — but the rationale is
//!   that overloaded parsing, not the argument's absence.
//!
//!   The consequence: `SQLTables(CatalogName = "MYSRV.MyDb")` here treats the
//!   entire string as one literal (three-part-bracketed) database name,
//!   fails to resolve it, and returns an empty result set via the
//!   nonexistent-catalog retry below — the same outcome as any other
//!   nonexistent catalog, with no diagnostic calling out the unsupported
//!   server-qualified form specifically. This is a real, known gap (a
//!   dedicated diagnostic would be more debuggable), traded off here against
//!   a genuine improvement in the other direction: a database *legitimately*
//!   named `My.Db` (SQL Server permits dots in quoted identifiers) resolves
//!   correctly through this implementation and is mis-parsed as a
//!   server-qualified name by msodbcsql.
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
    SQL_ERROR, SQL_INVALID_HANDLE, SQL_NTS, SqlHandle, SqlReturn, SqlSmallInt, SqlUSmallInt,
    SqlWChar,
};
use super::sqlstate::{ERR_INVALID_CURSOR_STATE, ERR_INVALID_STRING_OR_BUFFER_LENGTH, post_diag};
use super::txn::begin_transaction_if_manual;
use super::util::{COLMETA_NULLABLE_FLAG, read_utf16};
use crate::error::free_errors;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, STMT_STATE_PREPARED,
};
use crate::handles::{HandleType, OdbcVersion, StmtHandle, handle_from_raw};

/// Maximum SQL Server identifier length (`SYSNAMELEN` in msodbcsql
/// `sqlcdd.cpp`). Declared length for every catalog/schema/table/column
/// RPC parameter.
const SYSNAME_LEN: u16 = 128;

/// Declared length for `SQLProcedures`' procedure-name argument specifically:
/// msodbcsql validates it against `MAXPROCNAMESPHINX` (`sqlsrv.h`), not the
/// bare `SYSNAMELEN` other identifiers use — `MAX_PROCNAME + 6`, the extra 6
/// characters covering a numbered-procedure `;nnnnn` group suffix (e.g.
/// `myproc;1`). Using [`SYSNAME_LEN`] here would reject a valid, longer
/// numbered-procedure name msodbcsql accepts.
const PROC_NAME_LEN: u16 = SYSNAME_LEN + 6;

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
        // The DM validates this before calling the driver (see SQLTables/
        // SQLColumns/... spec); matches the identical guard in prepare.rs.
        debug_assert!(
            len == SQL_NTS || len >= 0,
            "catalog function: invalid argument length ({len}) — DM should have rejected this"
        );
        Some(unsafe { read_utf16(ptr, len) })
    }
}

/// Builds an `NVARCHAR(len)` positional RPC parameter; `None` encodes SQL
/// NULL.
fn nvarchar(value: Option<&str>, len: u16) -> RpcParameter {
    let sql_value = value.map(|v| SqlString::from_utf8_string(v.to_string()));
    RpcParameter::new(None, StatusFlags::NONE, SqlType::NVarchar(sql_value, len))
}

/// Validates that a catalog-function string argument's character count fits
/// within the length declared for its RPC parameter (`max_len` — [`SYSNAME_LEN`]
/// for ordinary identifier arguments, [`TABLE_TYPE_LEN`] for `SQLTables`'
/// `TableType`), posting the ODBC-mandated `HY090` diagnostic and returning
/// `Err(SQL_ERROR)` for the first violation. Mirrors msodbcsql's upfront
/// `ValidateArgument`/`ValidateTableQualifier` (`sqlcdd.cpp` lines 296-310,
/// 425-440), which rejects an oversized argument before attempting any
/// network call, rather than letting it surface as an internal RPC-parameter-
/// serialization failure (`HY000`, not the state the spec mandates for this
/// condition).
///
/// `is_pattern` must be `true` for every identifier-style argument (catalog,
/// schema, table, column, procedure name) and `false` only for `SQLTables`'
/// `TableType` (a literal comma-separated value list, not a search pattern —
/// already transformed by [`table_type_value`] before this runs). When
/// `true`, the length is measured *after* stripping ODBC search-pattern
/// escapes (see [`unescape_search_pattern`]), matching msodbcsql exactly: a
/// pattern like `my\_identifier\_that\_is\_130\_chars` can be under the true
/// 128-character limit once its three escape backslashes are removed, and
/// rejecting it on the raw (escaped) length would reject input msodbcsql
/// accepts.
fn check_arg_length(
    stmt: &StmtHandle,
    value: &Arg,
    max_len: u16,
    is_pattern: bool,
) -> Result<(), SqlReturn> {
    let within_limit = value.as_deref().is_none_or(|v| {
        let effective_len = if is_pattern {
            unescape_search_pattern(v).chars().count()
        } else {
            v.chars().count()
        };
        effective_len <= max_len as usize
    });
    if within_limit {
        return Ok(());
    }
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("catalog function: stmt mutex poisoned validating argument length");
        return Err(SQL_ERROR);
    };
    post_diag(&mut stmt_state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
    Err(SQL_ERROR)
}

/// Runs [`check_arg_length`] over every `(argument, max_len, is_pattern)`
/// triple, short-circuiting on the first violation.
fn check_arg_lengths(stmt: &StmtHandle, args: &[(&Arg, u16, bool)]) -> Result<(), SqlReturn> {
    for (value, max_len, is_pattern) in args {
        check_arg_length(stmt, value, *max_len, *is_pattern)?;
    }
    Ok(())
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
        Some(db) => format!(
            "[{}].sys.{proc}",
            unescape_search_pattern(db).replace(']', "]]")
        ),
        None => format!("[sys].{proc}"),
    }
}

/// Strips the ODBC search-pattern escape character (`\`) when it precedes
/// `\`, `_`, or `%`, matching msodbcsql's `ValidateSearchPattern`
/// (`sqlcdd.cpp` line 1308), which performs this same unescape before
/// bracket-quoting a catalog value to build the three-part qualified
/// procedure name above. Without it, a catalog argument containing an
/// escaped wildcard character — e.g. `my\_db`, meaning the literal
/// identifier `my_db` under the ODBC search-pattern escape convention —
/// fails to resolve as a database and falls into the nonexistent-catalog
/// retry instead of finding it.
fn unescape_search_pattern(pattern: &str) -> String {
    let mut result = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('\\') | Some('_') | Some('%')) {
            result.push(chars.next().expect("peeked Some above"));
        } else {
            result.push(c);
        }
    }
    result
}

/// Renders the `SQLTables` `TableType` argument the way `sp_tables` expects:
/// a comma-separated list with each element individually single-quoted, so
/// `TABLE,VIEW` becomes `'TABLE','VIEW'` (msodbcsql `ValidateTableType`,
/// `sqlcdd.cpp` lines 448-552). `None`, blank, or a bare `%` pass through
/// unchanged so the proc's own wildcard/NULL handling applies; unlike the
/// dynamic-SQL approach this replaces, no T-SQL string-literal escaping is
/// needed here — this is RPC parameter *data*, not SQL text.
///
/// Trims whitespace around each element (`"TABLE, VIEW"` -> `'TABLE','VIEW'`),
/// which is more lenient than msodbcsql's non-trimming `ValidateTableType` —
/// a deliberate, low-risk divergence, not an oversight.
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

/// Whether `arg` is SQL NULL or a zero-length string once trimmed of nothing —
/// i.e. "no value supplied", collapsing the `None` (null pointer) vs.
/// `Some(String::new())` (valid zero-length string) distinction that matters
/// elsewhere in this module. Used only for the catalog-enumeration-sentinel
/// check below, where msodbcsql's own `rgcchArg[...] != 0` test makes the same
/// simplification.
fn is_blank(arg: &Arg) -> bool {
    arg.as_deref().is_none_or(str::is_empty)
}

/// Whether `catalog` is the ODBC-defined bare-`%` catalog-enumeration
/// sentinel: exactly `"%"`, with the function's name and owner arguments both
/// blank. `SQLTables(CatalogName="%", SchemaName="", TableName="", ...)` is
/// the ODBC-mandated idiom for "return the list of catalogs" — every ODBC
/// client tool relies on it. msodbcsql excludes this exact combination from
/// three-part qualification *universally*, not only for `SQLTables`
/// (`sqlcdd.cpp` `DoDD()`, lines 1460-1463: the qualifier-parsing block is
/// skipped whenever `rgcchArg[QUAL_IND] == 1 && *rglpchArg[QUAL_IND] == '%' &&
/// rgcchArg[NAME_IND] == 0 && rgcchArg[OWNER_IND] == 0`, regardless of
/// `bAPI`), so a caller passing this combination to any of the seven
/// functions gets the unqualified dispatch this returns `true` for. The
/// stored procedure still receives the literal `"%"` value (see the
/// `table_qualifier`/`proc_qualifier` RPC parameter in each function below) —
/// only the three-part *qualification* is skipped, matching msodbcsql exactly.
///
/// Deliberately narrower than msodbcsql's full behavior: a wildcard catalog
/// combined with a non-blank name/owner switches `SQLTables` to a dedicated
/// `sp_tableswc` proc for pattern-based catalog enumeration (`sqlcdd.cpp`
/// lines 1468-1486); that path has no ODBC-mandated caller (mssql-python never
/// exercises it) and is not implemented here — such a combination is treated
/// as a literal (and typically nonexistent) catalog name, same as any other
/// catalog value containing `%`.
///
/// Only `sp_tables` recognizes the literal `%` qualifier itself (its
/// "Special feature #1" branch, gated on blank name/owner exactly like this
/// check); the other six procedures `raiserror` 15250 on it. Skipping
/// three-part qualification here is what lets `sp_tables` reach that branch,
/// but for the other six it merely avoids a spurious `[%].sys.proc`
/// resolution failure — they still depend on [`run_catalog`]'s
/// nonexistent-catalog retry (gated on whether the caller supplied a
/// catalog, not on this function's output) to turn the 15250 into the empty
/// result set the sentinel is supposed to produce.
fn is_catalog_enumeration_sentinel(catalog: &Arg, name: &Arg, owner: &Arg) -> bool {
    catalog.as_deref() == Some("%") && is_blank(name) && is_blank(owner)
}

/// The catalog to use for three-part qualification — `catalog` itself unless
/// it is the enumeration sentinel above, in which case qualification is
/// skipped (`None`) while the RPC parameter still carries the real value.
/// Deliberately *not* used to gate [`run_catalog`]'s nonexistent-catalog
/// retry: that gate must reflect whether the caller supplied a catalog at
/// all, not whether qualification happened to be skipped for the sentinel.
fn qualification_catalog(catalog: &Arg, name: &Arg, owner: &Arg) -> Arg {
    if is_catalog_enumeration_sentinel(catalog, name, owner) {
        None
    } else {
        catalog.clone()
    }
}

/// The value to send as the `table_qualifier`/`proc_qualifier` RPC parameter.
/// SQL Server rejects a non-NULL qualifier that doesn't match the *current*
/// database with error 15250 ("The database name component of the object
/// qualifier must be the name of the current database"); NULL bypasses that
/// check, and so does the exact literal `%` for `sp_tables` specifically
/// (see [`is_catalog_enumeration_sentinel`]) since it special-cases that
/// value before ever comparing it to the current database name. The other
/// six procedures have no such exemption and rely entirely on
/// `run_catalog`'s retry when sent a literal `%`. That retry always
/// dispatches unqualified against the current database, so `unmatchable`
/// (`true` only on that retry) must null out any real catalog value here —
/// otherwise a nonexistent-catalog call would itself fail with 15250 instead
/// of the empty result set the retry exists to produce.
///
/// Unescapes ODBC search-pattern escapes (see [`unescape_search_pattern`])
/// exactly as [`qualified_proc_name`] does for the three-part name: the two
/// must agree on what the catalog value *is*, since the RPC dispatches into
/// the database `qualified_proc_name` names but the server compares that
/// context against the literal bytes sent here. For `catalog = "my\_db"`,
/// sending the still-escaped `my\_db` here while `qualified_proc_name` builds
/// `[my_db].sys.proc` makes the two disagree, and the qualifier's raw
/// backslash makes it a non-NULL value that doesn't match the current
/// database (`my_db`) — triggering 15250 and the nonexistent-catalog retry
/// for a database that actually exists.
fn qualifier_value(catalog: &Arg, unmatchable: bool) -> Option<String> {
    if unmatchable {
        None
    } else {
        catalog.as_deref().map(unescape_search_pattern)
    }
}

/// Whether the application declared ODBC 2.x (`SQLSetEnvAttr(SQL_ATTR_ODBC_VERSION,
/// SQL_OV_ODBC2)`), read from the parent ENV. Selects `@ODBCVer` / `@fUsePattern`
/// inclusion exactly as `get_type_info::sql_get_type_info_w_safe` does for
/// `SQLGetTypeInfo`. Returns `Err(SQL_ERROR)` on a poisoned ENV mutex instead
/// of guessing a version, matching `get_type_info.rs`'s identical check.
fn is_2x_app(stmt: &StmtHandle) -> Result<bool, SqlReturn> {
    let env = stmt.parent_dbc().parent_env();
    let Ok(env_state) = env.inner.lock() else {
        error!("catalog function: env mutex poisoned reading ODBC version");
        return Err(SQL_ERROR);
    };
    Ok(env_state.odbc_version == OdbcVersion::Odbc2)
}

/// Runs a catalog-function stored procedure and leaves its result set open
/// for `SQLFetch`, then applies the ODBC 3.x column renames and NOT NULL
/// flags msodbcsql applies via `SetColNames`/`ClearNullable` (`DoDD()`,
/// `sqlcdd.cpp` lines 1910-1913).
///
/// `catalog` scopes the call to a specific database via a three-part
/// qualified procedure name (see [`qualified_proc_name`]) — the
/// *qualification* catalog from [`qualification_catalog`], which is `None`
/// for the bare-`%` enumeration sentinel even though a catalog value was
/// supplied.
///
/// `retry_on_error` gates the nonexistent-catalog retry below and must
/// reflect whether the *caller* supplied a catalog at all, before the
/// sentinel transform — msodbcsql's own gate (`DoDD()` line ~1880) is
/// `!(wNBits & QUAL_BIT)`, set only when the argument was a null pointer, not
/// derived from whether three-part qualification happened. Only `sp_tables`
/// recognizes the bare-`%` sentinel internally (its "Special feature #1"
/// branch); the other six procedures `raiserror` 15250 on a literal `%`
/// qualifier that doesn't match the current database, so gating the retry on
/// the *qualification* catalog (which the sentinel deliberately nulls) would
/// skip the very retry that turns that 15250 into the empty result set the
/// sentinel is supposed to produce.
///
/// If a catalog was supplied and that qualified call fails for any reason,
/// msodbcsql (`DoDD()`, lines 1883-1895) recovers by re-running the same
/// procedure unqualified (i.e. against the *current* database) with its
/// primary name filter forced to [`UNMATCHABLE_NAME`], producing a
/// correctly-shaped empty result set instead of surfacing the object-
/// resolution error. `build_params(true)` must build that "unmatchable"
/// parameter set. Only a server-reported SQL error triggers the retry — a
/// transport-level failure (connection drop, timeout) is propagated
/// immediately, since retrying on a dead connection cannot succeed. Info
/// messages captured during the abandoned first call are discarded before
/// the retry runs, matching msodbcsql's `FreeErrors(lpstmt)` (`sqlcdd.cpp`
/// line ~1884), so a successful empty retry can't surface stale messages
/// from a call the application never saw succeed or fail.
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
    retry_on_error: bool,
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

    let (positional, named) = build_params(false);
    let mut exec_result = dbc.runtime.block_on(client.execute_stored_procedure(
        qualified_proc_name(catalog, proc),
        Some(positional),
        named,
        (),
    ));

    if retry_on_error && matches!(exec_result, Err(TdsError::SqlServerError { .. })) {
        debug!(%proc, "{name}: qualified catalog call failed, retrying unqualified");
        let _ = client.take_info_messages();
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
    if let Err(rc) = apply_catalog_metadata(stmt, not_null_cols, renames) {
        return rc;
    }
    rc
}

/// Applies the post-execution column-metadata fixups every catalog function
/// needs: renaming the ODBC 2.x names the system procedures emit
/// (`TABLE_QUALIFIER`, `TABLE_OWNER`, ...) to their ODBC 3.x equivalents, and
/// clearing the nullable flag on the columns the ODBC specification guarantees
/// are NOT NULL — mirrors `SetColNames`/`ClearNullable` (`sqlcdd.cpp` lines
/// 1910-1913, 2412-2472).
///
/// Returns `Err(SQL_ERROR)` on a poisoned stmt mutex rather than silently
/// skipping the fixup: msodbcsql's `SetColNames` uses `SETRC_SERR_GOTO`
/// (`sqlcdd.cpp` line 2459) and aborts the call on failure, and this repo's
/// own convention (every other poisoned-mutex site in this file) is to fail
/// loudly rather than report `SQL_SUCCESS` with metadata the caller can't
/// trust. Reporting success here would leave the raw stored-procedure column
/// names (`TABLE_QUALIFIER`, `TABLE_OWNER`, `PRECISION`, ...) and
/// still-`SQL_NULLABLE` flags in place — a conforming application binding by
/// ODBC 3.x column name would simply fail to find its columns, silently.
fn apply_catalog_metadata(
    stmt: &StmtHandle,
    not_null_cols: &[usize],
    renames: &[(usize, &str)],
) -> Result<(), SqlReturn> {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("catalog function: stmt mutex poisoned applying column metadata");
        return Err(SQL_ERROR);
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
    Ok(())
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
    if let Err(rc) = check_arg_lengths(
        stmt,
        &[
            (&catalog, SYSNAME_LEN, true),
            (&schema, SYSNAME_LEN, true),
            (&table, SYSNAME_LEN, true),
            (&table_type, TABLE_TYPE_LEN, false),
        ],
    ) {
        return rc;
    }
    let qualify_catalog = qualification_catalog(&catalog, &table, &schema);
    let retry_on_error = !is_blank(&catalog);
    let build_params = |unmatchable: bool| {
        let table_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            // The real catalog value (or NULL on the unqualified retry — see
            // `qualifier_value`), not just a qualification aid: msodbcsql
            // sends it too (`sqlcdd.cpp` lines 1745-1757), and `sp_tables`
            // needs the literal `%` to recognize the catalog-enumeration
            // idiom (see `is_catalog_enumeration_sentinel`).
            nvarchar(
                qualifier_value(&catalog, unmatchable).as_deref(),
                SYSNAME_LEN,
            ),
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
        &qualify_catalog,
        retry_on_error,
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
    if let Err(rc) = check_arg_lengths(
        stmt,
        &[
            (&catalog, SYSNAME_LEN, true),
            (&schema, SYSNAME_LEN, true),
            (&table, SYSNAME_LEN, true),
            (&column, SYSNAME_LEN, true),
        ],
    ) {
        return rc;
    }
    let is_2x = match is_2x_app(stmt) {
        Ok(is_2x) => is_2x,
        Err(rc) => return rc,
    };
    let qualify_catalog = qualification_catalog(&catalog, &table, &schema);
    let retry_on_error = !is_blank(&catalog);
    let build_params = |unmatchable: bool| {
        let table_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(
                qualifier_value(&catalog, unmatchable).as_deref(),
                SYSNAME_LEN,
            ),
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
        &qualify_catalog,
        retry_on_error,
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
    if let Err(rc) = check_arg_lengths(
        stmt,
        &[
            (&catalog, SYSNAME_LEN, true),
            (&schema, SYSNAME_LEN, true),
            (&table, SYSNAME_LEN, true),
        ],
    ) {
        return rc;
    }
    let qualify_catalog = qualification_catalog(&catalog, &table, &schema);
    let retry_on_error = !is_blank(&catalog);
    let build_params = |unmatchable: bool| {
        let table_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(
                qualifier_value(&catalog, unmatchable).as_deref(),
                SYSNAME_LEN,
            ),
        ];
        (positional, None)
    };

    run_catalog(
        statement_handle,
        "SQLPrimaryKeysW",
        stmt,
        "sp_pkeys",
        &qualify_catalog,
        retry_on_error,
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
    if let Err(rc) = check_arg_lengths(
        stmt,
        &[
            (&pk_catalog, SYSNAME_LEN, true),
            (&pk_schema, SYSNAME_LEN, true),
            (&pk_table, SYSNAME_LEN, true),
            (&fk_catalog, SYSNAME_LEN, true),
            (&fk_schema, SYSNAME_LEN, true),
            (&fk_table, SYSNAME_LEN, true),
        ],
    ) {
        return rc;
    }
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
    let qualify_catalog = qualification_catalog(&catalog, &pk_table, &pk_schema);
    let retry_on_error = pk_has || fk_has;

    let build_params = |unmatchable: bool| {
        let pk_table_name = if unmatchable || catalogs_conflict {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            pk_table.clone()
        };
        let positional = vec![
            nvarchar(pk_table_name.as_deref(), SYSNAME_LEN),
            nvarchar(pk_schema.as_deref(), SYSNAME_LEN),
            // `catalog` (the already-resolved value — see above), not the raw
            // `pk_catalog`: when only `fk_catalog` was supplied, `catalog` is
            // `fk_catalog` and the three-part name qualifies into it, but
            // `pk_catalog` is still blank. Sending the raw blank `pk_catalog`
            // here (msodbcsql normalizes this the same way, `sqlcdd.cpp`
            // lines 984-990) would send a zero-length, non-NULL qualifier
            // while the RPC dispatches into `fk_catalog`'s database — a
            // mismatch SQL Server can reject with 15250. Sending the
            // resolved `catalog` value here instead makes the qualifier and
            // the three-part name provably consistent.
            nvarchar(
                qualifier_value(&catalog, unmatchable).as_deref(),
                SYSNAME_LEN,
            ),
            nvarchar(fk_table.as_deref(), SYSNAME_LEN),
            nvarchar(fk_schema.as_deref(), SYSNAME_LEN),
            // `fk_catalog` is also nulled when the two sides conflict: the
            // call qualifies into `pk_catalog`'s context (see `catalog`
            // above), and a `fk_catalog` that disagrees with the *current*
            // database would itself raise SQL Server error 15250 rather than
            // the empty result `catalogs_conflict` (via the unmatchable PK
            // table name) already forces. Sent as the real value (not
            // unconditionally NULL, unlike msodbcsql's
            // `SetNullArgument(&dd_arg, XARG3_IND)`, `sqlcdd.cpp` line 1021)
            // because the three-part name already makes the target database
            // current, so a matching `fk_catalog` cannot trigger 15250 here.
            nvarchar(
                qualifier_value(&fk_catalog, unmatchable || catalogs_conflict).as_deref(),
                SYSNAME_LEN,
            ),
        ];
        (positional, None)
    };

    run_catalog(
        statement_handle,
        "SQLForeignKeysW",
        stmt,
        "sp_fkeys",
        &qualify_catalog,
        retry_on_error,
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
    if let Err(rc) = check_arg_lengths(
        stmt,
        &[
            (&catalog, SYSNAME_LEN, true),
            (&schema, SYSNAME_LEN, true),
            (&table, SYSNAME_LEN, true),
        ],
    ) {
        return rc;
    }
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

    let qualify_catalog = qualification_catalog(&catalog, &table, &schema);
    let retry_on_error = !is_blank(&catalog);
    let build_params = |unmatchable: bool| {
        let table_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(
                qualifier_value(&catalog, unmatchable).as_deref(),
                SYSNAME_LEN,
            ),
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
        &qualify_catalog,
        retry_on_error,
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
    identifier_type: SqlUSmallInt,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    scope: SqlUSmallInt,
    nullable: SqlUSmallInt,
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
    identifier_type: SqlUSmallInt,
    catalog_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    schema_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    table_name: *const SqlWChar,
    name_length_3: SqlSmallInt,
    scope: SqlUSmallInt,
    nullable: SqlUSmallInt,
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
    identifier_type: SqlUSmallInt,
    catalog: Arg,
    schema: Arg,
    table: Arg,
    scope: SqlUSmallInt,
    nullable: SqlUSmallInt,
) -> SqlReturn {
    use super::odbc_types::{
        SQL_BEST_ROWID, SQL_NO_NULLS, SQL_SCOPE_CURROW, SQL_SCOPE_TRANSACTION, SQL_TXN_SERIALIZABLE,
    };

    if let Err(rc) = check_arg_lengths(
        stmt,
        &[
            (&catalog, SYSNAME_LEN, true),
            (&schema, SYSNAME_LEN, true),
            (&table, SYSNAME_LEN, true),
        ],
    ) {
        return rc;
    }

    let col_type = if identifier_type == SQL_BEST_ROWID {
        "R"
    } else {
        "V"
    };
    let scope_char = if scope == SQL_SCOPE_CURROW { "C" } else { "T" };
    // `SQL_NO_NULLS` is typed `SqlSmallInt` (`i16`) for its other use as
    // `SQLDescribeCol`'s signed `Nullable` output; `SQLSpecialColumns`'
    // `fNullable` is `SQLUSMALLINT` per the ODBC header, hence the cast.
    let nullable_char = if nullable == SQL_NO_NULLS as SqlUSmallInt {
        "O"
    } else {
        "U"
    };

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

    let is_2x = match is_2x_app(stmt) {
        Ok(is_2x) => is_2x,
        Err(rc) => return rc,
    };
    let qualify_catalog = qualification_catalog(&catalog, &table, &schema);
    let retry_on_error = !is_blank(&catalog);
    let build_params = move |unmatchable: bool| {
        let table_name = if unmatchable || force_empty {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            table.clone()
        };
        let positional = vec![
            nvarchar(table_name.as_deref(), SYSNAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(
                qualifier_value(&catalog, unmatchable).as_deref(),
                SYSNAME_LEN,
            ),
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
        &qualify_catalog,
        retry_on_error,
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
    if let Err(rc) = check_arg_lengths(
        stmt,
        &[
            (&catalog, SYSNAME_LEN, true),
            (&schema, SYSNAME_LEN, true),
            (&proc, PROC_NAME_LEN, true),
        ],
    ) {
        return rc;
    }
    let qualify_catalog = qualification_catalog(&catalog, &proc, &schema);
    let retry_on_error = !is_blank(&catalog);
    let build_params = |unmatchable: bool| {
        let proc_name = if unmatchable {
            Some(UNMATCHABLE_NAME.to_string())
        } else {
            proc.clone()
        };
        let positional = vec![
            nvarchar(proc_name.as_deref(), PROC_NAME_LEN),
            nvarchar(schema.as_deref(), SYSNAME_LEN),
            nvarchar(
                qualifier_value(&catalog, unmatchable).as_deref(),
                SYSNAME_LEN,
            ),
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
        &qualify_catalog,
        retry_on_error,
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
    fn is_blank_treats_none_and_empty_string_alike() {
        assert!(is_blank(&None));
        assert!(is_blank(&Some(String::new())));
        assert!(!is_blank(&Some("x".to_string())));
    }

    #[test]
    fn apply_catalog_metadata_poisoned_mutex_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        // Poison the stmt mutex by panicking while it is held.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = stmt.inner.lock().unwrap();
            panic!("poison the stmt lock");
        }));

        // A poisoned mutex must fail loudly, not silently report success with
        // stale column metadata (raw stored-procedure names, still-nullable
        // flags) a conforming ODBC 3.x application would fail to bind by name.
        assert_eq!(apply_catalog_metadata(stmt, &[], &[]), Err(SQL_ERROR));
    }

    #[test]
    fn check_arg_length_measures_pattern_args_after_unescaping() {
        // 129 raw characters (127 'a's plus one escaped underscore `\_`) but
        // exactly 128 once the escape backslash is stripped — must be
        // accepted as a pattern argument, matching msodbcsql's
        // ValidateArgument/ValidateTableQualifier (only measuring the
        // unescaped length once the raw length exceeds the limit;
        // `check_arg_length`'s simpler "always unescape, then measure" is
        // behaviorally equivalent since unescaping never lengthens a
        // string).
        let raw = format!("{}{}", "a".repeat(127), r"\_");
        assert_eq!(raw.chars().count(), 128 + 1);
        assert_eq!(unescape_search_pattern(&raw).chars().count(), 128);

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let arg = Some(raw);
        assert_eq!(check_arg_length(stmt, &arg, SYSNAME_LEN, true), Ok(()));
        // The same raw value measured WITHOUT unescaping (as a non-pattern
        // argument, e.g. `TableType`) is correctly rejected: at 129 raw
        // characters it exceeds the 128 limit and there is no escape
        // convention to strip.
        assert!(check_arg_length(stmt, &arg, SYSNAME_LEN, false).is_err());
    }

    #[test]
    fn check_arg_length_rejects_oversized_pattern_arg_even_after_unescaping() {
        // 129 characters even after stripping the one escape sequence —
        // still over the 128 limit, so this must still be rejected.
        let raw = format!("{}{}", "a".repeat(128), r"\_");
        assert_eq!(unescape_search_pattern(&raw).chars().count(), 129);

        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let arg = Some(raw);
        assert!(check_arg_length(stmt, &arg, SYSNAME_LEN, true).is_err());
    }

    #[test]
    fn catalog_enumeration_sentinel_requires_bare_percent_and_blank_name_owner() {
        let percent = Some("%".to_string());
        // The exact ODBC idiom: catalog="%", name and owner both blank.
        assert!(is_catalog_enumeration_sentinel(&percent, &None, &None));
        assert!(is_catalog_enumeration_sentinel(
            &percent,
            &Some(String::new()),
            &Some(String::new())
        ));
        // A non-blank name or owner takes it out of the enumeration idiom.
        assert!(!is_catalog_enumeration_sentinel(
            &percent,
            &Some("t".to_string()),
            &None
        ));
        assert!(!is_catalog_enumeration_sentinel(
            &percent,
            &None,
            &Some("dbo".to_string())
        ));
        // A catalog that merely contains '%' (not exactly "%") is a literal
        // (typically nonexistent) name, not the sentinel.
        assert!(!is_catalog_enumeration_sentinel(
            &Some("My%".to_string()),
            &None,
            &None
        ));
        // No catalog at all is not the sentinel either.
        assert!(!is_catalog_enumeration_sentinel(&None, &None, &None));
    }

    #[test]
    fn qualification_catalog_skips_qualifying_the_enumeration_sentinel() {
        let percent = Some("%".to_string());
        // The sentinel must not be used to build a three-part name.
        assert_eq!(qualification_catalog(&percent, &None, &None), None);
        // A real catalog name still qualifies normally.
        let real = Some("MyDb".to_string());
        assert_eq!(
            qualification_catalog(&real, &None, &None),
            Some("MyDb".to_string())
        );
        // The sentinel combined with a non-blank name is a literal catalog,
        // not the enumeration idiom, so it still qualifies (and will fail
        // to resolve, triggering run_catalog's nonexistent-catalog retry).
        assert_eq!(
            qualification_catalog(&percent, &Some("t".to_string()), &None),
            Some("%".to_string())
        );
    }

    #[test]
    fn enumeration_sentinel_still_gates_retry_despite_null_qualification_catalog() {
        // The bug this guards against: gating run_catalog's nonexistent-
        // catalog retry on the *qualification* catalog (which is `None` for
        // the bare-`%` enumeration sentinel) would skip the retry for the
        // six catalog procedures that don't special-case a literal `%`
        // internally (only `sp_tables` does) — their SQL Server error 15250
        // would surface as `SQL_ERROR` instead of the empty result set the
        // retry produces. Each `sql_*_w_safe` function instead derives its
        // `retry_on_error` argument from `!is_blank(&catalog)` — the raw,
        // pre-qualification-transform argument — which must stay `true` for
        // the sentinel even though `qualification_catalog` nulls it out.
        let percent = Some("%".to_string());
        assert_eq!(qualification_catalog(&percent, &None, &None), None);
        assert!(!is_blank(&percent));
        // No catalog at all: qualification is trivially None, and so is the
        // retry gate — there is nothing to retry unqualified against.
        assert!(is_blank(&None));
    }

    #[test]
    fn qualifier_value_is_real_catalog_on_first_attempt() {
        let real = Some("MyDb".to_string());
        assert_eq!(qualifier_value(&real, false), Some("MyDb".to_string()));
        assert_eq!(qualifier_value(&None, false), None);
        // The literal enumeration sentinel is echoed too — it is what tells
        // the stored procedure to enumerate catalogs.
        let percent = Some("%".to_string());
        assert_eq!(qualifier_value(&percent, false), Some("%".to_string()));
    }

    #[test]
    fn qualifier_value_is_null_on_the_unqualified_retry() {
        // `run_catalog`'s retry always dispatches unqualified against the
        // current database: echoing a real (non-matching) catalog value here
        // would itself raise SQL Server error 15250, defeating the retry.
        let real = Some("MyDb".to_string());
        assert_eq!(qualifier_value(&real, true), None);
        assert_eq!(qualifier_value(&None, true), None);
    }

    #[test]
    fn qualifier_value_unescapes_search_pattern_same_as_qualified_proc_name() {
        // The RPC qualifier parameter and the three-part proc name must
        // agree on what the catalog value *is* — sending the still-escaped
        // qualifier while the proc name resolves into the unescaped database
        // would make SQL Server see them as different databases and raise
        // 15250, silently emptying results for a database that exists.
        let escaped = Some(r"my\_db".to_string());
        assert_eq!(qualifier_value(&escaped, false), Some("my_db".to_string()));
        assert_eq!(
            qualified_proc_name(&escaped, "sp_tables"),
            "[my_db].sys.sp_tables"
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
    fn qualified_proc_name_unescapes_search_pattern_before_bracketing() {
        // `my\_db` under the ODBC search-pattern escape convention means the
        // literal identifier `my_db`; msodbcsql's `ValidateSearchPattern`
        // strips the backslash before building the bracketed qualifier
        // (`sqlcdd.cpp` line 1308), and this must match or the database
        // fails to resolve.
        assert_eq!(
            qualified_proc_name(&Some(r"my\_db".to_string()), "sp_tables"),
            "[my_db].sys.sp_tables"
        );
        assert_eq!(
            qualified_proc_name(&Some(r"my\%db".to_string()), "sp_tables"),
            "[my%db].sys.sp_tables"
        );
        assert_eq!(
            qualified_proc_name(&Some(r"my\\db".to_string()), "sp_tables"),
            r"[my\db].sys.sp_tables"
        );
        // A backslash not followed by an escapable character is literal.
        assert_eq!(
            qualified_proc_name(&Some(r"my\db".to_string()), "sp_tables"),
            r"[my\db].sys.sp_tables"
        );
        // Unescaping happens before bracket-doubling: the escaped `_`
        // disappears, then the standalone `]` still gets doubled.
        assert_eq!(
            qualified_proc_name(&Some(r"a\_b]c".to_string()), "sp_tables"),
            "[a_b]]c].sys.sp_tables"
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
                SQL_NTS,
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
                    SQL_NTS,
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

    /// A named, boxed catalog-function call used by the parameterized
    /// cursor-state test below.
    type NamedCall<'a> = (&'a str, Box<dyn Fn() -> SqlReturn>);

    #[test]
    fn open_cursor_returns_24000_for_every_function() {
        // All seven functions share `run_catalog`'s cursor/exec-state guard;
        // one test per entry point prevents the gap the single-function
        // version of this test used to leave for the other six.
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().set_state(STMT_STATE_CURSOR_OPEN);

        let calls: [NamedCall; 7] = [
            (
                "SQLTablesW",
                Box::new(move || unsafe {
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
                }),
            ),
            (
                "SQLColumnsW",
                Box::new(move || unsafe {
                    sql_columns_w(
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
                }),
            ),
            (
                "SQLPrimaryKeysW",
                Box::new(move || unsafe {
                    sql_primary_keys_w(
                        h.stmt,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                    )
                }),
            ),
            (
                "SQLForeignKeysW",
                Box::new(move || unsafe {
                    sql_foreign_keys_w(
                        h.stmt,
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
                }),
            ),
            (
                "SQLStatisticsW",
                Box::new(move || unsafe {
                    sql_statistics_w(
                        h.stmt,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                        0,
                        0,
                    )
                }),
            ),
            (
                "SQLSpecialColumnsW",
                Box::new(move || unsafe {
                    sql_special_columns_w(
                        h.stmt,
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
                }),
            ),
            (
                "SQLProceduresW",
                Box::new(move || unsafe {
                    sql_procedures_w(
                        h.stmt,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        0,
                    )
                }),
            ),
        ];

        for (name, call) in calls {
            let ret = call();
            assert_eq!(ret, SQL_ERROR, "{name}");
            let state = stmt.inner.lock().unwrap();
            assert_eq!(
                state.diag_records[0].sql_state,
                crate::api::sqlstate::SQLSTATE_24000,
                "{name}"
            );
        }
    }
}
