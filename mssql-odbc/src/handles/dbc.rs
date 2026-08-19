// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use mssql_tds::connection::tds_client::TdsClient;
use tokio::runtime::Runtime;

use super::{EnvHandle, HandleType, HasObjectType};
use crate::api::odbc_types::{DEFAULT_PACKET_SIZE, SQL_MODE_READ_WRITE, SQL_TXN_READ_COMMITTED};
use crate::error::{DiagRecord, HasDiagnostics};

/// Connection state machine — tracks whether the DBC is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionState {
    /// Allocated but not connected (C2 in ODBC state table).
    Disconnected,
    /// Connection attempt in progress - blocks concurrent SQLDriverConnect calls.
    Connecting,
    /// Connected to a data source (C4/C5/C6 in ODBC state table).
    Connected,
}

/// Connection handle
///
/// Created by `SQLAllocHandle(SQL_HANDLE_DBC, henv, ...)`.
/// Holds a back-pointer to the parent environment and connection-level state.
///
/// Thread-safety: The `inner` mutex protects mutable state, mirroring
/// msodbcsql's connection-level critical section.
#[derive(Debug)]
pub(crate) struct DbcHandle {
    pub(crate) object_type: HandleType,
    /// Back-pointer to the parent ENV handle. Stored as opaque pointer because
    /// the ENV owns the DBC's lifetime, not the other way around.
    pub(crate) parent_env: *mut c_void,
    /// Shared Tokio runtime from the parent ENV.
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) inner: Mutex<DbcState>,
}

// SAFETY: The raw pointer `parent_env` prevents auto-impl of Send/Sync.
// We assert these are safe because `parent_env` is set once at construction
// and never mutated. The parent ENV is guaranteed alive because the DM
// ensures all DBCs are freed before calling SQLFreeEnv.
// All mutable state is Mutex-protected.
unsafe impl Send for DbcHandle {}
unsafe impl Sync for DbcHandle {}

/// Mutable state within a connection handle, protected by `inner`.
pub(crate) struct DbcState {
    pub(crate) diag_records: Vec<DiagRecord>,
    pub(crate) connection_state: ConnectionState,
    /// Active child STMT handles
    pub(crate) statements: Vec<*mut c_void>,
    /// The STMT handle that currently has an open cursor, if any.
    /// Set when SQLExecDirect succeeds; cleared by SQLCloseCursor /
    /// SQLFreeStmt(SQL_CLOSE). Used to enforce the non-MARS rule that only
    /// one statement may hold an open cursor per connection at a time.
    pub(crate) active_stmt: Option<*mut c_void>,
    /// Active TDS connection, present only when `connection_state == Connected`.
    pub(crate) client: Option<TdsClient>,
    /// Pre-connect access token set via `SQL_COPT_SS_ACCESS_TOKEN`.
    /// Consumed by `SQLDriverConnect` to select `AccessToken` authentication.
    pub(crate) access_token: Option<String>,
    /// Login timeout in seconds set via `SQL_ATTR_LOGIN_TIMEOUT`. Applied to the
    /// TDS login deadline at connect time. `Some(0)` means wait indefinitely.
    pub(crate) login_timeout: Option<u32>,
    /// `SQL_ATTR_ACCESS_MODE`. Stored so a set/get round-trip agrees; the driver
    /// does not yet vary its behaviour on it.
    pub(crate) access_mode: u32,
    /// `SQL_ATTR_CONNECTION_TIMEOUT` in seconds. Stored, not yet honored.
    /// `0` is the ODBC default and means "no timeout".
    pub(crate) connection_timeout: u32,
    /// `SQL_ATTR_PACKET_SIZE` in bytes. Stored, not yet honored.
    pub(crate) packet_size: u32,
    /// `SQL_ATTR_AUTOCOMMIT`. `true` is the ODBC-mandated default
    /// (msodbcsql `SQL_AUTOCOMMIT_DEFAULT`); `false` selects manual-commit, in
    /// which the driver keeps a transaction open until `SQLEndTran`.
    pub(crate) autocommit: bool,
    /// `SQL_ATTR_TXN_ISOLATION`, one of the `SQL_TXN_*` bits. Cached client-side
    /// and read back without a server round trip, matching msodbcsql
    /// (`sqlcmisc.cpp:3426`). Applied as a `SET TRANSACTION ISOLATION LEVEL`
    /// batch when connected, otherwise deferred to connect time.
    pub(crate) txn_isolation: u32,
    /// The application executed a statement in manual-commit mode, so the open
    /// transaction may hold uncommitted user work. Mirrors msodbcsql's
    /// `CONN_ST_LOCALTRANS_STARTED` (`sqlcprot.h:2298`) and is deliberately
    /// distinct from `TdsClient::has_active_transaction()`, which also reports
    /// driver-begun *piggyback* transactions that carry no user work. Only this
    /// flag blocks `SQLDisconnect` (25000) and `SQL_ATTR_TXN_ISOLATION` (HY011).
    /// Set when a pooling reset is armed and cleared once the request that
    /// carries the RESETCONNECTION bit has been sent. While true,
    /// `SQL_ATTR_TXN_ISOLATION` must not take its same-value short circuit: that
    /// checkout `SET TRANSACTION ISOLATION LEVEL` is the request the armed bit
    /// rides, so short-circuiting it would defer the reset to the borrower's
    /// first query and lose fail-at-checkout.
    pub(crate) pending_reset_ack: bool,
    pub(crate) local_tran_started: bool,
}

