// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFreeHandle — the ODBC handle deallocation entry point.

use tracing::{debug, error};

use super::exec_common::{return_client_idle, try_claim_idle_client};
use crate::api::odbc_types::{
    SQL_ERROR, SQL_HANDLE_DBC, SQL_HANDLE_DBC_INFO_TOKEN, SQL_HANDLE_DESC, SQL_HANDLE_ENV,
    SQL_HANDLE_STMT, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn, SqlSmallInt,
};
use crate::api::sqlstate::{ERR_INVALID_USE_OF_AUTO_DESC, SQLSTATE_HY000, post_diag};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
use crate::handles::{
    DbcHandle, DescHandle, EnvHandle, HandleType, StmtHandle, free_handle, handle_from_raw,
    live_type,
};
use mssql_tds::connection::tds_client::StatementId;

/// Implementation of [`SQLFreeHandle`](super::exports::SQLFreeHandle).
///
/// # Safety
/// See the exported function's doc for caller requirements.
pub(crate) unsafe fn sql_free_handle(handle_type: SqlSmallInt, handle: SqlHandle) -> SqlReturn {
    debug!(handle_type, ?handle, "SQLFreeHandle called");

    crate::ffi_entry!("SQLFreeHandle", {
        if handle.is_null() {
            error!("SQLFreeHandle: handle is null");
            return SQL_INVALID_HANDLE;
        }

        match handle_type {
            SQL_HANDLE_ENV => unsafe { free_env(handle) },
            SQL_HANDLE_DBC => unsafe { free_dbc(handle) },
            SQL_HANDLE_STMT => unsafe { free_stmt(handle) },
            SQL_HANDLE_DESC => unsafe { free_desc(handle) },
            SQL_HANDLE_DBC_INFO_TOKEN => {
                error!(
                    handle_type,
                    "SQLFreeHandle: handle type not yet implemented"
                );
                SQL_ERROR
            }
            _ => {
                error!(handle_type, "SQLFreeHandle: unknown handle type");
                SQL_INVALID_HANDLE
            }
        }
    })
}

/// Mirrors msodbcsql's `SQLFreeEnv` behavior.
///
/// No mutex is acquired - per the ODBC spec, the DM guarantees the
/// connection count on this ENV is 0 before calling `SQLFreeEnv`. DM also
/// ensures no concurrent SQLFreeHandle calls on the same handle. Returns
/// `SQL_INVALID_HANDLE`, per spec, if the handle is live but of some other
/// type — matches `free_stmt`/`free_desc`'s identical check; unlike those
/// two, an ENV is never cascade-freed behind the DM's back, so there is no
/// legitimate stale-handle case here to distinguish from "wrong type."
///
/// # Safety
/// `handle` must be a live `EnvHandle` created by `alloc_env`.
unsafe fn free_env(handle: SqlHandle) -> SqlReturn {
    match live_type(handle) {
        Some(HandleType::Env) => {}
        other => {
            error!(
                ?handle,
                ?other,
                "SQLFreeHandle(ENV): handle is not a live ENV"
            );
            return SQL_INVALID_HANDLE;
        }
    }

    let env = unsafe { handle_from_raw::<EnvHandle>(handle) };
    debug_assert_eq!(
        env.object_type,
        HandleType::Env,
        "SQLFreeHandle(ENV): handle is not an ENV"
    );

    if let Ok(mut state) = env.inner.lock() {
        free_errors(&mut state);
    }

    debug_assert!(
        env.inner
            .lock()
            .map(|s| s.connections.is_empty())
            .unwrap_or(true),
        "SQLFreeHandle(ENV): DM should have freed all DBCs before calling SQLFreeEnv"
    );

    unsafe { free_handle::<EnvHandle>(handle) };
    SQL_SUCCESS
}

