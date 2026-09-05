// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use mssql_tds::connection::tds_client::TdsClient;

use super::env::SharedRuntime;
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
    pub(crate) runtime: Arc<SharedRuntime>,
    pub(crate) inner: Mutex<DbcState>,
}

// SAFETY: The raw pointer `parent_env` prevents auto-impl of Send/Sync.
// We assert these are safe because `parent_env` is set once at construction
// and never mutated. The parent ENV is guaranteed alive because the DM
// ensures all DBCs are freed before calling SQLFreeEnv.
// All mutable state is Mutex-protected.
unsafe impl Send for DbcHandle {}
unsafe impl Sync for DbcHandle {}

/// Pre-connect values from the `SQL_COPT_SS_*` attributes that duplicate a
/// connection-string keyword.
///
/// mssql-python applies `attrs_before` unfiltered at connect, so a caller can
/// configure the same setting through either path. Measured against msodbcsql
/// 18, the two paths do not rank the same way:
///
/// - The vendor attributes here **override** the keyword. `Encrypt=no` in the
///   string plus `SQL_COPT_SS_ENCRYPT=1` connects encrypted, and the reverse
///   pairing connects unencrypted; `SQL_COPT_SS_TRUST_SERVER_CERTIFICATE=0`
///   overrides `TrustServerCertificate=yes` hard enough to fail the handshake.
/// - `SQL_ATTR_CURRENT_CATALOG` is the opposite: the `Database=` keyword wins
///   and the attribute is only a fallback (see `driver_connect`).
///
/// So this is deliberately not a general "attributes beat keywords" rule -- it
/// is per attribute, and each field here was confirmed by observing the
/// resulting session in `sys.dm_exec_connections` rather than by reading a
/// header.
///
/// Values are normalized on the way in, matching msodbcsql: setting any of
/// these to an out-of-range `7` and reading it back post-connect returns `1`,
/// so the driver stores the effective value rather than the caller's input.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct VendorConnOverrides {
    /// `SQL_COPT_SS_ENCRYPT` (1223): `0` no, `1` yes, `2` strict.
    pub(crate) encrypt: Option<u32>,
    /// `SQL_COPT_SS_TRUST_SERVER_CERTIFICATE` (1228): `0` or `1`.
    pub(crate) trust_server_certificate: Option<u32>,
    /// `SQL_COPT_SS_INTEGRATED_SECURITY` (1203): `0` or `1`.
    pub(crate) integrated_security: Option<u32>,
}

/// Mutable state within a connection handle, protected by `inner`.
pub(crate) struct DbcState {
    pub(crate) diag_records: Vec<DiagRecord>,
    pub(crate) connection_state: ConnectionState,
    /// Active child STMT handles
    pub(crate) statements: Vec<*mut c_void>,
    /// Explicitly-allocated DESC handles (`SQLAllocHandle(SQL_HANDLE_DESC, ...)`),
    /// owned by this connection independent of any one statement. A statement
    /// references one by raw pointer in `StmtState::active_ard`/`active_apd`
    /// once associated (`SQLSetStmtAttrW`); freeing an entry here
    /// (`SQLFreeHandle(SQL_HANDLE_DESC)`) resets every statement referencing
    /// it back to its own implicit descriptor first.
    pub(crate) descriptors: Vec<*mut c_void>,
    /// The STMT handle that currently has an open cursor, if any.
    /// Set when SQLExecDirect succeeds; cleared by SQLCloseCursor /
    /// SQLFreeStmt(SQL_CLOSE). Used to enforce the non-MARS rule that only
    /// one statement may hold an open cursor per connection at a time.
    pub(crate) active_stmt: Option<*mut c_void>,
    /// Active TDS connection, present only when `connection_state == Connected`.
    pub(crate) client: Option<TdsClient>,
    /// Server endpoint from the successful connection string.
    pub(crate) server_name: String,
    /// Login identity sent for the successful connection.
    pub(crate) user_name: String,
    /// Pre-connect access token set via `SQL_COPT_SS_ACCESS_TOKEN`.
    /// Consumed by `SQLDriverConnect` to select `AccessToken` authentication.
    pub(crate) access_token: Option<String>,
    /// Login timeout in seconds set via `SQL_ATTR_LOGIN_TIMEOUT`. Applied to the
    /// TDS login deadline at connect time. `Some(0)` means wait indefinitely.
    pub(crate) login_timeout: Option<u32>,
    /// Pre-connect overrides from the `SQL_COPT_SS_*` attribute forms of
    /// connection-string keywords. `None` means "the caller did not set this
    /// attribute", which is what lets the keyword stand.
    pub(crate) vendor_overrides: VendorConnOverrides,
    /// Effective vendor settings for the current connected session. Kept
    /// separate from [`vendor_overrides`](Self::vendor_overrides) so a keyword
    /// resolved by one connection is never reused as an explicit attribute on
    /// the next connection attempt.
    pub(crate) effective_vendor_settings: Option<VendorConnOverrides>,
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
    /// The server's transaction isolation level is no longer known to match
    /// [`txn_isolation`](Self::txn_isolation).
    ///
    /// Set when a pool reset is armed: SQL Server's connection reset does not
    /// restore the isolation level, and the previous borrower may have changed
    /// it through raw T-SQL that this cache never saw. While set,
    /// `SQL_ATTR_TXN_ISOLATION` must not take its same-value short circuit, or
    /// the checkout SET would be skipped and the next borrower would silently
    /// inherit the previous one's level. Cleared once an isolation SET reaches
    /// the server, or at connect time when the session starts from a known
    /// state.
    ///
    /// This is not reset *acknowledgement* state: `TdsClient` verifies that
    /// itself on the request that carries the RESETCONNECTION bit.
    pub(crate) server_isolation_unknown: bool,
    /// Monotonic count of pool resets armed on this connection.
    ///
    /// `set_txn_isolation` captures it before it sends and only clears
    /// [`server_isolation_unknown`](Self::server_isolation_unknown) afterwards
    /// if the count is unchanged. Without it a checkout SET already in flight
    /// could clear an invalidation armed *after* it reached the server, and the
    /// next same-value SET would short-circuit against a session the newer reset
    /// had made unknown again.
    pub(crate) reset_generation: u64,
    /// The application executed a statement in manual-commit mode, so the open
    /// transaction may hold uncommitted user work. Mirrors msodbcsql's
    /// `CONN_ST_LOCALTRANS_STARTED` (`sqlcprot.h:2298`) and is deliberately
    /// distinct from `TdsClient::has_active_transaction()`, which also reports
    /// driver-begun *piggyback* transactions that carry no user work. Only this
    /// flag blocks `SQLDisconnect` (25000) and `SQL_ATTR_TXN_ISOLATION` (HY011).
    pub(crate) local_tran_started: bool,
    /// `SQL_ATTR_CURRENT_CATALOG`. Before connecting this holds the requested
    /// initial database (the connection string's `Database=` keyword wins if
    /// both are given, matching msodbcsql); afterwards the live name comes from
    /// the TDS client's ENVCHANGE tracking, so this is only the pre-connect
    /// seed and the fallback answer for a disconnected `SQLGetConnectAttr`.
    pub(crate) current_catalog: Option<String>,
    /// Connection-level default for `SQL_ATTR_QUERY_TIMEOUT`, inherited by
    /// statements allocated afterwards.
    ///
    /// `SQLSetConnectAttr` accepts statement options and both fans the value out
    /// to the connection's existing statements and records it here for future
    /// ones (msodbcsql `sqlcmisc.cpp:2879-2922`, `sqlcfunc.cpp:173`).
    pub(crate) stmt_query_timeout: u32,
}

