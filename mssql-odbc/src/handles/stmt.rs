// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::query::metadata::ColumnMetadata;

use super::desc::{DescHandle, DescKind};
use super::{DbcHandle, HandleType, HasObjectType, free_handle, handle_to_raw};
use crate::api::odbc_types::{SqlULen, SqlUSmallInt};
use crate::error::{DiagRecord, HasDiagnostics};
use crate::params::BoundParam;

pub(crate) const STMT_STATE_EXEC_STARTED: u32 = 0x0000_0100;
pub(crate) const STMT_STATE_PREPARED: u32 = 0x0000_0200;
pub(crate) const STMT_STATE_CURSOR_OPEN: u32 = 0x0000_0800;
pub(crate) const STMT_STATE_EXEC_CONTEXT: u32 = 0x0000_1000;

/// Statement handle
///
/// Created by `SQLAllocHandle(SQL_HANDLE_STMT, hdbc, ...)`.
#[derive(Debug)]
pub(crate) struct StmtHandle {
    pub(crate) object_type: HandleType,
    /// Back-pointer to the parent DBC handle. Stored as opaque pointer because
    /// the DBC owns the STMT's lifetime, not the other way around.
    /// Mirrors msodbcsql's statement→connection back-pointer.
    pub(crate) parent_dbc: *mut c_void,
    /// The four automatically-allocated implicit descriptors (ARD/APD/IRD/IPD),
    /// the permanent implicit allocations (cf. msodbcsql's embedded `lpstmt->ARD`
    /// / `cmdp.APD`, `sqlcfunc.cpp`). Set once in `new()`, freed in `Drop`, never
    /// reassigned — hence sound as plain fields outside `inner`, same set-once
    /// rationale as `parent_dbc`. Do NOT repurpose them into the mutable *active*
    /// ARD/APD association that `SQLSetStmtAttr(SQL_ATTR_APP_ROW_DESC / APP_PARAM_DESC)`
    /// swaps (msodbcsql's separate `pARD`/`pAPD`); that path is still a stub, and
    /// when implemented its active pointer belongs in `StmtState` behind `inner`
    /// (concurrent set/get would otherwise race). IRD/IPD are never swappable.
    pub(crate) ard: *mut c_void,
    pub(crate) apd: *mut c_void,
    pub(crate) ird: *mut c_void,
    pub(crate) ipd: *mut c_void,
    pub(crate) inner: Mutex<StmtState>,
}

