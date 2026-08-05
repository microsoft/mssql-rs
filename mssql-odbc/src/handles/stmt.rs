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
    pub(crate) current_row: Option<Vec<ColumnValues>>,
    /// Rows of the open result set that were read off the wire ahead of time so
    /// the connection could be handed to another statement. `SQLFetch` drains
    /// this before touching the connection. Mirrors msodbcsql, which serves a
    /// second statement as soon as the first result set is fully buffered.
    pub(crate) buffered_rows: VecDeque<Vec<ColumnValues>>,
    /// `true` once `buffered_rows` holds the complete remainder of the open
    /// result set, so an empty buffer means `SQL_NO_DATA` rather than a read.
    pub(crate) buffered_eof: bool,
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
    /// Result columns bound via `SQLBindCol`, indexed by `(ColumnNumber - 1)`.
    /// `None` slots are gaps left by binding a higher ordinal first, or columns
    /// explicitly unbound with a null buffer pointer.
    pub(crate) bound_cols: Vec<Option<BoundCol>>,
    /// Column currently being streamed by `SQLGetData`.
    pub(crate) getdata_col: Option<usize>,
    /// Units of that column already delivered.
    pub(crate) getdata_offset: usize,
    /// Whether the streamed column has been fully delivered.
    pub(crate) getdata_done: bool,
    /// Number of parameter-array elements per execution (`SQL_ATTR_PARAMSET_SIZE`).
    pub(crate) paramset_size: SqlULen,
    /// In-flight data-at-execution state, present between the `SQL_NEED_DATA`
    /// return from `SQLExecute`/`SQLExecDirect` and the final `SQLParamData`
    /// that actually runs the statement.
    pub(crate) dae: Option<DaeState>,
    /// Statement lifecycle/status flags used for ODBC API state checks.
    pub(crate) state_flags: u32,
}

/// Deferred execution state for data-at-execution (DAE) parameters.
///
/// ODBC streams oversized parameter values *after* `SQLExecute` returns
/// `SQL_NEED_DATA`: the application loops `SQLParamData` (which names the next
/// hungry parameter) and `SQLPutData` (which feeds it) until every DAE
/// parameter is satisfied, at which point `SQLParamData` performs the real
/// execution. The statement text and prepared-handle bookkeeping captured at
/// staging time are parked here for that final call.
#[derive(Debug)]
pub(crate) struct DaeState {
    /// Rewritten SQL (`?` markers replaced with `@P<n>`).
    pub(crate) rewritten_sql: String,
    /// Number of parameter markers in the statement.
    pub(crate) marker_count: usize,
    /// Cached server-side prepared handle, if the statement was already
    /// prepared.
    pub(crate) handle: Option<i32>,
    /// A superseded prepared handle to drop on the server during execution.
    pub(crate) drop_handle: Option<i32>,
    /// Zero-based indexes of parameters awaiting data, in ODBC order.
    pub(crate) order: Vec<usize>,
    /// Position within `order` of the next parameter to hand out.
    pub(crate) next: usize,
    /// Parameter currently being fed by `SQLPutData`.
    pub(crate) current: Option<usize>,
    /// Accumulated bytes per parameter index (`None` = the application sent
    /// `SQL_NULL_DATA`).
    pub(crate) data: Vec<Option<Vec<u8>>>,
    /// Entry point that started the DAE sequence, for diagnostics.
    pub(crate) op: &'static str,
}

/// A result column bound to an application buffer by `SQLBindCol`.
///
/// For block fetches the buffers are arrays of `row_array_size` elements, each
/// `buffer_length` bytes wide (column-wise binding).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BoundCol {
    pub(crate) target_type: crate::api::odbc_types::SqlSmallInt,
    pub(crate) target_value_ptr: *mut c_void,
    pub(crate) buffer_length: crate::api::odbc_types::SqlLen,
    pub(crate) strlen_or_ind_ptr: *mut crate::api::odbc_types::SqlLen,
}

// SAFETY: the raw pointers are application-owned buffers that the ODBC contract
// requires to stay valid until the column is unbound; they are only written
// while the statement mutex is held, exactly like `rows_fetched_ptr`.
unsafe impl Send for BoundCol {}
unsafe impl Sync for BoundCol {}

impl StmtState {
    /// Discards the current row and any rows buffered ahead of the cursor.
    /// Called whenever the cursor is repositioned, closed, or re-executed.
    pub(crate) fn reset_rows(&mut self) {
        self.current_row = None;
        self.buffered_rows.clear();
        self.buffered_eof = false;
        self.reset_getdata();
    }

    /// Clears `SQLGetData` streaming position; called whenever the current row
    /// changes, since offsets are only meaningful within a single row.
    pub(crate) fn reset_getdata(&mut self) {
        self.getdata_col = None;
        self.getdata_offset = 0;
        self.getdata_done = false;
    }

    pub(crate) fn has_state(&self, mask: u32) -> bool {
        (self.state_flags & mask) != 0
    }

    pub(crate) fn set_state(&mut self, mask: u32) {
        self.state_flags |= mask;
    }

    pub(crate) fn clear_state(&mut self, mask: u32) {
        self.state_flags &= !mask;
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
                buffered_rows: VecDeque::new(),
                buffered_eof: false,
                row_count: -1,
                pending_row_counts: VecDeque::new(),
                row_array_size: 1,
                rows_fetched_ptr: std::ptr::null_mut(),
                row_status_ptr: std::ptr::null_mut(),
                row_bind_type: crate::api::odbc_types::SQL_BIND_BY_COLUMN,
                bound_cols: Vec::new(),
                getdata_col: None,
                getdata_offset: 0,
                getdata_done: false,
                paramset_size: 1,
                dae: None,
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