// Manual `Debug` so the bearer access token is never rendered in logs or panic
// messages; presence is shown, the value is redacted (mirrors `ConnectionParams`).
impl std::fmt::Debug for DbcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbcState")
            .field("diag_records", &self.diag_records)
            .field("connection_state", &self.connection_state)
            .field("statements", &self.statements)
            .field("descriptors", &self.descriptors)
            .field("active_stmt", &self.active_stmt)
            .field("client", &self.client)
            .field("server_name", &self.server_name)
            .field("user_name", &self.user_name)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<REDACTED>"),
            )
            .field("login_timeout", &self.login_timeout)
            .field("autocommit", &self.autocommit)
            .field("txn_isolation", &self.txn_isolation)
            .field("local_tran_started", &self.local_tran_started)
            .field("current_catalog", &self.current_catalog)
            .field("stmt_query_timeout", &self.stmt_query_timeout)
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
    pub(crate) fn new(parent_env: *mut c_void, runtime: Arc<SharedRuntime>) -> Self {
        Self {
            object_type: HandleType::Dbc,
            parent_env,
            runtime,
            inner: Mutex::new(DbcState {
                diag_records: Vec::new(),
                connection_state: ConnectionState::Disconnected,
                statements: Vec::new(),
                descriptors: Vec::new(),
                active_stmt: None,
                client: None,
                server_name: String::new(),
                user_name: String::new(),
                access_token: None,
                vendor_overrides: VendorConnOverrides::default(),
                effective_vendor_settings: None,
                login_timeout: None,
                access_mode: SQL_MODE_READ_WRITE,
                connection_timeout: 0,
                packet_size: DEFAULT_PACKET_SIZE,
                autocommit: true,
                txn_isolation: SQL_TXN_READ_COMMITTED,
                local_tran_started: false,
                server_isolation_unknown: false,
                reset_generation: 0,
                current_catalog: None,
                stmt_query_timeout: 0,
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

#[cfg(test)]
mod tests {
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

    use super::*;

    #[test]
    fn debug_redacts_the_token_and_reports_the_attribute_state() {
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut state = dbc.inner.lock().unwrap();
        state.access_token = Some("super-secret-jwt".into());
        state.current_catalog = Some("reporting".into());
        state.stmt_query_timeout = 30;

        let rendered = format!("{:?}", *state);
        assert!(!rendered.contains("super-secret-jwt"));
        assert!(rendered.contains("<REDACTED>"));
        assert!(rendered.contains("current_catalog: Some(\"reporting\")"));
        assert!(rendered.contains("stmt_query_timeout: 30"));
    }
}