/// Mutable state within a statement handle, protected by `inner`.
#[derive(Debug)]
pub(crate) struct StmtState {
    pub(crate) diag_records: Vec<DiagRecord>,
    /// Column metadata from the most recent execution.
    pub(crate) column_metadata: Vec<ColumnMetadata>,
    /// SQL text stored by `SQLPrepare`, awaiting execution. The server-side
    /// prepare is deferred to `SQLExecute`.
    pub(crate) prepared_sql: Option<String>,
    /// Parameters bound via `SQLBindParameter`, indexed by `(ParameterNumber
    /// - 1)`. `None` slots are gaps left by binding a higher ordinal first.
    pub(crate) bound_params: Vec<Option<BoundParam>>,
    /// Server-side prepared-statement handle from `sp_prepare`, cached so
    /// subsequent `SQLExecute` calls reuse it via `sp_execute`. `None`
    /// until the first execute prepares it.
    pub(crate) prepared_handle: Option<i32>,
    /// A prepared handle orphaned by a re-prepare / rebind / `SQLExecDirect`
    /// that must be released with `sp_unprepare`. The drop is deferred to the
    /// next point that already holds the TDS client (execute / exec-direct) or
    /// to statement free, so bind/prepare stay I/O-free — mirroring msodbcsql's
    /// deferred `hPrepDropDeferred`. Invariant: this is `None` whenever
    /// `prepared_handle` is `Some` (a new handle can only be acquired by an
    /// execute, which flushes any pending drop first).
    pub(crate) pending_unprepare: Option<i32>,
    /// Current fetched row, populated by SQLFetch for later SQLGetData support.
    /// The sync fetch arm recycles this allocation into its row writer.
    pub(crate) current_row: Option<Vec<ColumnValues>>,
    /// Rows affected by the last execution, reported by `SQLRowCount`. `-1`
    /// means "not available" (no statement executed yet, a result-returning
    /// SELECT, DDL, or `SET NOCOUNT ON`) — matching msodbcsql's
    /// `SQL_NO_ROWCOUNT_TOTAL` default.
    pub(crate) row_count: i64,
    /// Remaining per-statement row counts from a pure-DML batch
    /// (`UPDATE; DELETE; INSERT`). `finish_execute` reports the first via
    /// `row_count` and queues the rest here; each `SQLMoreResults` pops the next
    /// (in memory — no cursor or connection), mirroring msodbcsql's one
    /// result set per DML statement.
    pub(crate) pending_row_counts: VecDeque<i64>,
    /// Rowset size for block fetches (`SQL_ATTR_ROW_ARRAY_SIZE`). Defaults to 1
    /// (single-row). Consumed by the columnar `SQLFetchScroll` path.
    pub(crate) row_array_size: SqlULen,
    /// Application buffer that receives the count of rows fetched by a block
    /// fetch (`SQL_ATTR_ROWS_FETCHED_PTR`); null when unset. The application
    /// owns this buffer and must keep it valid across the fetch.
    pub(crate) rows_fetched_ptr: *mut SqlULen,
    /// Application array that receives per-row status codes
    /// (`SQL_ATTR_ROW_STATUS_PTR`); null when unset.
    pub(crate) row_status_ptr: *mut SqlUSmallInt,
    /// Row binding orientation (`SQL_ATTR_ROW_BIND_TYPE`): `SQL_BIND_BY_COLUMN`
    /// (0) for column-wise arrays, otherwise a row-struct byte size.
    pub(crate) row_bind_type: SqlULen,
    /// Statement lifecycle/status flags used for ODBC API state checks.
    pub(crate) state_flags: u32,
    /// Active `SQLGetData` streaming cursor for the current row. Tracks how much
    /// of a single column's value has already been handed back across successive
    /// `SQLGetData` calls so large (LOB / `max`) values stream in buffer-sized
    /// chunks that terminate in `SQL_NO_DATA`, instead of re-returning the same
    /// truncated prefix every call. Reset at each `SQLFetch` and cursor close.
    pub(crate) getdata: Option<GetDataProgress>,
}

/// Encoded, ready-to-serve units for an in-progress `SQLGetData` stream. The
/// column value is converted and encoded once when a column becomes active, then
/// served in chunks — keeping chunked retrieval O(n) total rather than
/// re-encoding the whole value on every call.
#[derive(Debug)]
pub(crate) enum GetDataUnits {
    /// `SQL_C_CHAR` payload (UTF-8 bytes).
    Char(Vec<u8>),
    /// `SQL_C_WCHAR` payload (UTF-16 code units).
    WChar(Vec<u16>),
    /// SQL `NULL`: one `SQL_NULL_DATA` delivery, then `SQL_NO_DATA`.
    Null,
}

impl GetDataUnits {
    /// True when this payload matches the requested C type, so an in-progress
    /// stream can continue instead of being rebuilt. `NULL` matches either.
    pub(crate) fn matches_wchar(&self, is_wchar: bool) -> bool {
        match self {
            GetDataUnits::Char(_) => !is_wchar,
            GetDataUnits::WChar(_) => is_wchar,
            GetDataUnits::Null => true,
        }
    }
}

/// Per-column streaming progress for `SQLGetData`.
#[derive(Debug)]
pub(crate) struct GetDataProgress {
    /// 1-based column number the cursor is bound to.
    pub(crate) column: SqlUSmallInt,
    /// Cached encoded payload.
    pub(crate) units: GetDataUnits,
    /// Number of units already delivered.
    pub(crate) offset: usize,
    /// Set once the terminal chunk (or the sole NULL/empty delivery) has been
    /// served; the next call on this column returns `SQL_NO_DATA`.
    pub(crate) exhausted: bool,
}

