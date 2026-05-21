// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;

use super::{HandleHeader, HandleType, HasHeader};

/// Statement handle — equivalent to msodbcsql's `struct tagSTMT`.
///
/// Created by `SQLAllocHandle(SQL_HANDLE_STMT, hdbc, ...)`.
#[derive(Debug)]
pub(crate) struct StmtHandle {
    pub(crate) header: HandleHeader,
    /// Back-pointer to the parent DBC handle. Stored as opaque pointer because
    /// the DBC owns the STMT's lifetime, not the other way around.
    /// Mirrors msodbcsql's `lpstmt->lpdbc`.
    pub(crate) parent_dbc: *mut c_void,
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
            header: HandleHeader {
                object_type: HandleType::Stmt,
            },
            parent_dbc,
        }
    }
}

impl HasHeader for StmtHandle {
    fn header_mut(&mut self) -> &mut HandleHeader {
        &mut self.header
    }
}