/// Mirrors msodbcsql's `SQLFreeConnect` behavior.
///
/// No DBC mutex is acquired — the DM guarantees the DBC is disconnected
/// before calling `SQLFreeConnect`, and `SQLDisconnect` drops all child
/// handles (statements, and their implicit descriptors, plus any explicit
/// descriptors — `sql_disconnect_safe`). msodbcsql's `SQLFreeConnect` doesn't
/// lock the connection mutex either. Returns `SQL_INVALID_HANDLE`, per spec,
/// if the handle is live but of some other type — matches `free_stmt`/
/// `free_desc`'s identical check; unlike those two, a DBC is never
/// cascade-freed behind the DM's back, so there is no legitimate
/// stale-handle case here to distinguish from "wrong type."
///
/// # Safety
/// `handle` must be a live `DbcHandle` created by `alloc_dbc`.
unsafe fn free_dbc(handle: SqlHandle) -> SqlReturn {
    match live_type(handle) {
        Some(HandleType::Dbc) => {}
        other => {
            error!(
                ?handle,
                ?other,
                "SQLFreeHandle(DBC): handle is not a live DBC"
            );
            return SQL_INVALID_HANDLE;
        }
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLFreeHandle(DBC): handle is not a DBC"
    );

    if let Ok(mut state) = dbc.inner.lock() {
        free_errors(&mut state);
    }

    debug_assert!(
        dbc.inner
            .lock()
            .map(|s| s.statements.is_empty())
            .unwrap_or(true),
        "SQLFreeHandle(DBC): DM should have freed all STMTs before calling SQLFreeConnect"
    );
    // Holds because every explicit descriptor must be freed (directly, or by
    // `sql_disconnect_safe` on disconnect) before its parent DBC — the
    // ordinary "free children before the parent" contract, not something
    // that depends on connection state — same pattern as msodbcsql's own
    // `assert(CItemsPl(lpdbc->lpplDAN) == 0)` (`sqlcconn.cpp:727`), an assert
    // rather than a defensive free loop here.
    debug_assert!(
        dbc.inner
            .lock()
            .map(|s| s.descriptors.is_empty())
            .unwrap_or(true),
        "SQLFreeHandle(DBC): DM should have freed all explicit DESCs before calling SQLFreeConnect"
    );

    // Unregister from parent ENV
    let env = unsafe { handle_from_raw::<EnvHandle>(dbc.parent_env) };
    {
        // Lock scope for ENV mutex - so that we unlock before deallocating the DBC
        let Ok(mut env_state) = env.inner.lock() else {
            error!(?handle, "SQLFreeHandle(DBC): env mutex poisoned");
            if let Ok(mut dbc_state) = dbc.inner.lock() {
                post_sql_error(
                    &mut dbc_state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error while freeing connection",
                );
            }
            return SQL_ERROR;
        };
        if let Some(i) = env_state.connections.iter().position(|&p| p == handle) {
            env_state.connections.swap_remove(i);
        }
    }

    unsafe { free_handle::<DbcHandle>(handle) };
    SQL_SUCCESS
}

/// Mirrors msodbcsql's `SQLFreeStmt(SQL_DROP)` behavior.
///
/// If the handle was already freed by an earlier `SQLDisconnect` cascading
/// cleanup (`sql_disconnect_safe`), returns `SQL_SUCCESS` immediately without
/// dereferencing it — that cleanup happens behind the Driver Manager's back,
/// so an application legitimately holding this now-stale handle can still
/// reach here (mssql-rs#400). Returns `SQL_INVALID_HANDLE`, per spec, if the
/// handle is live but of some other type. If the STMT is live and correctly
/// typed but somehow missing from its parent DBC's statement list — an
/// invariant break rather than an expected path, since "already freed" is
/// caught above — logs it and still returns `SQL_SUCCESS` rather than
/// double-freeing or panicking.
///
/// # Safety
/// `handle` must be a live `StmtHandle` created by `alloc_stmt`.
unsafe fn free_stmt(handle: SqlHandle) -> SqlReturn {
    match live_type(handle) {
        None => {
            debug!(?handle, "SQLFreeHandle(STMT): handle already freed, no-op");
            return SQL_SUCCESS;
        }
        Some(HandleType::Stmt) => {}
        Some(actual) => {
            error!(
                ?handle,
                ?actual,
                "SQLFreeHandle(STMT): handle is live but not a STMT"
            );
            return SQL_INVALID_HANDLE;
        }
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLFreeHandle(STMT): handle is not a STMT"
    );

    if let Ok(mut state) = stmt.inner.lock() {
        free_errors(&mut state);
    }

    // Lock parent DBC and try to unregister.
    let dbc = unsafe { handle_from_raw::<DbcHandle>(stmt.parent_dbc) };

    // Best-effort: release any server-side prepared handle(s) before the
    // statement is dropped, while the connection is still live and idle.
    best_effort_unprepare_on_free(handle, stmt, dbc);

    {
        // Lock scope for DBC mutex - so that we unlock before deallocating the STMT
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!(?handle, "SQLFreeHandle(STMT): dbc mutex poisoned");
            if let Ok(mut stmt_state) = stmt.inner.lock() {
                post_sql_error(
                    &mut stmt_state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error while freeing statement",
                );
            }
            return SQL_ERROR;
        };
        let Some(i) = dbc_state.statements.iter().position(|&p| p == handle) else {
            // Live but untracked: not the post-SQLDisconnect case (caught by
            // `live_type` above), so something removed it from the parent
            // without freeing it — an invariant break, not an expected path.
            error!(
                ?handle,
                "SQLFreeHandle(STMT): live handle missing from parent DBC"
            );
            debug_assert!(false, "live STMT not tracked by its parent DBC");
            return SQL_SUCCESS;
        };
        dbc_state.statements.swap_remove(i);
    }

    unsafe { free_handle::<StmtHandle>(handle) };
    SQL_SUCCESS
}

