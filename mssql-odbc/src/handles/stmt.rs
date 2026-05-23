// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::sync::Mutex;

use super::{DiagRecord, HandleType, HasObjectType};

/// Statement handle — Rust port of msodbcsql's `struct tagSTMT : tagOBJBASE`.
///
/// Created by `SQLAllocHandle(SQL_HANDLE_STMT, hdbc, ...)`. Field layout mirrors
/// `tagSTMT`: inherited `ObjectType` first (lock-free), then the lock
/// (`inner` ≈ `csStmt`) covering inherited `errinfo` plus derived fields.
#[derive(Debug)]
pub(crate) struct StmtHandle {
    pub(crate) object_type: HandleType,
    /// Back-pointer to the parent DBC handle. Stored as opaque pointer because
    /// the DBC owns the STMT's lifetime, not the other way around.
    /// Mirrors msodbcsql's `lpstmt->lpdbc`.
    pub(crate) parent_dbc: *mut c_void,
    pub(crate) inner: Mutex<StmtState>,
}

/// Fields of `tagSTMT` protected by `csStmt`. Layout mirrors C++ inheritance:
/// inherited `tagOBJBASE` fields first, then derived `tagSTMT` fields.
#[derive(Debug)]
pub(crate) struct StmtState {
    /// Inherited from `tagOBJBASE.errinfo` — see `EnvState::diag_records`.
    pub(crate) diag_records: Vec<DiagRecord>,
    // ---- derived tagSTMT fields below ----
    // TODO: statement attributes (cursor type, concurrency, etc.) and execution state
}

// SAFETY: The raw pointer `parent_dbc` prevents auto-impl of Send/Sync.
// `parent_dbc` is set once at construction and never mutated. The parent DBC
// is guaranteed alive because the DM ensures all STMTs are freed before
// calling SQLFreeConnect on the parent DBC.
unsafe impl Send for StmtHandle {}
unsafe impl Sync for StmtHandle {}

impl StmtHandle {
    pub(crate) fn new(parent_dbc: *mut c_void) -> Self {
        Self {
            object_type: HandleType::Stmt,
            parent_dbc,
            inner: Mutex::new(StmtState {
                diag_records: Vec::new(),
            }),
        }
    }
}

impl HasObjectType for StmtHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}