// Manual `Debug` so the bearer access token is never rendered in logs or panic
// messages; presence is shown, the value is redacted (mirrors `ConnectionParams`).
impl std::fmt::Debug for DbcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbcState")
            .field("diag_records", &self.diag_records)
            .field("connection_state", &self.connection_state)
            .field("statements", &self.statements)
            .field("active_stmt", &self.active_stmt)
            .field("client", &self.client)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<REDACTED>"),
            )
            .field("login_timeout", &self.login_timeout)
            .field("autocommit", &self.autocommit)
            .field("txn_isolation", &self.txn_isolation)
            .field("local_tran_started", &self.local_tran_started)
            .finish()
    }
}

impl HasDiagnostics for DbcState {
    fn diag_records(&self) -> &[DiagRecord] {
        &self.diag_records
    }
    fn diag_records_mut(&mut self) -> &mut Vec<DiagRecord> {
        &mut self.diag_records
    }
}

impl DbcHandle {
    pub(crate) fn new(parent_env: *mut c_void, runtime: Arc<Runtime>) -> Self {
        Self {
            object_type: HandleType::Dbc,
            parent_env,
            runtime,
            inner: Mutex::new(DbcState {
                diag_records: Vec::new(),
                connection_state: ConnectionState::Disconnected,
                statements: Vec::new(),
                active_stmt: None,
                client: None,
                access_token: None,
                login_timeout: None,
                access_mode: SQL_MODE_READ_WRITE,
                connection_timeout: 0,
                packet_size: DEFAULT_PACKET_SIZE,
                autocommit: true,
                txn_isolation: SQL_TXN_READ_COMMITTED,
                local_tran_started: false,
                pending_reset_ack: false,
            }),
        }
    }

    /// Returns a reference to the parent ENV handle.
    ///
    /// The returned reference is bound to `&self` so it cannot outlive this
    /// connection, and the parent ENV is guaranteed alive for at least that
    /// long because the DM frees all DBC handles before freeing their parent
    /// ENV.
    pub(crate) fn parent_env(&self) -> &EnvHandle {
        // SAFETY: `parent_env` is set at construction to a live `EnvHandle`
        // pointer (allocated by `handle_to_raw::<EnvHandle>`), is never mutated,
        // and the ENV outlives this DBC per the DM contract.
        unsafe { &*(self.parent_env as *const EnvHandle) }
    }
}

impl HasObjectType for DbcHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}
