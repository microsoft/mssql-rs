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

/// Default sync-edge prefetch batch size seeded into [`StmtState::max_rows`].
/// `1` keeps each sync `SQLFetch` byte-identical to the async `next_row` it
/// replaces (no prefetch INFO/SWI timing shift) — the clean "reactor removed"
/// datapoint. The buffer machinery is batch-ready: raising the per-statement
/// `max_rows` amortizes syscalls across a batch with no other code impact.
pub(crate) const SYNC_FETCH_DEFAULT_MAX_ROWS: usize = 1;

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
    pub(crate) current_row: Option<Vec<ColumnValues>>,
    /// Sync-edge prefetch buffer: rows pulled by `TdsSyncClient::fetch_rows_batch`
    /// awaiting service to `current_row`, one per `SQLFetch`. Always empty on the
    /// async fetch path (which pulls a single row at a time via `block_on`).
    pub(crate) row_batch: VecDeque<Vec<ColumnValues>>,
    /// Recycled row allocations handed back to `fetch_rows_batch` as its spare
    /// pool, amortizing per-row `Vec` allocation across batch refills.
    pub(crate) spare_rows: Vec<Vec<ColumnValues>>,
    /// A fetch error captured mid-batch on the sync edge, deferred until every
    /// row `fetch_rows_batch` buffered *before* it has been served. At
    /// `max_rows > 1` a refill can return rows **and** an error in one call
    /// (`fetch_rows_batch` pushes rows into `out` as it reads, then propagates
    /// the failing row's error); serving the buffered rows first, then surfacing
    /// the error on the following `SQLFetch`, reproduces the async arm's
    /// row-then-error ordering byte-identically. Always `None` at `max_rows == 1`
    /// (a failing refill fetches zero rows, so the error surfaces immediately).
    pub(crate) pending_fetch_error: Option<mssql_tds::error::Error>,
    /// Rows pulled per `TdsSyncClient::fetch_rows_batch` refill on the sync fetch
    /// edge. Defaults to [`SYNC_FETCH_DEFAULT_MAX_ROWS`] (`1`) so each `SQLFetch`
    /// on the sync path is byte-identical to the async `next_row` it replaces
    /// (INFO/SWI surfaced per row, no prefetch timing shift). The buffer
    /// machinery is batch-ready: raising this amortizes syscalls across a batch
    /// (INFO for rows 2..N surfaces on the refill that fetched them — ODBC-legal).
    pub(crate) max_rows: usize,
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

    /// Clears the current row and the sync-edge prefetch buffer. Called at every
    /// result-set boundary (execute, `SQLMoreResults`, cursor close) so a fresh
    /// result set never serves rows buffered from a prior one. Within a single
    /// result set the sync fetch arm recycles allocations through `spare_rows`;
    /// across boundaries they are dropped to keep the pool bounded.
    pub(crate) fn reset_fetch_state(&mut self) {
        self.current_row = None;
        self.row_batch.clear();
        self.spare_rows.clear();
        self.pending_fetch_error = None;
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
                row_batch: VecDeque::new(),
                spare_rows: Vec::new(),
                pending_fetch_error: None,
                max_rows: SYNC_FETCH_DEFAULT_MAX_ROWS,
                row_count: -1,
                pending_row_counts: VecDeque::new(),
                row_array_size: 1,
                rows_fetched_ptr: std::ptr::null_mut(),
                row_status_ptr: std::ptr::null_mut(),
                row_bind_type: crate::api::odbc_types::SQL_BIND_BY_COLUMN,
                state_flags: 0,
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
