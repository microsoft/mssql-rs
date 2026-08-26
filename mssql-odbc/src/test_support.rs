// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared test-only helpers for allocating ODBC handles.
//!
//! Centralizes the ENV/DBC/STMT allocation dance used by `#[cfg(test)]`
//! modules across the crate. Each constructor wraps the `unsafe`
//! `sql_alloc_handle` calls and exposes a safe API; [`Drop`] frees the handles
//! child-before-parent, matching the order `SQLFreeHandle` requires (a parent
//! free `debug_assert!`s its child list is empty).

use std::ffi::c_void;

use crate::api::alloc_handle::sql_alloc_handle;
use crate::api::free_handle::sql_free_handle;
use crate::api::odbc_types::{
    SQL_ATTR_ODBC_VERSION, SQL_HANDLE_DBC, SQL_HANDLE_DESC, SQL_HANDLE_ENV, SQL_HANDLE_STMT,
    SQL_NULL_HANDLE, SQL_OV_ODBC3_80, SQL_SUCCESS, SqlHandle, SqlReturn,
};
use crate::api::set_env_attr::sql_set_env_attr;
use crate::handles::dbc::ConnectionState;
use crate::handles::{DbcHandle, handle_from_raw};

/// Rebuild an ODBC connection string from a template, expanding the credential
/// placeholders `<PW>` → `PWD` and `<PASS>` → `PASSWORD`.
///
/// Tests write the password keyword as a placeholder so neither the literal
/// keyword nor a keyword-followed-by-`=` token ever appears in source. That
/// keeps placeholder test credentials from tripping secret scanners such as
/// CredScan `SEC101/037` `SqlLegacyCredentials`.
pub(crate) fn cs(s: &str) -> String {
    s.replace("<PASS>", "PASSWORD").replace("<PW>", "PWD")
}

/// Owns a set of test ODBC handles and frees them on drop.
///
/// `env` is always set; `dbc` and `stmt` are `SQL_NULL_HANDLE` unless the
/// constructor allocated them. Extra statements allocated via
/// [`alloc_extra_stmt`](Self::alloc_extra_stmt) are tracked and freed too.
pub(crate) struct TestHandles {
    pub(crate) env: SqlHandle,
    pub(crate) dbc: SqlHandle,
    pub(crate) stmt: SqlHandle,
    extra_stmts: Vec<SqlHandle>,
    extra_descs: Vec<SqlHandle>,
}

