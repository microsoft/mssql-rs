// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use super::desc::{DescHandle, DescKind};
use super::{DbcHandle, HandleType, HasObjectType, free_handle, handle_to_raw};
use crate::api::odbc_types::{SqlULen, SqlUSmallInt};
use crate::error::{DiagRecord, HasDiagnostics};
use crate::params::BoundParam;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::query::metadata::{ColumnMetadata, PlpEncoding};

/// State for a PLP column being streamed across repeated SQLGetData calls.
#[derive(Debug)]
pub(crate) struct ActivePlpStream {
    /// 1-based column ordinal being streamed.
    pub(crate) column: usize,
    /// Wire encoding of the PLP column.
    pub(crate) encoding: PlpEncoding,
    /// Trailing odd wire byte from the previous read, awaiting its pair. Only
    /// used on the UTF-16LE -> UTF-8 (`nvarchar(max)` -> `SQL_C_CHAR`) path,
    /// where a chunk boundary can fall between the two bytes of a code unit.
    pub(crate) pending_byte: Option<u8>,
    /// High surrogate whose low half lands in the next chunk. Held back so the
    /// pair is transcoded together instead of each half becoming U+FFFD.
    pub(crate) pending_high_surrogate: Option<u16>,
}

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
    /// `true` when SQLFetch has positioned the cursor on a row ready for SQLGetData.
    pub(crate) row_positioned: bool,
    /// The column value captured by the most recent resume_row_to_column call, with its 1-based column index.
    pub(crate) last_captured: Option<(usize, ColumnValues)>,
    /// `true` when the last resume consumed the row's final column
    /// (`CursorColumn::RowEnded`). Distinguishes "row exhausted" from "decoder
    /// paused at a PLP column" when `last_captured` is `None` (see
    /// `get_data.rs` resume path).
    pub(crate) row_exhausted: bool,
    /// Active PLP stream state; `None` when no PLP stream is in progress.
    pub(crate) active_plp: Option<ActivePlpStream>,
    /// 1-based column number of the last successful SQLGetData call on this row.
    /// Used to enforce forward-only column access (07009) and SQL_NO_DATA on re-read.
    pub(crate) current_row_last_col: usize,
    /// Byte/code-unit offset into the current non-PLP column's text, for
    /// resumable `SQLGetData`. `(1-based column, offset)`; `None` when no
    /// partial read is outstanding. The offset unit matches the target C type
    /// the column is being read as (bytes for `SQL_C_CHAR`, UTF-16 code units
    /// for `SQL_C_WCHAR`); a single column's chunk loop uses one target type.
    pub(crate) partial_text_offset: Option<(usize, usize)>,
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

    /// Clears all row-stream state (cursor invalidated, no PLP in progress).
    pub(crate) fn reset_row_stream(&mut self) {
        self.row_positioned = false;
        self.last_captured = None;
        self.row_exhausted = false;
        self.active_plp = None;
        self.current_row_last_col = 0;
        self.partial_text_offset = None;
    }

    /// Positions the row stream on a freshly fetched row: clears all per-row
    /// state, then marks the cursor as positioned for `SQLGetData`. This is the
    /// "begin a new row" counterpart to `reset_row_stream`'s "invalidate"; both
    /// clear the same fields, but keeping them named apart means a future
    /// row-scoped field that must differ between the two cases can't silently
    /// inherit the invalidate value.
    pub(crate) fn begin_row(&mut self) {
        self.reset_row_stream();
        self.row_positioned = true;
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
                row_positioned: false,
                last_captured: None,
                row_exhausted: false,
                active_plp: None,
                current_row_last_col: 0,
                partial_text_offset: None,
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