/// Mirrors msodbcsql's explicit-descriptor free path (`FreeDesc` called with a
/// null `lpstmt`, `sqlcdesc.cpp:5911-5976`): rejects an implicitly-allocated
/// descriptor with HY017 — implicit descriptors are owned by their statement
/// and can never be freed as explicit handles — otherwise walks every
/// statement on the owning connection and resets any whose active ARD/APD is
/// this descriptor back to implicit (`None`, meaning "use `StmtHandle::ard`/
/// `apd`"), then unregisters and frees it. A descriptor associated with
/// several statements at once is a supported case, not an error: every one of
/// them is reset, not just the first found.
///
/// If the handle was already freed by an earlier `SQLDisconnect` cascading
/// cleanup (`sql_disconnect_safe`), returns `SQL_SUCCESS` immediately without
/// dereferencing it — mirrors `free_stmt`'s identical guard and the same
/// reasoning (mssql-rs#400). Returns `SQL_INVALID_HANDLE`, per spec, if the
/// handle is live but of some other type. If the descriptor is live and
/// correctly typed but somehow missing from its parent DBC's descriptor
/// list — an invariant break rather than an expected path, since "already
/// freed" is caught above — logs it and still returns `SQL_SUCCESS` rather
/// than double-freeing or panicking.
///
/// # Safety
/// `handle` must be a live `DescHandle` created by `alloc_desc`.
unsafe fn free_desc(handle: SqlHandle) -> SqlReturn {
    match live_type(handle) {
        None => {
            debug!(?handle, "SQLFreeHandle(DESC): handle already freed, no-op");
            return SQL_SUCCESS;
        }
        Some(HandleType::Desc) => {}
        Some(actual) => {
            error!(
                ?handle,
                ?actual,
                "SQLFreeHandle(DESC): handle is live but not a DESC"
            );
            return SQL_INVALID_HANDLE;
        }
    }

    let desc = unsafe { handle_from_raw::<DescHandle>(handle) };
    debug_assert_eq!(
        desc.object_type,
        HandleType::Desc,
        "SQLFreeHandle(DESC): handle is not a DESC"
    );

    // Checked unconditionally, before taking any lock: `is_explicit` reads
    // only `alloc_type`, set once at construction and never mutated, so this
    // rejection cannot depend on whether `desc.inner`'s lock is healthy.
    // Nesting it inside the lock below used to mean a poisoned mutex skipped
    // this check entirely — silently letting an implicit descriptor (never
    // registered in `dbc_state.descriptors`, by design) fall through to the
    // "live handle missing from parent DBC" branch further down, which is
    // meant to catch a genuine invariant break, not this legitimate case.
    if !desc.is_explicit() {
        error!("SQLFreeHandle(DESC): cannot free an implicitly allocated descriptor");
        if let Ok(mut state) = desc.inner.lock() {
            post_diag(&mut state, ERR_INVALID_USE_OF_AUTO_DESC);
        }
        return SQL_ERROR;
    }

    if let Ok(mut state) = desc.inner.lock() {
        free_errors(&mut state);
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(desc.parent_dbc) };
    let Ok(mut dbc_state) = dbc.inner.lock() else {
        error!(?handle, "SQLFreeHandle(DESC): dbc mutex poisoned");
        if let Ok(mut state) = desc.inner.lock() {
            post_sql_error(
                &mut state,
                SQLSTATE_HY000,
                0,
                "Internal error while freeing descriptor",
            );
        }
        return SQL_ERROR;
    };

    let Some(i) = dbc_state.descriptors.iter().position(|&p| p == handle) else {
        // Live but untracked: not the post-SQLDisconnect case (caught by
        // `live_type` above), so something removed it from the parent
        // without freeing it — an invariant break, not an expected path.
        error!(
            ?handle,
            "SQLFreeHandle(DESC): live handle missing from parent DBC"
        );
        debug_assert!(false, "live DESC not tracked by its parent DBC");
        return SQL_SUCCESS;
    };

    for &stmt_raw in &dbc_state.statements {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt_raw) };
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            // A poisoned STMT mutex means we can't safely confirm or clear
            // its active_ard/active_apd. Freeing the descriptor anyway would
            // leave that statement's association dangling — a later
            // SQLGetStmtAttrW would hand the application a stale handle, and
            // the binding work in AB#47437 would dereference it — so refuse
            // the free instead: the descriptor stays intact and tracked in
            // `dbc_state.descriptors` for a retry, matching how every other
            // lock failure in this function is handled.
            error!(
                ?stmt_raw,
                "SQLFreeHandle(DESC): stmt mutex poisoned; refusing to free"
            );
            drop(dbc_state);
            if let Ok(mut state) = desc.inner.lock() {
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error while freeing descriptor",
                );
            }
            return SQL_ERROR;
        };
        if stmt_state.active_ard == Some(handle) {
            stmt_state.active_ard = None;
        }
        if stmt_state.active_apd == Some(handle) {
            stmt_state.active_apd = None;
        }
    }

    dbc_state.descriptors.swap_remove(i);
    drop(dbc_state);

    unsafe { free_handle::<DescHandle>(handle) };
    SQL_SUCCESS
}

