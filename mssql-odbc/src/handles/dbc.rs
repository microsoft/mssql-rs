// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::Mutex;

use super::{DiagRecord, HandleType, HasObjectType};

/// Connection state machine — tracks whether the DBC is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ConnectionState {
    /// Allocated but not connected (C2 in ODBC state table).
    Disconnected,
    /// Connected to a data source (C4/C5/C6 in ODBC state table).
    Connected,
}

/// Connection handle — Rust port of msodbcsql's `struct tagDBC : tagOBJBASE`.
///
/// Created by `SQLAllocHandle(SQL_HANDLE_DBC, henv, ...)`. Holds a back-pointer
/// to the parent ENV and connection-level state. Field layout mirrors `tagDBC`:
/// inherited `tagOBJBASE.ObjectType` first (lock-free), then the lock
/// (`inner` ≈ `csDbc`) covering inherited `errinfo` plus derived fields.
#[derive(Debug)]
pub(crate) struct DbcHandle {
    pub(crate) object_type: HandleType,
    /// Back-pointer to the parent ENV handle. Stored as opaque pointer because
    /// the ENV owns the DBC's lifetime, not the other way around.
    /// Mirrors msodbcsql's `lpdbc->lpenv`.
    #[allow(dead_code)]
    pub(crate) parent_env: *mut c_void,
    pub(crate) inner: Mutex<DbcState>,
}

// SAFETY: The raw pointer `parent_env` prevents auto-impl of Send/Sync.
// We assert these are safe because `parent_env` is set once at construction
// and never mutated. The parent ENV is guaranteed alive because the DM
// ensures all DBCs are freed before calling SQLFreeEnv.
// All mutable state is Mutex-protected.
unsafe impl Send for DbcHandle {}
unsafe impl Sync for DbcHandle {}

/// Fields of `tagDBC` protected by `csDbc`. Layout mirrors C++ inheritance:
/// inherited `tagOBJBASE` fields first, then derived `tagDBC` fields.
#[derive(Debug)]
pub(crate) struct DbcState {
    /// Inherited from `tagOBJBASE.errinfo` — see `EnvState::diag_records`.
    pub(crate) diag_records: Vec<DiagRecord>,
    // ---- derived tagDBC fields below ----
    #[allow(dead_code)]
    pub(crate) connection_state: ConnectionState,
    /// Active child STMT handles, mirroring msodbcsql's `lppllpstmt`.
    pub(crate) statements: Vec<*mut c_void>,
    // TODO: connection string, login timeout, query timeout, autocommit mode, transaction state,
    // server name, DB name, etc. — all mutable state that would be protected by the mutex.
}

impl DbcHandle {
    pub(crate) fn new(parent_env: *mut c_void) -> Self {
        Self {
            object_type: HandleType::Dbc,
            parent_env,
            inner: Mutex::new(DbcState {
                diag_records: Vec::new(),
                connection_state: ConnectionState::Disconnected,
                statements: Vec::new(),
            }),
        }
    }
}

impl HasObjectType for DbcHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}
