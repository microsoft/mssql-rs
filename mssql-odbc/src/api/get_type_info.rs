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

use std::time::Instant;

use tracing::{debug, error};

use mssql_tds::connection::tds_client::ExecuteOptions;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

use super::exec_common::{
    claim_connection, deduct_query_timeout, fail_with_tds, finish_execute, flush_pending_unprepare,
    query_timeout_expired_error,
};
use super::sqlstate::*;
use super::txn::begin_transaction_if_manual;
use super::util::COLMETA_NULLABLE_FLAG;
use crate::api::odbc_types::{
    SQL_ALL_TYPES, SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_CHAR, SQL_DATETIME, SQL_DECIMAL,
    SQL_DOUBLE, SQL_ERROR, SQL_FLOAT, SQL_GUID, SQL_INTEGER, SQL_INTERVAL_MINUTE_TO_SECOND,
    SQL_INTERVAL_YEAR, SQL_INVALID_HANDLE, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NUMERIC,
    SQL_REAL, SQL_SMALLINT, SQL_SS_TABLE, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_SS_VARIANT,
    SQL_SS_VECTOR, SQL_SS_XML, SQL_TIME, SQL_TIMESTAMP, SQL_TINYINT, SQL_TYPE_DATE,
    SQL_TYPE_DRIVER_START, SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR,
    SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::error::free_errors;
use crate::handles::stmt::{
    STMT_STATE_CURSOR_OPEN, STMT_STATE_EXEC_CONTEXT, STMT_STATE_EXEC_STARTED, STMT_STATE_PREPARED,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Catalog procedure returning the ODBC `SQLGetTypeInfo` result set. This
/// driver targets SQL Server 2016+, so the Katmai (`_100`) form is always
/// available; selecting `_90`/`_170` by negotiated server version and vector
/// support is deferred until that version is surfaced to the ODBC layer.
const DATATYPE_INFO_PROC: &str = "[sys].sp_datatype_info_100";

/// `@ODBCVer` value sent for ODBC 3.x applications against a Katmai+ server.
// Classic SQLGetTypeInfoW sends this pseudo-version 4 on Yukon-or-newer servers
// (`sqlcdd.cpp:2206`, `fODBCVer = ISYUKON(lpdbc) ? 4 : 3`), where the catalog
// functions send 3. The comment beside it attributes the 4 to making
// sp_datatype_info report NULL precision for XML; that effect no longer shows
// on a modern server (both drivers report a non-NULL COLUMN_SIZE there), but
// the value msodbcsql sends is 4 regardless, which is what parity requires.
const ODBC_VER_YUKON: u8 = 4;

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

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`.
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

    // Validate the requested type and reset prior context under the stmt lock.
    // Validation runs before any state mutation so an invalid type leaves the
    // statement unchanged, matching msodbcsql.
    let query_timeout = {
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
            TypeClass::NotAnOdbcType => {
                error!(
                    data_type,
                    "SQLGetTypeInfoW: driver-range type is not reported as an ODBC type"
                );
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
        stmt_state.clear_result_metadata();
        stmt_state.reset_row_stream();
        // A cached prepared plan is superseded; release its server handle
        // (deferred) once we hold the client below.
        stmt_state.orphan_prepared_handle();
        stmt_state.prepared = None;
        stmt_state.parameter_metadata.clear();
        stmt_state.clear_state(STMT_STATE_PREPARED);
        stmt_state.set_state(STMT_STATE_EXEC_STARTED);
        stmt_state.query_timeout
    };

    // `@data_type` is positional and uses the ODBC 3.x identifier unchanged.
    let (positional, named) = type_info_rpc_params(data_type);

    let mut client = match claim_connection(dbc, stmt, statement_handle, "SQLGetTypeInfoW") {
        Ok(client) => client,
        Err(rc) => return rc,
    };
    let budget = query_timeout;
    let started = Instant::now();

    // Release any handle orphaned by the reset above before running the RPC.
    // `SQL_ATTR_QUERY_TIMEOUT` bounds this call: msodbcsql runs SQLGetTypeInfo
    // through `SQLExecDirectW` itself (`sqlcdd.cpp:2239`), inheriting
    // `GetQueryTimeOut(lpstmt)`, and the function's documented SQLSTATE table
    // lists `HYT00` naming this attribute. `0` (the default) stays unlimited.
    flush_pending_unprepare(dbc, stmt, &mut client, "SQLGetTypeInfoW", query_timeout);

    let query_timeout = match deduct_query_timeout(budget, started.elapsed()) {
        Ok(remaining) => remaining,
        Err(()) => {
            return fail_with_tds(
                dbc,
                stmt,
                statement_handle,
                client,
                &query_timeout_expired_error(),
            );
        }
    };

    if let Err(e) = begin_transaction_if_manual(dbc, &mut client, "SQLGetTypeInfoW", query_timeout)
    {
        return fail_with_tds(dbc, stmt, statement_handle, client, &e);
    }

    let query_timeout = match deduct_query_timeout(budget, started.elapsed()) {
        Ok(remaining) => remaining,
        Err(()) => {
            return fail_with_tds(
                dbc,
                stmt,
                statement_handle,
                client,
                &query_timeout_expired_error(),
            );
        }
    };

    let exec_result = dbc.runtime.block_on(client.execute_stored_procedure(
        DATATYPE_INFO_PROC.to_string(),
        Some(positional),
        named,
        ExecuteOptions::new().timeout_secs(query_timeout),
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
    rename_type_info_columns(stmt);
    clear_type_info_nullable(stmt);
    rc
}

/// Builds the RPC arguments for the type-info catalog proc: the positional
/// `@data_type`, forwarded unchanged, and the named `@ODBCVer`. Split out so a
/// test can assert what actually goes on the wire — pinning `ODBC_VER_YUKON`
/// alone would still pass if this call site stopped using it.
fn type_info_rpc_params(data_type: SqlSmallInt) -> (Vec<RpcParameter>, Option<Vec<RpcParameter>>) {
    let positional = vec![RpcParameter::new(
        None,
        StatusFlags::NONE,
        SqlType::SmallInt(Some(data_type)),
    )];
    let named = Some(vec![RpcParameter::new(
        Some("@ODBCVer".to_string()),
        StatusFlags::NONE,
        SqlType::TinyInt(Some(ODBC_VER_YUKON)),
    )]);
    (positional, named)
}

/// Outcome of validating a caller-supplied `SQLGetTypeInfo` `DataType`.
enum TypeClass {
    /// A supported SQL type, or `SQL_ALL_TYPES` — run the catalog proc.
    Valid,
    /// An id in the driver-specific range that is not surfaced as an ODBC data
    /// type — reported as HYC00.
    NotAnOdbcType,
    /// Not a recognized SQL type — reported as HY004.
    Invalid,
}

/// Classifies a `DataType` argument the same way msodbcsql does before issuing
/// the catalog RPC (`odbc/sqlcdd.cpp`, `SQLGetTypeInfoW`), which is a three-step
/// sequence rather than a single table:
///
/// 1. `FInternalSqlType` (`odbc/sqlcprot.h`) rejects the internal "MAPPED" ids
///    and `SQL_SS_TABLE` with HY004 (line 1999).
/// 2. The SS ids msodbcsql surfaces are folded to an internal id, and anything
///    still at or below `SQL_TYPE_DRIVER_START` (-80) is HYC00 (line 2035).
/// 3. `IsValidSqlType` runs last, but only its HY004 verdict aborts: line 2042
///    discards a HYC00 from it, so the interval types reach the RPC and come
///    back as an empty result set rather than an error.
///
/// `SQL_SS_VECTOR` is the one id that deliberately does not follow msodbcsql
/// yet; see its arm below.
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
        // Step 3: `IsValidSqlType` calls these HYC00, which `SQLGetTypeInfoW`
        // then discards, so the caller gets an empty result set.
        SQL_INTERVAL_YEAR..=SQL_INTERVAL_MINUTE_TO_SECOND => TypeClass::Valid,
        // Step 1: a table type is HY004, not HYC00, even though its id is far
        // below the driver-range bound checked next. It is the only member of
        // `FInternalSqlType`'s set that sits below that bound and so needs an
        // arm of its own — the internal `*_MAPPED` ids are `SQL_VARCHAR + 1..7`
        // (13-19) and the `SQL_NATIVE_*` ids are -21/-22, so all of them reach
        // the final `Invalid` arm and get HY004 without special-casing.
        SQL_SS_TABLE => TypeClass::Invalid,
        // Deliberately not msodbcsql's answer. msodbcsql accepts this id, but
        // only because it also switches the catalog proc: `sp_datatype_info_170`
        // when the connection negotiated vector support, `sp_datatype_info_100`
        // otherwise (`sqlcdd.cpp:1931`, selected at lines 2049-2056). This
        // driver always calls `_100`, which has no vector row, so accepting the
        // id would report success while telling the application the type does
        // not exist. HYC00 says "not implemented", which is true until the
        // negotiated vector capability is surfaced from `mssql-tds` to this
        // layer and `_170` can be selected; accept it in the arm above at that
        // point. Tracked by AB#48326 (P9g: Vector parameter binding and the
        // SQL_SS_VECTOR client struct).
        SQL_SS_VECTOR => TypeClass::NotAnOdbcType,
        // Step 2: unlike the SS types above, `SQL_SS_UDT` has no internal
        // "MAPPED" form, so it — and every other unmapped id in the driver
        // range — falls through to the HYC00 bound.
        d if d <= SQL_TYPE_DRIVER_START => TypeClass::NotAnOdbcType,
        _ => TypeClass::Invalid,
    }
}

/// ODBC 3.x column names for the three type-info ordinals (3, 11, 12) that
/// `sp_datatype_info_*` emits under generic names.
fn type_info_column_names() -> [&'static str; 3] {
    ["COLUMN_SIZE", "FIXED_PREC_SCALE", "AUTO_UNIQUE_VALUE"]
}

/// Zero-based column indices (for the 1-based ODBC ordinals 3, 11, 12) paired
/// with the ODBC 3.x name each should take.
fn type_info_column_renames() -> [(usize, &'static str); 3] {
    let [col3, col11, col12] = type_info_column_names();
    [(2, col3), (10, col11), (11, col12)]
}

/// Renames the three catalog-proc columns (ODBC ordinals 3, 11, 12) to the names
/// an ODBC 3.x application expects, matching msodbcsql's
/// `SetColNames(COL(3)|COL(11)|COL(12), ...)` post-processing so `SQLDescribeCol`
/// reports identical column names.
fn rename_type_info_columns(stmt: &StmtHandle) {
    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLGetTypeInfoW: stmt mutex poisoned renaming columns");
        return;
    };
    let cols = &mut stmt_state.column_metadata;
    for (idx, name) in type_info_column_renames() {
        if let Some(col) = cols.get_mut(idx) {
            col.column_name = name.to_string();
        }
    }
    stmt_state.refresh_metadata_caches();
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
    use crate::api::odbc_types::{SQL_NULL_HANDLE, SQL_SS_UDT};
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe { sql_get_type_info_w(SQL_NULL_HANDLE, SQL_ALL_TYPES) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    /// `SQL_ATTR_QUERY_TIMEOUT` must bound `SQLGetTypeInfo`, not just
    /// `SQLExecute`/`SQLExecDirectW`. msodbcsql runs this function through
    /// `SQLExecDirectW` itself (`sqlcdd.cpp:2239`), so it inherits
    /// `GetQueryTimeOut(lpstmt)`, and the function's documented SQLSTATE table
    /// lists `HYT00` naming this attribute.
    ///
    /// The delay is applied to the `sp_datatype_info_100` RPC response itself
    /// (via `RPC_DELAY_KEY`), not to the transaction begin, so this fails if
    /// the timeout stops reaching the RPC's own `ExecuteOptions` — the exact
    /// regression mssql-rs#466 describes, where passing `()` left
    /// `remaining_request_timeout` unset for the whole batch.
    #[test]
    fn get_type_info_query_timeout_bounds_a_longer_server_delay() {
        use crate::handles::dbc::DbcHandle;
        use mssql_mock_tds::QueryResponse;
        use std::time::{Duration, Instant};

        const RESPONSE_DELAY: Duration = Duration::from_secs(8);
        const STMT_TIMEOUT_SECS: u32 = 1;
        // Comfortably above STMT_TIMEOUT_SECS plus connection/RTT overhead,
        // comfortably below RESPONSE_DELAY — the gap is what proves the
        // statement timeout, not the server delay, ended the wait.
        const BOUND: Duration = Duration::from_secs(5);

        let h = TestHandles::with_env_dbc_stmt();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mock_server =
            crate::test_support::connect_mock_server(dbc, "SELECT 1", QueryResponse::select_one());
        mock_server.set_rpc_delay(RESPONSE_DELAY);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().query_timeout = STMT_TIMEOUT_SECS;

        let started = Instant::now();
        let ret = sql_get_type_info_w_safe(h.stmt, stmt, SQL_ALL_TYPES);
        let elapsed = started.elapsed();

        assert_eq!(ret, SQL_ERROR);
        assert!(
            elapsed < BOUND,
            "SQLGetTypeInfoW took {elapsed:?} — a {STMT_TIMEOUT_SECS}s SQL_ATTR_QUERY_TIMEOUT \
             must bound the wait well below the server's {RESPONSE_DELAY:?} delay"
        );
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state, *b"HYT00",
            "a query-timeout expiry must report HYT00, got {:?}",
            state.diag_records[0].sql_state
        );
    }

    /// The AC2 counterpart to the test above: `SQL_ATTR_QUERY_TIMEOUT` must
    /// also bound the implicit transaction begin that precedes the RPC. The
    /// sibling only delays the RPC response, so reverting the pre-execute
    /// arguments to `0` left it green; this delays only the Begin request.
    #[test]
    fn get_type_info_query_timeout_bounds_a_delayed_implicit_transaction_begin() {
        use crate::handles::dbc::DbcHandle;
        use mssql_mock_tds::QueryResponse;
        use std::time::{Duration, Instant};

        const BEGIN_DELAY: Duration = Duration::from_secs(8);
        const STMT_TIMEOUT_SECS: u32 = 1;
        const BOUND: Duration = Duration::from_secs(5);

        let h = TestHandles::with_env_dbc_stmt();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mock_server =
            crate::test_support::connect_mock_server(dbc, "SELECT 1", QueryResponse::select_one());
        mock_server.set_tm_begin_delay(BEGIN_DELAY);
        dbc.inner.lock().unwrap().autocommit = false;

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().query_timeout = STMT_TIMEOUT_SECS;

        let started = Instant::now();
        let ret = sql_get_type_info_w_safe(h.stmt, stmt, SQL_ALL_TYPES);
        let elapsed = started.elapsed();

        assert_eq!(ret, SQL_ERROR);
        assert!(
            elapsed < BOUND,
            "SQLGetTypeInfoW took {elapsed:?} — a {STMT_TIMEOUT_SECS}s SQL_ATTR_QUERY_TIMEOUT \
             must bound the implicit transaction begin well below the server's \
             {BEGIN_DELAY:?} delay"
        );
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state, *b"HYT00",
            "a query-timeout expiry must report HYT00, got {:?}",
            state.diag_records[0].sql_state
        );
    }

    /// The `SQLGetTypeInfo` counterpart to `catalog.rs`'s
    /// `catalog_query_timeout_exhausted_by_unprepare_fails_before_sending`:
    /// a swallowed best-effort unprepare timeout must leave the budget
    /// exhausted and stop the call before the type-info RPC is sent.
    #[test]
    fn get_type_info_query_timeout_exhausted_by_unprepare_fails_before_sending() {
        use crate::handles::dbc::DbcHandle;
        use mssql_mock_tds::QueryResponse;
        use std::time::{Duration, Instant};

        const RESPONSE_DELAY: Duration = Duration::from_secs(8);
        const STMT_TIMEOUT_SECS: u32 = 1;
        // A sanity bound only. The real discriminator is the message
        // assertion below: timing cannot separate the two paths, because a
        // bypassed deduction just lets the RPC time out on its own budget
        // and reproduce HYT00 just as quickly.
        const BOUND: Duration = Duration::from_secs(5);

        let h = TestHandles::with_env_dbc_stmt();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mock_server =
            crate::test_support::connect_mock_server(dbc, "SELECT 1", QueryResponse::select_one());
        mock_server.set_rpc_delay(RESPONSE_DELAY);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        crate::test_support::arm_pending_unprepare(dbc, stmt);
        stmt.inner.lock().unwrap().query_timeout = STMT_TIMEOUT_SECS;

        let started = Instant::now();
        let ret = sql_get_type_info_w_safe(h.stmt, stmt, SQL_ALL_TYPES);
        let elapsed = started.elapsed();

        assert_eq!(ret, SQL_ERROR);
        assert!(
            elapsed < BOUND,
            "SQLGetTypeInfoW took {elapsed:?} — the {STMT_TIMEOUT_SECS}s budget was already spent \
             by the unprepare, so the type-info RPC must not have been sent at all"
        );
        let state = stmt.inner.lock().unwrap();
        assert_eq!(
            state.diag_records[0].sql_state, *b"HYT00",
            "an exhausted budget must report HYT00, got {:?}",
            state.diag_records[0].sql_state
        );
        // This is what makes the test mutation-resistant: the two paths carry
        // different text. Reaching the RPC and timing out there yields
        // "Elapsed: deadline has elapsed", so only the pre-send budget check
        // produces this message.
        assert!(
            state.diag_records[0]
                .message
                .contains("expired before the statement could be sent"),
            "the budget must be found exhausted before the RPC is sent, not by the RPC's own \
             timeout: {}",
            state.diag_records[0].message
        );
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
    fn type_info_column_names_are_odbc3_names() {
        assert_eq!(
            type_info_column_names(),
            ["COLUMN_SIZE", "FIXED_PREC_SCALE", "AUTO_UNIQUE_VALUE"]
        );
    }

    #[test]
    fn legacy_and_odbc3_datetime_identifiers_remain_valid() {
        for data_type in [
            SQL_DATETIME,
            SQL_TIME,
            SQL_TIMESTAMP,
            SQL_TYPE_DATE,
            SQL_TYPE_TIME,
            SQL_TYPE_TIMESTAMP,
        ] {
            assert!(matches!(classify_sql_type(data_type), TypeClass::Valid));
        }
    }

    /// Every arm of the classification table, against the three-step sequence in
    /// `SQLGetTypeInfoW` (`odbc/sqlcdd.cpp` lines 1999 / 2035 / 2042).
    /// Every id `FInternalSqlType` rejects (`odbc/sqlcprot.h:2388`) must be
    /// HY004, not HYC00. The internal `*_MAPPED` ids are `SQL_VARCHAR + 1..=7`
    /// (13-19) and `SQL_NATIVE_SMALLDATETIME`/`SQL_NATIVE_LEGACY_DATETIME` are
    /// -21/-22, so none of them reaches the `<= SQL_TYPE_DRIVER_START` (-80)
    /// arm; only `SQL_SS_TABLE` (-153) does, which is why it is the one
    /// special case. Pinned so widening the driver-range arm cannot silently
    /// turn any of these into HYC00.
    #[test]
    fn internal_mapped_identifiers_are_hy004_not_hyc00() {
        const SQL_VARIANT_MAPPED: SqlSmallInt = SQL_VARCHAR + 1;
        const SQL_VECTOR_MAPPED: SqlSmallInt = SQL_VARCHAR + 7;
        const SQL_NATIVE_SMALLDATETIME: SqlSmallInt = -21;
        const SQL_NATIVE_LEGACY_DATETIME: SqlSmallInt = -22;

        for data_type in SQL_VARIANT_MAPPED..=SQL_VECTOR_MAPPED {
            assert!(
                matches!(classify_sql_type(data_type), TypeClass::Invalid),
                "mapped id {data_type} must be HY004"
            );
        }
        for data_type in [SQL_NATIVE_SMALLDATETIME, SQL_NATIVE_LEGACY_DATETIME] {
            assert!(
                matches!(classify_sql_type(data_type), TypeClass::Invalid),
                "native id {data_type} must be HY004"
            );
        }
        assert!(matches!(
            classify_sql_type(SQL_SS_TABLE),
            TypeClass::Invalid
        ));
    }

    #[test]
    fn classification_matches_msodbcsql_for_every_arm() {
        // Mapped to an internal id before the driver-range bound, so valid.
        for data_type in [
            SQL_SS_VARIANT,
            SQL_SS_XML,
            SQL_SS_TIME2,
            SQL_SS_TIMESTAMPOFFSET,
        ] {
            assert!(
                matches!(classify_sql_type(data_type), TypeClass::Valid),
                "{data_type} is folded to a *_MAPPED id and accepted"
            );
        }

        // `IsValidSqlType` answers HYC00 for these, and line 2042 discards it.
        for data_type in [
            SQL_INTERVAL_YEAR,
            SQL_INTERVAL_YEAR + 6,
            SQL_INTERVAL_MINUTE_TO_SECOND,
        ] {
            assert!(
                matches!(classify_sql_type(data_type), TypeClass::Valid),
                "interval {data_type} reaches the RPC and returns no rows"
            );
        }

        // `FInternalSqlType` rejects a table type outright, so it is HY004 even
        // though -153 is below the driver-range bound tested afterwards.
        assert!(matches!(
            classify_sql_type(SQL_SS_TABLE),
            TypeClass::Invalid
        ));

        // Unmapped ids at or below the bound are HYC00.
        for data_type in [SQL_SS_UDT, SQL_TYPE_DRIVER_START, -200] {
            assert!(
                matches!(classify_sql_type(data_type), TypeClass::NotAnOdbcType),
                "{data_type} is in the driver range"
            );
        }

        // Just above the bound, and unrecognized, so HY004 rather than HYC00.
        for data_type in [SQL_TYPE_DRIVER_START + 1, -21, -22, 999] {
            assert!(
                matches!(classify_sql_type(data_type), TypeClass::Invalid),
                "{data_type} is not a recognized SQL type"
            );
        }
    }

    /// The deliberate exception: msodbcsql accepts `SQL_SS_VECTOR`, but only
    /// together with the `sp_datatype_info_170` selection this driver does not
    /// have yet. Flip this to `Valid` when that lands.
    #[test]
    fn vector_is_deferred_until_the_170_catalog_proc_is_selectable() {
        assert!(matches!(
            classify_sql_type(SQL_SS_VECTOR),
            TypeClass::NotAnOdbcType
        ));
        assert_eq!(DATATYPE_INFO_PROC, "[sys].sp_datatype_info_100");
    }

    /// `SQLGetTypeInfo` sends the Yukon pseudo-version, not the 3 the catalog
    /// functions send (`sqlcdd.cpp:2206` versus `sqlcdd.cpp:1814`). Asserted
    /// over the constructed parameters rather than the constant, so replacing
    /// the call site's `ODBC_VER_YUKON` with a literal, or dropping the named
    /// parameter entirely, fails here. The live suite cannot cover this: the
    /// XML NULL-precision effect msodbcsql's comment attributes to the value no
    /// longer reproduces. Capturing the RPC in `mssql-mock-tds` would assert it
    /// end to end, but that crate exposes no request-capture API yet.
    #[test]
    fn type_info_rpc_sends_the_yukon_pseudo_version_and_the_unmodified_type() {
        assert_eq!(ODBC_VER_YUKON, 4);

        let (positional, named) = type_info_rpc_params(SQL_TYPE_TIMESTAMP);

        assert_eq!(positional.len(), 1);
        let positional_debug = format!("{:?}", positional[0]);
        assert!(
            positional_debug.contains(&format!("SmallInt(Some({SQL_TYPE_TIMESTAMP}))")),
            "the requested type must be forwarded unchanged, got: {positional_debug}"
        );

        let named = named.expect("@ODBCVer is sent for every 3.x application");
        assert_eq!(named.len(), 1);
        let named_debug = format!("{:?}", named[0]);
        assert!(
            named_debug.contains("\"@ODBCVer\""),
            "expected an @-prefixed parameter name, got: {named_debug}"
        );
        assert!(
            named_debug.contains("TinyInt(Some(4))"),
            "@ODBCVer must be TINYINT 4, got: {named_debug}"
        );
    }

    #[test]
    fn type_info_column_renames_pair_ordinals_with_odbc3_names() {
        assert_eq!(
            type_info_column_renames(),
            [
                (2, "COLUMN_SIZE"),
                (10, "FIXED_PREC_SCALE"),
                (11, "AUTO_UNIQUE_VALUE")
            ]
        );
    }

    #[test]
    fn rename_type_info_columns_is_a_noop_without_metadata() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        rename_type_info_columns(stmt);
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
