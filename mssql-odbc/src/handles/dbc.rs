// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use mssql_tds::connection::tds_client::TdsClient;
use tokio::runtime::Runtime;

use super::{HandleType, HasObjectType};
use crate::error::DiagRecord;

/// Connection state machine — tracks whether the DBC is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ConnectionState {
    /// Allocated but not connected (C2 in ODBC state table).
    Disconnected,
    /// Connection attempt in progress - blocks concurrent SQLDriverConnect calls.
    Connecting,
    /// Connected to a data source (C4/C5/C6 in ODBC state table).
    Connected,
}

/// Connection handle — equivalent to msodbcsql's `struct tagDBC`.
///
/// Created by `SQLAllocHandle(SQL_HANDLE_DBC, henv, ...)`.
/// Holds a back-pointer to the parent environment and connection-level state.
///
/// Thread-safety: The `inner` mutex protects mutable state, mirroring
/// msodbcsql's `csDbc` critical section.
#[derive(Debug)]
pub(crate) struct DbcHandle {
    pub(crate) object_type: HandleType,
    /// Back-pointer to the parent ENV handle. Stored as opaque pointer because
    /// the ENV owns the DBC's lifetime, not the other way around.
    /// Mirrors msodbcsql's `lpdbc->lpenv`.
    #[allow(dead_code)]
    pub(crate) parent_env: *mut c_void,
    /// Shared Tokio runtime from the parent ENV.
    pub(crate) runtime: Arc<Runtime>,
    #[allow(dead_code)]
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
#[derive(Debug)]
pub(crate) struct DbcState {
    pub(crate) diag_records: Vec<DiagRecord>,
    // ---- derived tagDBC fields below ----
    #[allow(dead_code)]
    pub(crate) connection_state: ConnectionState,
    /// Active child STMT handles, mirroring msodbcsql's `lppllpstmt`.
    pub(crate) statements: Vec<*mut c_void>,
    /// Active TDS connection, present only when `connection_state == Connected`.
    pub(crate) client: Option<TdsClient>,
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
                client: None,
            }),
        }
    }
}

impl HasObjectType for DbcHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}