/// Releases a statement's cached and pending prepared handles with
/// `sp_unprepare` before the statement is freed, so server-side plans don't
/// leak for the lifetime of the connection (mirrors msodbcsql dropping the
/// prepared handle on statement drop).
///
/// If the statement still has an open cursor, its result set is drained first
/// (via `close_cursor::drain_and_release`) so the trailing `@handle` token is
/// captured and the connection goes idle. The unprepare itself is best-effort
/// and non-fatal: it acts only when the connection is live and idle, reusing
/// [`try_claim_idle_client`] / [`return_client_idle`]. If the connection is
/// disconnected or busy with another statement, the handles are left for the
/// server to reclaim when the connection closes. No lock is held across I/O.
fn best_effort_unprepare_on_free(handle: SqlHandle, stmt: &StmtHandle, dbc: &DbcHandle) {
    // If a cursor is still open, drain it first: `drain_and_release` reads the
    // trailing `@handle` token (capturing it into `prepared`), returns the
    // client, and clears `active_stmt` — leaving the connection idle so the
    // unprepare below can claim it. Without this, a `prepare -> SQLExecute
    // (SELECT) -> SQLFreeHandle` sequence (no `SQLCloseCursor`) would skip the
    // unprepare and leak the handle.
    let cursor_open = stmt
        .inner
        .lock()
        .map(|s| s.has_state(STMT_STATE_CURSOR_OPEN))
        .unwrap_or(false);
    if cursor_open {
        super::close_cursor::drain_and_release(stmt, handle);
    }

    // A statement freed mid data-at-execution still owns the connection's
    // client, parked inside `DaeState`. Dropping the handle with it parked
    // would strand the client and leave the DBC recording the freed statement
    // as busy, so the connection could never be used again. `unwind_dae`
    // discards the half-written request, returns the client to the DBC, and
    // restores `prepared` / `pending_unprepare` — so the unprepare below then
    // releases the plan exactly as it would for any other statement.
    let needs_data = stmt.inner.lock().map(|s| s.needs_data()).unwrap_or(false);
    if needs_data {
        super::exec_common::unwind_dae(dbc, stmt, handle, None);
    }

    let (prepared, pending) = match stmt.inner.lock() {
        Ok(mut stmt_state) => (
            stmt_state.prepared.take().map(|p| p.stmt),
            stmt_state.pending_unprepare.take(),
        ),
        Err(_) => return,
    };
    let handles: Vec<StatementId> = [prepared.and_then(|p| p.id()), pending]
        .into_iter()
        .flatten()
        .collect();
    if handles.is_empty() {
        return;
    }

    // Claim the client only if connected and idle; otherwise skip and let the
    // server reclaim the handles when the connection closes.
    let Some(mut client) = try_claim_idle_client(dbc, handle) else {
        return;
    };

    // `unprepare` skips a handle from a superseded session (already gone
    // server-side) and releases a live one.
    for statement_id in handles {
        if let Err(e) = dbc.runtime.block_on(client.unprepare(statement_id, ())) {
            error!(%e, "SQLFreeHandle(STMT): sp_unprepare failed — handle leaked until disconnect");
        }
    }

    return_client_idle(dbc, handle, client);
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::odbc_types::{
        SQL_ATTR_ODBC_VERSION, SQL_HANDLE_ENV, SQL_NULL_HANDLE, SQL_OV_ODBC3_80,
    };
    use crate::api::set_env_attr::sql_set_env_attr;

    /// Allocate an ENV handle with ODBC 3.80 version set.
    fn alloc_env() -> SqlHandle {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);
        let ret = unsafe {
            sql_set_env_attr(
                env,
                SQL_ATTR_ODBC_VERSION,
                SQL_OV_ODBC3_80 as usize as *mut std::ffi::c_void,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        env
    }

    #[test]
    fn free_env_returns_success() {
        let env = alloc_env();

        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn free_dbc_returns_success() {
        let env = alloc_env();

        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    /// The type-confusion fix for `free_stmt`/`free_desc` (`live_type`
    /// checked before any dereference) has an ENV/DBC counterpart: without
    /// it, `SQLFreeHandle(SQL_HANDLE_ENV, <live DBC>)` would reach
    /// `handle_from_raw::<EnvHandle>` and reinterpret a `DbcHandle` as an
    /// `EnvHandle` — `debug_assert_eq!` on `object_type` can't catch this
    /// either, since it reads through the already-wrongly-typed reference,
    /// and `EnvHandle`/`DbcHandle` are neither `#[repr(C)]` nor the same
    /// size. `free_env` now checks `live_type` first, matching `free_stmt`/
    /// `free_desc`.
    #[test]
    fn free_env_on_a_live_dbc_handle_returns_invalid_handle() {
        let env = alloc_env();

        let mut dbc: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) },
            SQL_SUCCESS
        );

        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_ENV, dbc) },
            SQL_INVALID_HANDLE,
            "a live DBC handle passed as an ENV must be rejected, not reinterpreted"
        );

        // The DBC itself is untouched by the rejected call and still frees
        // normally as what it actually is.
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// Mirror of `free_env_on_a_live_dbc_handle_returns_invalid_handle`
    /// through `free_dbc`'s own type check: a live ENV passed where a DBC
    /// was expected must be rejected, not reinterpreted as a `DbcHandle`.
    #[test]
    fn free_dbc_on_a_live_env_handle_returns_invalid_handle() {
        let env = alloc_env();

        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_DBC, env) },
            SQL_INVALID_HANDLE,
            "a live ENV handle passed as a DBC must be rejected, not reinterpreted"
        );

        // The ENV itself is untouched by the rejected call and still frees
        // normally as what it actually is.
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_null_handle_returns_invalid() {
        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, ptr::null_mut()) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn free_unknown_type_returns_invalid() {
        let mut env: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(99, env) };
        assert_eq!(ret, SQL_INVALID_HANDLE);

        // env is still live — clean up
        unsafe { free_handle::<EnvHandle>(env) };
    }

    #[test]
    fn free_env_with_outstanding_dbc_fails_in_debug() {
        // The DM guarantees all DBCs are freed before calling SQLFreeEnv.
        // The driver trusts this and frees unconditionally (matching msodbcsql).
        // In debug builds, debug_assert! fires and catch_unwind returns SQL_ERROR.
        let env = alloc_env();

        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
        if cfg!(debug_assertions) {
            // debug_assert! panics, catch_unwind converts to SQL_ERROR.
            assert_eq!(ret, SQL_ERROR);
            // ENV was not freed due to panic — clean up both handles.
            unsafe { free_handle::<DbcHandle>(dbc) };
            unsafe { free_handle::<EnvHandle>(env) };
        } else {
            assert_eq!(ret, SQL_SUCCESS);
            // ENV freed, DBC orphaned — clean up directly.
            unsafe { free_handle::<DbcHandle>(dbc) };
        }
    }

    // --- Helper: alloc ENV + DBC for STMT tests ---
    fn alloc_env_dbc() -> (SqlHandle, SqlHandle) {
        let env = alloc_env();
        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);
        (env, dbc)
    }

    // --- Helper: alloc ENV + DBC, marked connected ---
    // Establishes a Connected state without a real TDS client, for tests
    // that want a realistic connected-session baseline (not required for
    // DESC allocation itself — see `alloc_desc`'s doc comment).
    fn alloc_env_dbc_connected() -> (SqlHandle, SqlHandle) {
        use crate::handles::dbc::ConnectionState;

        let (env, dbc) = alloc_env_dbc();
        let dbc_ref = unsafe { &*(dbc as *const DbcHandle) };
        dbc_ref.inner.lock().unwrap().connection_state = ConnectionState::Connected;
        (env, dbc)
    }

    #[test]
    fn free_stmt_returns_success() {
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// Reproduces mssql-rs#400: freeing a stale STMT handle a second time —
    /// standing in for the real scenario, where `SQLDisconnect` cascades-frees
    /// it first and an application legitimately still holds the handle
    /// afterward — must return `SQL_SUCCESS` without dereferencing it.
    ///
    /// Before the `is_live` guard, this would panic on the `debug_assert_eq!`
    /// a few lines into `free_stmt`: nothing reallocates the freed memory in
    /// this single-threaded test, so it still holds the `HandleType::Invalid`
    /// tombstone `free_handle` stamped on the first free, which doesn't match
    /// `HandleType::Stmt` — a panic `ffi_entry!` catches and reports as
    /// `SQL_ERROR`. That return code (not a crash) was the only prior signal
    /// something had gone wrong; on an allocator that reuses the block
    /// instead of leaving the tombstone intact (observed on macOS in CI),
    /// there would have been no signal at all. `is_live` now short-circuits
    /// before any of that, so this never reaches the assert either way.
    #[test]
    fn free_stmt_twice_is_a_no_op_not_a_panic() {
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );

        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) },
            SQL_SUCCESS,
            "freeing an already-freed STMT handle must no-op, not panic"
        );

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// `SQLFreeHandle` dispatches on the caller-supplied `handle_type` with
    /// no cross-check against the handle it's actually given. Before
    /// tracking `HandleType` in the live-handle registry, `is_live` only
    /// confirmed *some* handle was live at this address — so passing a live
    /// DESC where a STMT was expected still reached
    /// `handle_from_raw::<StmtHandle>` and reinterpreted a `DescHandle` as a
    /// `StmtHandle`, no address reuse required. Checking the type recorded
    /// at allocation time catches this before any dereference, matching the
    /// ODBC spec's own answer: `SQL_INVALID_HANDLE` for "the handle
    /// indicated by *Handle* was not a valid handle of the type indicated
    /// by *HandleType*."
    #[test]
    fn free_stmt_on_a_live_desc_handle_returns_invalid_handle() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut desc: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) },
            SQL_SUCCESS
        );

        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_STMT, desc) },
            SQL_INVALID_HANDLE,
            "a live DESC handle passed as a STMT must be rejected, not reinterpreted"
        );

        // The DESC itself is untouched by the rejected call and still frees
        // normally as what it actually is.
        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) },
            SQL_SUCCESS
        );
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// Freeing a statement mid data-at-execution must drain the parked
    /// sequence rather than drop the handle with it intact. A dropped
    /// `DaeState` takes the connection's client with it, leaving the DBC with
    /// no client and `active_stmt` pointing at freed memory.
    ///
    /// A unit test can only observe that the sequence is drained: returning the
    /// client to the DBC needs a real parked `TdsClient`, which `for_test`
    /// deliberately does not build. The connection-survives-free half is
    /// covered live by
    /// `FreeStatementMidDataAtExecutionIsRejectedUntilCancelled`, which also
    /// pins the driver-manager rejection this path only sees without a DM.
    #[test]
    fn free_stmt_mid_dae_drains_the_parked_sequence() {
        use crate::handles::stmt::DaeState;

        let (env, dbc) = alloc_env_dbc();
        let mut stmt: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );

        let stmt_ref = unsafe { &*(stmt as *const StmtHandle) };
        stmt_ref.inner.lock().unwrap().dae = Some(DaeState::for_test(Vec::new(), None));

        best_effort_unprepare_on_free(stmt, stmt_ref, unsafe { &*(dbc as *const DbcHandle) });

        assert!(
            !stmt_ref.inner.lock().unwrap().needs_data(),
            "the parked sequence must be unwound before the handle is dropped"
        );

        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) },
            SQL_SUCCESS
        );
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_stmt_unregisters_from_parent_dbc() {
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let dbc_ref = unsafe { &*(dbc as *const DbcHandle) };
        assert_eq!(dbc_ref.inner.lock().unwrap().statements.len(), 1);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(dbc_ref.inner.lock().unwrap().statements.is_empty());

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_dbc_with_outstanding_stmt_fails_in_debug() {
        // The DM guarantees all STMTs are freed before calling SQLFreeConnect.
        // The driver trusts this and frees unconditionally (matching msodbcsql).
        // In debug builds, debug_assert! fires and catch_unwind returns SQL_ERROR.
        let (env, dbc) = alloc_env_dbc();

        let mut stmt: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        if cfg!(debug_assertions) {
            // debug_assert! panics, catch_unwind converts to SQL_ERROR.
            assert_eq!(ret, SQL_ERROR);
            // DBC was not freed due to panic — clean up all handles.
            unsafe { free_handle::<StmtHandle>(stmt) };
            unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        } else {
            assert_eq!(ret, SQL_SUCCESS);
            // DBC freed, STMT orphaned — clean up directly.
            unsafe { free_handle::<StmtHandle>(stmt) };
        }

        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_desc_returns_success() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut desc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        assert_eq!(ret, SQL_SUCCESS);

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// Reproduces mssql-rs#400 for descriptors: same scenario and reasoning
    /// as `free_stmt_twice_is_a_no_op_not_a_panic`, but through `free_desc`'s
    /// own `debug_assert_eq!` a few lines in.
    #[test]
    fn free_desc_twice_is_a_no_op_not_a_panic() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut desc: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) },
            SQL_SUCCESS
        );

        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) },
            SQL_SUCCESS,
            "freeing an already-freed DESC handle must no-op, not panic"
        );

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// Mirror of `free_stmt_on_a_live_desc_handle_returns_invalid_handle`
    /// through `free_desc`'s own type check: a live STMT passed where a DESC
    /// was expected must be rejected, not reinterpreted as a `DescHandle`.
    #[test]
    fn free_desc_on_a_live_stmt_handle_returns_invalid_handle() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut stmt: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );

        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_DESC, stmt) },
            SQL_INVALID_HANDLE,
            "a live STMT handle passed as a DESC must be rejected, not reinterpreted"
        );

        assert_eq!(
            unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) },
            SQL_SUCCESS
        );
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_desc_unregisters_from_parent_dbc() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut desc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) };
        assert_eq!(ret, SQL_SUCCESS);

        let dbc_ref = unsafe { &*(dbc as *const DbcHandle) };
        assert_eq!(dbc_ref.inner.lock().unwrap().descriptors.len(), 1);

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(dbc_ref.inner.lock().unwrap().descriptors.is_empty());

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// `free_desc`'s "live but untracked by parent" branch documents a
    /// genuine invariant, not an expected path: once the `live_type` guard
    /// above already catches the ordinary "freed by `SQLDisconnect`'s
    /// cascade" case (mssql-rs#400), reaching here while still live and
    /// correctly typed means something else removed the descriptor from
    /// `dbc_state.descriptors` without freeing it. No public API can reach
    /// that state (it would need a panic between `swap_remove` and
    /// `free_handle`, which `ffi_entry!` swallows), so this test forces it
    /// directly to exercise the `debug_assert!` that documents it.
    #[test]
    fn free_desc_live_but_untracked_by_parent_fails_in_debug() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut desc: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) },
            SQL_SUCCESS
        );

        let dbc_ref = unsafe { &*(dbc as *const DbcHandle) };
        dbc_ref.inner.lock().unwrap().descriptors.clear();

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        if cfg!(debug_assertions) {
            assert_eq!(
                ret, SQL_ERROR,
                "debug_assert! must fire for a live-but-untracked descriptor"
            );
        } else {
            assert_eq!(ret, SQL_SUCCESS);
        }
        // Neither branch drops the box: the debug build panics before
        // reaching any drop code, and the release build's early return
        // skips it too — same cleanup either way.
        unsafe { free_handle::<DescHandle>(desc) };

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// A poisoned STMT mutex mid-reset must refuse the free rather than
    /// silently skip that statement and free the descriptor anyway: doing so
    /// would leave the statement's `active_ard`/`active_apd` pointing at
    /// freed memory, which a later `SQLGetStmtAttrW` would hand straight to
    /// the application. Refusing keeps the descriptor intact and tracked in
    /// `dbc_state.descriptors`, so it stays freeable once the poisoned
    /// statement is itself removed (via its own `SQLFreeHandle`, exercised
    /// below) — a real recovery path, not just "fails safely and stays
    /// stuck."
    #[test]
    fn free_desc_with_poisoned_stmt_mutex_refuses_to_free() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut stmt: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );

        let mut desc: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) },
            SQL_SUCCESS
        );

        // Associate the descriptor as the statement's active ARD directly
        // (bypassing SQLSetStmtAttrW's validation, which is not what's under
        // test here) and poison the statement's mutex by panicking while it
        // is held — the same technique used by
        // `catalog::tests::apply_catalog_metadata_poisoned_mutex_returns_error`.
        let stmt_ref = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        stmt_ref.inner.lock().unwrap().active_ard = Some(desc);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = stmt_ref.inner.lock().unwrap();
            panic!("poison the stmt lock");
        }));

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        assert_eq!(ret, SQL_ERROR);

        // The descriptor was not freed and is still tracked, not leaked into
        // an untracked, unreachable state.
        let dbc_ref = unsafe { &*(dbc as *const DbcHandle) };
        assert_eq!(dbc_ref.inner.lock().unwrap().descriptors, vec![desc]);

        // Freeing the poisoned statement itself tolerates its own poisoning
        // (matching `free_stmt`'s existing behavior) and removes it from the
        // DBC, after which the descriptor has nothing poisoned left to check
        // and frees normally.
        unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        let ret = unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        assert_eq!(ret, SQL_SUCCESS);

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// Implicit descriptors are owned by their statement and can never be
    /// freed as explicit handles (AB#47436 acceptance criteria) — matches the
    /// ODBC reference's HY017 for `SQLFreeHandle`. The diagnostic must land on
    /// the descriptor handle `SQLFreeHandle` tried (and failed) to free, per
    /// its own spec ("If SQLFreeHandle returns SQL_ERROR, the handle is still
    /// valid"), not on the parent STMT or DBC.
    #[test]
    fn free_implicit_desc_is_rejected() {
        use crate::api::sqlstate::SQLSTATE_HY017;

        let (env, dbc) = alloc_env_dbc();
        let mut stmt: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );
        let ard = unsafe { &*(stmt as *const StmtHandle) }.ard;

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DESC, ard) };
        assert_eq!(ret, SQL_ERROR);

        let desc = unsafe { handle_from_raw::<DescHandle>(ard) };
        let diag = desc.inner.lock().unwrap();
        assert_eq!(diag.diag_records.last().unwrap().sql_state, SQLSTATE_HY017);
        drop(diag);

        // The ARD is untouched: the statement (and its implicit descriptors)
        // still free normally.
        unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// Regression test: `free_implicit_desc_is_rejected` above only exercises
    /// the healthy-lock path. `is_explicit()` used to be checked *inside*
    /// `if let Ok(mut state) = desc.inner.lock()`, so a poisoned descriptor
    /// mutex skipped the whole block — including the HY017 rejection — and
    /// let an implicit descriptor (never registered in
    /// `dbc_state.descriptors`, by design) fall through all the way to the
    /// "live handle missing from parent DBC" branch, incorrectly firing that
    /// branch's `debug_assert!` in debug builds and silently returning
    /// `SQL_SUCCESS` for an unfreed, unfreeable handle in release builds.
    /// `is_explicit()` reads only `alloc_type`, set once at construction and
    /// never mutated, so the rejection must not depend on lock health at all.
    ///
    /// Calls `free_desc` directly rather than through `sql_free_handle`:
    /// the latter's `ffi_entry!` catches any panic and converts it to the
    /// same `SQL_ERROR` a correct HY017 rejection also returns, so a return
    /// code alone can't tell "rejected properly" apart from "panicked on the
    /// wrongly-firing `debug_assert!`, then caught." Catching the panic here
    /// instead keeps that distinction visible.
    #[test]
    fn free_implicit_desc_is_rejected_even_with_a_poisoned_mutex() {
        let (env, dbc) = alloc_env_dbc();
        let mut stmt: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, dbc, &mut stmt) },
            SQL_SUCCESS
        );
        let ard = unsafe { &*(stmt as *const StmtHandle) }.ard;

        let ard_ref = unsafe { handle_from_raw::<DescHandle>(ard) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ard_ref.inner.lock().unwrap();
            panic!("poison the ard lock");
        }));

        match std::panic::catch_unwind(|| unsafe { free_desc(ard) }) {
            Ok(ret) => assert_eq!(
                ret, SQL_ERROR,
                "an implicit descriptor must be rejected regardless of its own lock's health"
            ),
            Err(_) => panic!(
                "free_desc must reject an implicit descriptor via HY017, not panic on a \
                 debug_assert! that only fires because its own lock happens to be poisoned"
            ),
        }

        // The ARD is untouched: the statement (and its implicit descriptors)
        // still free normally, poisoned lock and all — matches
        // `free_stmt`/`free_desc`'s existing tolerance for their own
        // handle's lock elsewhere in this module.
        unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    /// `free_dbc`'s `debug_assert!` documents a genuine invariant, not a
    /// hypothetical one: reaching it with a non-empty `descriptors` list, as
    /// this test forces directly, means the app freed the DBC without first
    /// freeing (or disconnecting to free) every descriptor it allocated on
    /// it — an ordinary "free children before the parent" contract violation,
    /// exactly like this test's STMT twin above, which needs no connection
    /// state of its own to hold. This driver does not independently re-check
    /// that at `free_dbc` and trusts the DM instead, matching msodbcsql's own
    /// `assert(CItemsPl(lpdbc->lpplDAN) == 0)` (`sqlcconn.cpp:727`) — an
    /// assert, not a defensive free loop, at the exact same point.
    #[test]
    fn free_dbc_with_outstanding_desc_fails_in_debug() {
        let (env, dbc) = alloc_env_dbc_connected();

        let mut desc: SqlHandle = ptr::null_mut();
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) },
            SQL_SUCCESS
        );

        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        if cfg!(debug_assertions) {
            assert_eq!(ret, SQL_ERROR);
            // The failed free left `dbc` alive with `desc` still registered
            // on it (the assert fires before any actual cleanup runs) — free
            // the descriptor through the real entry point so it unregisters
            // from `dbc_state.descriptors` before the retry, instead of
            // dropping its box directly and leaving a dangling entry that
            // would trip the same assert again.
            unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
            unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        } else {
            // In release, the assert is a no-op and `free_dbc` already
            // dropped `dbc` (and its parent-ENV registration) despite the
            // outstanding descriptor, so `desc.parent_dbc` is now dangling —
            // the only safe cleanup left for `desc` itself is dropping its
            // own box directly, not routing through `sql_free_handle`, which
            // would dereference the freed `dbc`.
            assert_eq!(ret, SQL_SUCCESS);
            unsafe { free_handle::<DescHandle>(desc) };
        }

        unsafe { sql_free_handle(SQL_HANDLE_ENV, env) };
    }

    #[test]
    fn free_dbc_info_token_returns_error_not_implemented() {
        use crate::api::odbc_types::SQL_HANDLE_DBC_INFO_TOKEN;
        let ret = unsafe { sql_free_handle(SQL_HANDLE_DBC_INFO_TOKEN, 0x1 as SqlHandle) };
        assert_eq!(ret, SQL_ERROR);
    }
}