impl StmtState {
    pub(crate) fn has_state(&self, mask: u32) -> bool {
        (self.state_flags & mask) != 0
    }

    pub(crate) fn set_state(&mut self, mask: u32) {
        self.state_flags |= mask;
    }

    pub(crate) fn clear_state(&mut self, mask: u32) {
        self.state_flags &= !mask;
    }

    /// Clears the current row at every result-set boundary (execute,
    /// `SQLMoreResults`, cursor close) so a fresh result set never serves a row
    /// left over from a prior one.
    pub(crate) fn reset_fetch_state(&mut self) {
        self.current_row = None;
        self.getdata = None;
    }

    /// Moves the cached `prepared_handle` (if any) into `pending_unprepare` so
    /// the next execute / exec-direct (or statement free) releases it with
    /// `sp_unprepare`. Called by re-prepare, rebind, and `SQLExecDirect` when
    /// the current prepared plan is superseded. No network I/O.
    pub(crate) fn orphan_prepared_handle(&mut self) {
        if let Some(handle) = self.prepared_handle.take() {
            debug_assert!(
                self.pending_unprepare.is_none(),
                "orphan_prepared_handle: a pending unprepare already exists"
            );
            self.pending_unprepare = Some(handle);
        }
    }
}

impl HasDiagnostics for StmtState {
    fn diag_records(&self) -> &[DiagRecord] {
        &self.diag_records
    }
    fn diag_records_mut(&mut self) -> &mut Vec<DiagRecord> {
        &mut self.diag_records
    }
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
            ard: handle_to_raw(Box::new(DescHandle::new(DescKind::AppRow))),
            apd: handle_to_raw(Box::new(DescHandle::new(DescKind::AppParam))),
            ird: handle_to_raw(Box::new(DescHandle::new(DescKind::ImpRow))),
            ipd: handle_to_raw(Box::new(DescHandle::new(DescKind::ImpParam))),
            inner: Mutex::new(StmtState {
                diag_records: Vec::new(),
                column_metadata: Vec::new(),
                prepared_sql: None,
                bound_params: Vec::new(),
                prepared_handle: None,
                pending_unprepare: None,
                current_row: None,
                row_count: -1,
                pending_row_counts: VecDeque::new(),
                row_array_size: 1,
                rows_fetched_ptr: std::ptr::null_mut(),
                row_status_ptr: std::ptr::null_mut(),
                row_bind_type: crate::api::odbc_types::SQL_BIND_BY_COLUMN,
                state_flags: 0,
                getdata: None,
            }),
        }
    }

    /// Returns a reference to the parent DBC handle.
    ///
    /// The returned reference is bound to `&self` so it cannot outlive this
    /// statement handle, and the parent DBC is guaranteed alive for at least
    /// that long because the DM frees all STMT handles before freeing their
    /// parent DBC.
    pub(crate) fn parent_dbc(&self) -> &DbcHandle {
        // SAFETY: `parent_dbc` is set at construction to a live `DbcHandle`
        // pointer (allocated by `handle_to_raw::<DbcHandle>`), is never
        // mutated, and the DBC outlives this STMT per the DM contract.
        unsafe { &*(self.parent_dbc as *const DbcHandle) }
    }
}

impl HasObjectType for StmtHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}

impl Drop for StmtHandle {
    fn drop(&mut self) {
        // Free the four implicit descriptors owned by this statement through the
        // centralized deallocation path so each one's object type is stamped
        // `Invalid` (use-after-free detection) rather than raw `Box::from_raw`.
        // These are never handed to `SQLFreeHandle` (they are implicit), so
        // dropping the statement is the single owner responsible for them.
        for raw in [self.ard, self.apd, self.ird, self.ipd] {
            unsafe { free_handle::<DescHandle>(raw) };
        }
    }
}
