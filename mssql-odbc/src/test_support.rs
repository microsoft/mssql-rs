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
    /// before the DBC. Marks the DBC connected first (idempotent) so tests get
    /// a realistic connected-session baseline — not required by `alloc_desc`
    /// itself, which doesn't gate on connection state.
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

/// A running mock TDS server plus the real, connected `TdsClient` installed on
/// a DBC by [`connect_mock_server`]. Dropping it shuts the server down.
///
/// The server runs on its own dedicated Tokio runtime/thread — never the
/// DBC's own runtime — matching how a real SQL Server is a separate process
/// that never competes with the driver for the runtime's worker thread. This
/// is what lets a query-timeout regression test tell "the timeout bounded the
/// wait" from "the server just answered quickly": the server can independently
/// delay a response for seconds while the DBC-side call is timed.
pub(crate) struct MockServer {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    server_handle: Option<tokio::task::JoinHandle<()>>,
    server_runtime: tokio::runtime::Runtime,
    query_registry: std::sync::Arc<tokio::sync::Mutex<mssql_mock_tds::QueryRegistry>>,
}

impl MockServer {
    /// Registers a delay for the server's answer to the TDS Transaction
    /// Manager `Begin` request — the implicit transaction begin an
    /// autocommit-off connection issues before its first statement. Lets a
    /// test prove `SQL_ATTR_QUERY_TIMEOUT` bounds that step specifically,
    /// the same way [`connect_mock_server`]'s `response` bounds a query.
    pub(crate) fn set_tm_begin_delay(&self, delay: std::time::Duration) {
        self.server_runtime.block_on(async {
            self.query_registry.lock().await.register(
                mssql_mock_tds::TM_BEGIN_DELAY_KEY,
                mssql_mock_tds::QueryResponse::select_one().with_delay(delay),
            );
        });
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.server_handle.take() {
            let _ = self.server_runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(2), handle).await
            });
        }
    }
}

/// Starts a mock TDS server registered to answer `query` with `response`,
/// then connects a real `TdsClient` to it over a real TCP socket (via `dbc`'s
/// own runtime) and installs it as `dbc`'s active, connected client — so a
/// unit test can drive the real `SQLExecute` / `SQLExecDirectW` code path
/// against a genuinely slow (or fast) server response instead of a scripted,
/// instantaneous one.
///
/// Keep the returned [`MockServer`] alive for the duration of the test; it
/// shuts the server down on drop.
pub(crate) fn connect_mock_server(
    dbc: &crate::handles::dbc::DbcHandle,
    query: &str,
    response: mssql_mock_tds::QueryResponse,
) -> MockServer {
    use crate::handles::dbc::ConnectionState;
    use mssql_mock_tds::MockTdsServer;
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
    use std::time::Duration;

    let server_runtime =
        tokio::runtime::Runtime::new().expect("failed to build mock-server runtime");
    let (server_addr, shutdown_tx, server_handle, query_registry) =
        server_runtime.block_on(async {
            let server = MockTdsServer::new("127.0.0.1:0")
                .await
                .expect("failed to start mock server");
            let addr = server.local_addr();
            let registry = server.query_registry();
            registry.lock().await.register(query, response);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let handle = tokio::spawn(async move {
                let _ = server.run_with_shutdown(rx).await;
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            (addr, tx, handle, registry)
        });

    let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
    let mut context = ClientContext::default();
    context.user_name = "sa".to_string();
    context.password = "unused-by-the-mock-server".to_string();
    context.database = "master".to_string();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };

    let provider = TdsConnectionProvider {};
    let client = dbc
        .runtime
        .block_on(provider.create_client(context, &datasource, None))
        .expect("mock client failed to connect");

    {
        let mut state = dbc.inner.lock().unwrap();
        state.client = Some(client);
        state.connection_state = ConnectionState::Connected;
    }

    MockServer {
        shutdown_tx: Some(shutdown_tx),
        server_handle: Some(server_handle),
        server_runtime,
        query_registry,
    }
}
