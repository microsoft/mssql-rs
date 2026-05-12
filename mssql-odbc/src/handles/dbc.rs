// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::Mutex;

use super::{HandleHeader, HandleType, HasHeader};

/// Connection state machine — tracks whether the DBC is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ConnectionState {
    /// Allocated but not connected (C2 in ODBC state table).
    Disconnected,
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
    pub(crate) header: HandleHeader,
    /// Back-pointer to the parent ENV handle. Stored as opaque pointer because
    /// the ENV owns the DBC's lifetime, not the other way around.
    /// Mirrors msodbcsql's `lpdbc->lpenv`.
    #[allow(dead_code)]
    pub(crate) parent_env: *mut c_void,
    #[allow(dead_code)]
    pub(crate) inner: Mutex<DbcState>,
}

// SAFETY: The raw pointer `parent_env` prevents auto-impl of Send/Sync.
// We assert these are safe because `parent_env` is set once at construction
// and never mutated. It is dereferenced in `free_dbc` where the parent ENV
// is guaranteed alive (ENV refuses to free while outstanding DBCs exist).
// All mutable state is Mutex-protected.
unsafe impl Send for DbcHandle {}
unsafe impl Sync for DbcHandle {}

/// Mutable state within a connection handle, protected by the mutex.
#[derive(Debug)]
pub(crate) struct DbcState {
    #[allow(dead_code)]
    pub(crate) connection_state: ConnectionState,
    // TODO: connection string, login timeout (default 15s), query timeout,
    // authentication method, server name, database name, etc.
    // These will be populated by SQLDriverConnect/SQLConnect/SQLSetConnectAttr.
}

impl DbcHandle {
    pub(crate) fn new(parent_env: *mut c_void) -> Self {
        Self {
            header: HandleHeader {
                object_type: HandleType::Dbc,
            },
            parent_env,
            inner: Mutex::new(DbcState {
                connection_state: ConnectionState::Disconnected,
            }),
        }
    }
}

impl HasHeader for DbcHandle {
    fn header_mut(&mut self) -> &mut HandleHeader {
        &mut self.header
    }
}