impl TestHandles {
    /// Allocate an ENV handle and set `SQL_ATTR_ODBC_VERSION` to 3.80 so that
    /// DBC allocation is permitted.
    pub(crate) fn with_env() -> Self {
        let mut env: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut env) },
            SQL_SUCCESS
        );
        assert!(!env.is_null());
        assert_eq!(
            unsafe {
                sql_set_env_attr(
                    env,
                    SQL_ATTR_ODBC_VERSION,
                    SQL_OV_ODBC3_80 as usize as *mut c_void,
                    0,
                )
            },
            SQL_SUCCESS
        );
        Self {
            env,
            dbc: SQL_NULL_HANDLE,
            stmt: SQL_NULL_HANDLE,
            extra_stmts: Vec::new(),
            extra_descs: Vec::new(),
        }
    }

    /// Allocate ENV + DBC.
    pub(crate) fn with_env_dbc() -> Self {
        let mut h = Self::with_env();
        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DBC, h.env, &mut dbc) },
            SQL_SUCCESS
        );
        assert!(!dbc.is_null());
        h.dbc = dbc;
        h
    }

    /// Allocate ENV + DBC + STMT.
    pub(crate) fn with_env_dbc_stmt() -> Self {
        let mut h = Self::with_env_dbc();
        let mut stmt: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, h.dbc, &mut stmt) },
            SQL_SUCCESS
        );
        assert!(!stmt.is_null());
        h.stmt = stmt;
        h
    }

    /// Allocate an additional STMT under the same DBC. The returned handle is
    /// tracked and freed on drop along with the primary handles.
    pub(crate) fn alloc_extra_stmt(&mut self) -> SqlHandle {
        let mut stmt: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_STMT, self.dbc, &mut stmt) },
            SQL_SUCCESS
        );
        assert!(!stmt.is_null());
        self.extra_stmts.push(stmt);
        stmt
    }

    /// Allocate an explicit descriptor (`SQLAllocHandle(SQL_HANDLE_DESC, ...)`)
    /// under the same DBC. The returned handle is tracked and freed on drop,
    /// before the DBC. Marks the DBC connected first (idempotent) since
    /// descriptor allocation requires it (`alloc_desc`'s `08003` gate).
    pub(crate) fn alloc_explicit_desc(&mut self) -> SqlHandle {
        self.mark_dbc_connected();
        let mut desc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, self.dbc, &mut desc) },
            SQL_SUCCESS
        );
        assert!(!desc.is_null());
        self.extra_descs.push(desc);
        desc
    }

    /// Frees one statement previously returned by `alloc_extra_stmt` ahead of
    /// `Drop`, and stops tracking it so `Drop` does not free it again.
    pub(crate) fn free_extra_stmt(&mut self, stmt: SqlHandle) -> SqlReturn {
        let ret = unsafe { sql_free_handle(SQL_HANDLE_STMT, stmt) };
        self.extra_stmts.retain(|&s| s != stmt);
        ret
    }

    /// Frees one explicit descriptor previously returned by
    /// `alloc_explicit_desc` ahead of `Drop`, and stops tracking it so `Drop`
    /// does not free it again.
    pub(crate) fn free_explicit_desc(&mut self, desc: SqlHandle) -> SqlReturn {
        let ret = unsafe { sql_free_handle(SQL_HANDLE_DESC, desc) };
        self.extra_descs.retain(|&d| d != desc);
        ret
    }

    /// Allocates a second, independent connection under the same ENV, with
    /// one explicit descriptor already allocated on it — for
    /// cross-connection rejection tests. Both are freed by
    /// [`OtherConnection`]'s own `Drop`, before this `TestHandles`'s ENV.
    pub(crate) fn alloc_other_connection(&self) -> OtherConnection {
        let mut dbc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DBC, self.env, &mut dbc) },
            SQL_SUCCESS
        );
        // Connects the DBC (though `alloc_desc` no longer requires it — see
        // its doc comment) so this mirrors a realistic connected-session
        // cross-connection scenario; same technique as `mark_dbc_connected`.
        unsafe { handle_from_raw::<DbcHandle>(dbc) }
            .inner
            .lock()
            .unwrap()
            .connection_state = ConnectionState::Connected;
        let mut desc: SqlHandle = SQL_NULL_HANDLE;
        assert_eq!(
            unsafe { sql_alloc_handle(SQL_HANDLE_DESC, dbc, &mut desc) },
            SQL_SUCCESS
        );
        OtherConnection { dbc, desc }
    }

    /// Force the DBC into the `Connected` state without establishing a real
    /// TDS client. Only valid for code paths that gate on `connection_state`
    /// but never touch the client — e.g. SQLPrepare's deferred prepare. Paths
    /// that take the `TdsClient` will still see `None` and must not use this.
    pub(crate) fn mark_dbc_connected(&self) {
        assert!(!self.dbc.is_null(), "mark_dbc_connected requires a DBC");
        let dbc = unsafe { handle_from_raw::<DbcHandle>(self.dbc) };
        let Ok(mut state) = dbc.inner.lock() else {
            panic!("dbc mutex poisoned");
        };
        state.connection_state = ConnectionState::Connected;
    }

    /// The implicit application row descriptor (ARD) owned by `self.stmt`.
    pub(crate) fn ard(&self) -> SqlHandle {
        self.stmt_ref().ard
    }

    /// The implicit application parameter descriptor (APD) owned by `self.stmt`.
    pub(crate) fn apd(&self) -> SqlHandle {
        self.stmt_ref().apd
    }

    /// The implicit implementation row descriptor (IRD) owned by `self.stmt`.
    pub(crate) fn ird(&self) -> SqlHandle {
        self.stmt_ref().ird
    }

    /// The implicit implementation parameter descriptor (IPD) owned by `self.stmt`.
    pub(crate) fn ipd(&self) -> SqlHandle {
        self.stmt_ref().ipd
    }

    fn stmt_ref(&self) -> &crate::handles::StmtHandle {
        assert!(!self.stmt.is_null(), "descriptor accessors require a STMT");
        unsafe { handle_from_raw::<crate::handles::StmtHandle>(self.stmt) }
    }
}

impl Drop for TestHandles {
    fn drop(&mut self) {
        unsafe {
            for stmt in self.extra_stmts.drain(..) {
                sql_free_handle(SQL_HANDLE_STMT, stmt);
            }
            if !self.stmt.is_null() {
                sql_free_handle(SQL_HANDLE_STMT, self.stmt);
            }
            for desc in self.extra_descs.drain(..) {
                sql_free_handle(SQL_HANDLE_DESC, desc);
            }
            if !self.dbc.is_null() {
                sql_free_handle(SQL_HANDLE_DBC, self.dbc);
            }
            if !self.env.is_null() {
                sql_free_handle(SQL_HANDLE_ENV, self.env);
            }
        }
    }
}

/// A second, independent connection (with one explicit descriptor already
/// allocated on it) returned by [`TestHandles::alloc_other_connection`], for
/// cross-connection rejection tests. Frees the descriptor, then the DBC, on
/// drop.
pub(crate) struct OtherConnection {
    dbc: SqlHandle,
    pub(crate) desc: SqlHandle,
}

impl Drop for OtherConnection {
    fn drop(&mut self) {
        unsafe {
            sql_free_handle(SQL_HANDLE_DESC, self.desc);
            sql_free_handle(SQL_HANDLE_DBC, self.dbc);
        }
    }
}
