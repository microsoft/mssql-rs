// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection::tds_sync_client::TdsSyncClient;
use mssql_tds::core::{TdsResult, Version};
use tokio::runtime::Runtime;

use super::{EnvHandle, HandleType, HasObjectType};
use crate::api::odbc_types::{DEFAULT_PACKET_SIZE, SQL_MODE_READ_WRITE};
use crate::error::{DiagRecord, HasDiagnostics};

/// The connection's TDS client, held in one of two interchangeable edges.
///
/// The async [`TdsClient`] drives every control-plane operation (connect,
/// execute, COLMETADATA, advance, close). Once a row-returning result set is
/// open on an eligible (raw-TCP, plaintext) connection, [`finish_execute`] flips
/// it to the reactor-free [`TdsSyncClient`] so `SQLFetch` pulls rows off a
/// blocking socket with no tokio reactor. Result-set boundaries flip back to
/// `Async` (via [`DbcClient::into_async`]) before running the next control-plane
/// op, so the sync edge is only ever live while a cursor is being fetched.
///
/// TLS / non-raw transports never convert (`SyncConversion::NotEligible`), so
/// they stay on the `Async` edge and fetch through the unchanged `block_on`
/// path — byte-identical to the pre-rewire behaviour.
///
/// [`finish_execute`]: crate::api::exec_common
pub(crate) enum DbcClient {
    /// The async, reactor-driven client. All control-plane work uses this edge.
    Async(TdsClient),
    /// The reactor-free sync fetch client, live only while a cursor is open on
    /// an eligible connection.
    Sync(TdsSyncClient),
}

impl DbcClient {
    /// Coerces to the async [`TdsClient`], reverting a live sync cursor back to
    /// the tokio reactor. A no-op (never fails) when already `Async`.
    ///
    /// The revert re-registers the owned socket with the runtime handle captured
    /// at `into_sync` time — it performs no network I/O and needs no ambient
    /// runtime — so it is safe to call on the bare ODBC thread. Errors only if
    /// the connection was poisoned (missing handle / fd re-registration failure),
    /// in which case the connection is consumed and closed (known-dead).
    pub(crate) fn into_async(self) -> TdsResult<TdsClient> {
        match self {
            DbcClient::Async(client) => Ok(client),
            DbcClient::Sync(sync) => sync.into_async(),
        }
    }
}

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
    /// Held as a [`DbcClient`] so the fetch hot path can run on the reactor-free
    /// sync edge while control-plane work stays async.
    pub(crate) client: Option<DbcClient>,
    /// Server version negotiated at login, cached at connect time. Reported by
    /// `SQLGetInfo(SQL_DBMS_VER)` without touching the live client, so it stays
    /// available even while a sync fetch cursor owns the connection.
    pub(crate) server_version: Option<Version>,
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
            .field(
                "client",
                &self.client.as_ref().map(|c| match c {
                    DbcClient::Async(_) => "Async",
                    DbcClient::Sync(_) => "Sync",
                }),
            )
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<REDACTED>"),
            )
            .field("login_timeout", &self.login_timeout)
            .finish()
    }
}

impl DbcState {
    /// Stores an async client on the connection (idle or busy). The common
    /// restore path: every control-plane op and the async fetch arm return the
    /// client through here, wrapping it in [`DbcClient::Async`].
    pub(crate) fn store_async(&mut self, client: TdsClient) {
        self.client = Some(DbcClient::Async(client));
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
                server_version: None,
                access_token: None,
                login_timeout: None,
                access_mode: SQL_MODE_READ_WRITE,
                connection_timeout: 0,
                packet_size: DEFAULT_PACKET_SIZE,
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
