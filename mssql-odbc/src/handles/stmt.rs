// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use tracing::error;

use mssql_tds::connection::tds_client::{PreparedStatement, StatementId, TdsClient};

use super::desc::{DescHandle, DescKind};
use super::{DbcHandle, HandleType, HasObjectType, free_handle, handle_to_raw};
use crate::api::odbc_types::{
    SQL_DESC_ALLOC_AUTO, SqlLen, SqlPointer, SqlSmallInt, SqlULen, SqlUSmallInt,
};
use crate::error::{DiagRecord, HasDiagnostics};
use crate::params::BoundParam;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sqldatatypes::TdsDataType;
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

/// An application buffer bound to a result-set column by `SQLBindCol`.
///
/// The pointers belong to the application, which must keep them valid until it
/// unbinds the column, rebinds it, or frees the statement. They are written
/// only during a bound fetch, never at bind time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ColumnBinding {
    /// 1-based column number, as passed to `SQLBindCol`.
    pub(crate) column_number: SqlUSmallInt,
    /// The requested `SQL_C_*` target type.
    pub(crate) target_type: SqlSmallInt,
    /// Start of the application's buffer, or of its array when the rowset holds
    /// more than one row.
    pub(crate) target_value_ptr: SqlPointer,
    /// Capacity of one element of `target_value_ptr`, in bytes.
    pub(crate) buffer_length: SqlLen,
    /// Receives the length/indicator for each row, or null if the application
    /// does not want one.
    pub(crate) strlen_or_ind_ptr: *mut SqlLen,
}

pub(crate) const STMT_STATE_EXEC_STARTED: u32 = 0x0000_0100;
pub(crate) const STMT_STATE_PREPARED: u32 = 0x0000_0200;
pub(crate) const STMT_STATE_CURSOR_OPEN: u32 = 0x0000_0800;
pub(crate) const STMT_STATE_EXEC_CONTEXT: u32 = 0x0000_1000;
/// A block fetch is between taking its snapshot of the bindings and finishing
/// its writes. The fetch cannot hold the statement mutex across network I/O, so
/// this is what stops a concurrent rebind from freeing a buffer the fill loop is
/// still writing through: the mutating entry points refuse while it is set.
pub(crate) const STMT_STATE_FETCH_IN_PROGRESS: u32 = 0x0000_2000;

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
    /// rationale as `parent_dbc`. These are NOT the mutable *active* ARD/APD
    /// association `SQLSetStmtAttr(SQL_ATTR_APP_ROW_DESC / APP_PARAM_DESC)`
    /// swaps (msodbcsql's separate `pARD`/`pAPD`) — that association lives in
    /// `StmtState::active_ard`/`active_apd` behind `inner`, since concurrent
    /// set/get would otherwise race. A `None` active slot means "use the
    /// implicit descriptor here"; `Some(explicit_desc)` means an explicitly
    /// allocated descriptor has been associated instead. IRD/IPD are never
    /// swappable, so they have no `active_*` counterpart.
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
    /// The prepared statement (rewritten SQL + server handle once materialized)
    /// stored by `SQLPrepare`, bundled with its `@P1..@Pn` marker count so the
    /// two can only be set together. The server-side prepare is deferred to
    /// `SQLExecute`. `Some` marks the statement as prepared; the handle is filled
    /// after the first execute.
    pub(crate) prepared: Option<PreparedPlan>,
    /// Metadata inferred by `SQLDescribeParam`, indexed by parameter ordinal.
    /// The first describe call fills every marker; `SQLPrepare` invalidates it.
    pub(crate) parameter_metadata: Vec<ParameterDescription>,
    /// Parameters bound via `SQLBindParameter`, indexed by `(ParameterNumber
    /// - 1)`. `None` slots are gaps left by binding a higher ordinal first.
    pub(crate) bound_params: Vec<Option<BoundParam>>,
    /// The identity of a prepared statement superseded by a re-prepare / rebind
    /// / `SQLExecDirect`, whose server handle awaits release with `sp_unprepare`.
    /// The drop is deferred to the next point that already holds the TDS client
    /// (execute / exec-direct) or to statement free, so bind/prepare stay
    /// I/O-free. Invariant: this is `None` whenever `prepared` holds a live
    /// handle (a new handle can only be acquired by an execute, which flushes any
    /// pending drop first). `mssql_tds::TdsClient::execute_prepared` relies on
    /// this: it is what keeps a live orphan from being discarded on the
    /// `sp_execute` reuse path.
    pub(crate) pending_unprepare: Option<StatementId>,
    /// `true` when SQLFetch has positioned the cursor on a row ready for SQLGetData.
    pub(crate) row_positioned: bool,
    /// The column value captured by the most recent resume_row_to_column call, with its 1-based column index.
    pub(crate) last_captured: Option<(usize, ColumnValues)>,
    /// Base type of `last_captured` when that column is `sql_variant`, with its
    /// 1-based column index. Set per value, since a variant column can hold a
    /// different type in every row.
    pub(crate) last_variant_base: Option<(usize, TdsDataType)>,
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
    /// Application-supplied byte offset added to every bound column's data and
    /// indicator pointer at fetch time (`SQL_ATTR_ROW_BIND_OFFSET_PTR`); null
    /// when unset. Read at fetch rather than at bind, so the application can
    /// move the whole rowset by updating the pointed-to value.
    pub(crate) row_bind_offset_ptr: *mut SqlULen,
    /// Columns bound by `SQLBindCol`, in binding order. A column appears at
    /// most once: rebinding replaces its entry, unbinding removes it. Bindings
    /// outlive a result set, so they are cleared by `SQLFreeStmt(SQL_UNBIND)`
    /// rather than by closing the cursor.
    ///
    /// Staying empty is a legal state: an unbound `SQLFetchScroll` still
    /// advances the rowset and reports counts, it just delivers no data.
    pub(crate) bindings: Vec<ColumnBinding>,
    /// The active application row descriptor for `SQL_ATTR_APP_ROW_DESC`:
    /// `None` means "use the implicit ARD" (`StmtHandle::ard`); `Some` holds
    /// an explicitly-allocated descriptor associated by
    /// `SQLSetStmtAttrW(SQL_ATTR_APP_ROW_DESC, ...)`. A raw pointer, not an
    /// owned handle — the DBC owns explicit descriptors, so freeing one resets
    /// every statement referencing it back to `None` (`free_handle::free_desc`).
    pub(crate) active_ard: Option<*mut c_void>,
    /// The active application parameter descriptor for
    /// `SQL_ATTR_APP_PARAM_DESC`. See [`Self::active_ard`].
    pub(crate) active_apd: Option<*mut c_void>,
    /// Statement lifecycle/status flags used for ODBC API state checks.
    pub(crate) state_flags: u32,
    /// The data-at-execution sequence in progress, if any. `Some` is exactly
    /// the ODBC "Need Data" state — see [`StmtState::needs_data`].
    pub(crate) dae: Option<DaeState>,
}

/// One data-at-execution parameter: which binding it refers to and how many
/// bytes the application promised for it.
///
/// Keeping the pair together means a parameter can never be described by an
/// index with no matching length, which two parallel `Vec`s cannot rule out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DaeParam {
    /// 0-based index into [`StmtState::bound_params`].
    pub(crate) bound_index: usize,
    /// Total byte count declared by `SQL_LEN_DATA_AT_EXEC(n)`; `None` for
    /// `SQL_DATA_AT_EXEC`, where the application promised no total.
    pub(crate) expected_len: Option<usize>,
}

/// How much of the open data-at-execution parameter the application has
/// supplied. Reset as a unit whenever the cursor advances.
#[derive(Debug, Default)]
pub(crate) struct DaeProgress {
    /// Bytes supplied by `SQLPutData`, counted before any server-side
    /// conversion to match msodbcsql's `cbDataAppGiven`.
    pub(crate) bytes_sent: usize,
    /// Set by the first `SQLPutData` for this parameter, including zero-length
    /// and NULL writes. Closing a parameter without one is a sequence error in
    /// msodbcsql.
    pub(crate) put_data_called: bool,
    /// The parameter was supplied as SQL NULL, so the declared-length check is
    /// skipped, as in msodbcsql.
    pub(crate) is_null: bool,
}

/// A data-at-execution sequence in progress: everything the statement holds
/// between the `SQL_NEED_DATA` returned by `SQLExecute` / `SQLExecDirect` and
/// the `SQLParamData` that finishes the streaming RPC.
///
/// Grouped rather than spread across the statement so the whole sequence is
/// established and torn down in one move, and so no field can outlive it.
#[derive(Debug)]
pub(crate) struct DaeState {
    /// The streaming client, parked here while the DBC's own `client` is
    /// `None` and its `active_stmt` keeps the connection busy. Private, with
    /// `call_in_flight`, so the two can only move together — see
    /// [`DaeState::checkout_client`].
    client: Option<TdsClient>,
    /// `true` while `SQLPutData` or `SQLParamData` holds the client for network
    /// I/O.
    call_in_flight: bool,
    /// The prepared plan stashed for the sequence, written back to
    /// [`StmtState::prepared`] when it ends. `None` for `SQLExecDirect`, which
    /// runs ad-hoc `sp_executesql` and has no plan to restore.
    prepared: Option<PreparedPlan>,
    /// Orphaned server handle to release at next-execute time, stashed
    /// identically to the non-DAE path.
    orphaned: Option<StatementId>,
    /// The streamed parameters, in original parameter order.
    params: Vec<DaeParam>,
    /// Which of `params` is open. `None` until the first `SQLParamData`, which
    /// only hands the application a pointer.
    pub(crate) cursor: Option<usize>,
    /// Progress on the parameter named by `cursor`.
    pub(crate) progress: DaeProgress,
}

impl DaeState {
    pub(crate) fn new(
        client: TdsClient,
        prepared: Option<PreparedPlan>,
        orphaned: Option<StatementId>,
        params: Vec<DaeParam>,
    ) -> Self {
        Self {
            client: Some(client),
            call_in_flight: false,
            prepared,
            orphaned,
            params,
            cursor: None,
            progress: DaeProgress::default(),
        }
    }

    /// Checks the client out for a network write, so no lock is held across the
    /// I/O. `None` means the sequence has no client to give, which is internal
    /// state corruption. Pairs with [`DaeState::return_client`].
    #[must_use = "a checked-out client must be returned or disposed of"]
    pub(crate) fn checkout_client(&mut self) -> Option<TdsClient> {
        let client = self.client.take()?;
        self.call_in_flight = true;
        Some(client)
    }

    /// Parks the client back on the sequence after a write completes.
    pub(crate) fn return_client(&mut self, client: TdsClient) {
        self.client = Some(client);
        self.call_in_flight = false;
    }

    /// `true` while `SQLPutData` or `SQLParamData` holds the client for network
    /// I/O. `SQLCancel` is the one call an application may make on a busy
    /// statement from another thread, and during this window the sequence is not
    /// `SQLCancel`'s to reset: the owning thread is about to write back into it.
    pub(crate) fn call_in_flight(&self) -> bool {
        self.call_in_flight
    }

    /// The parameter the cursor is on; `None` before the first `SQLParamData`
    /// and once the last parameter has been closed.
    pub(crate) fn current_param(&self) -> Option<&DaeParam> {
        self.params.get(self.cursor?)
    }

    /// Opens the next parameter, discarding the closed one's progress. Called
    /// by every `SQLParamData`, including the first, which opens parameter 0.
    pub(crate) fn advance(&mut self) {
        self.cursor = Some(self.cursor.map_or(0, |current| current + 1));
        self.progress = DaeProgress::default();
    }

    /// Builds a sequence with no parked client, for tests that exercise the
    /// validation paths reached before any network write.
    #[cfg(test)]
    pub(crate) fn for_test(params: Vec<DaeParam>, cursor: Option<usize>) -> Self {
        Self {
            client: None,
            call_in_flight: false,
            prepared: None,
            orphaned: None,
            params,
            cursor,
            progress: DaeProgress::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_call_in_flight(&mut self, in_flight: bool) {
        self.call_in_flight = in_flight;
    }
}

/// A prepared statement bundled with the marker count of its rewritten SQL, so
/// the two can only be set together (the count is a property of the rewritten
/// `@P1..@Pn` text and must always agree with it).
#[derive(Debug)]
pub(crate) struct PreparedPlan {
    pub(crate) stmt: PreparedStatement,
    /// Number of `@P1..@Pn` markers in `stmt`'s SQL, computed once at prepare so
    /// `SQLExecute` builds the parameter list without re-scanning the text.
    pub(crate) marker_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParameterDescription {
    pub(crate) data_type: SqlSmallInt,
    pub(crate) parameter_size: SqlULen,
    pub(crate) decimal_digits: SqlSmallInt,
    pub(crate) nullable: SqlSmallInt,
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

    /// Binds, or rebinds, one column. A column can only be bound once, so an
    /// existing entry for the same column is replaced in place rather than
    /// shadowed.
    pub(crate) fn set_binding(&mut self, binding: ColumnBinding) {
        // Kept ordered by column number: the fill loop walks a forward-only row
        // cursor, so it can only visit bound columns in ascending order.
        match self
            .bindings
            .binary_search_by_key(&binding.column_number, |b| b.column_number)
        {
            Ok(existing) => self.bindings[existing] = binding,
            Err(insert_at) => self.bindings.insert(insert_at, binding),
        }
    }

    /// Removes one column's binding, which is what `SQLBindCol` does when the
    /// application passes a null `TargetValuePtr`.
    pub(crate) fn clear_binding(&mut self, column_number: SqlUSmallInt) {
        self.bindings.retain(|b| b.column_number != column_number);
    }

    /// Drops every column binding — `SQLFreeStmt(SQL_UNBIND)`.
    pub(crate) fn clear_bindings(&mut self) {
        self.bindings.clear();
    }

    /// Clears all row-stream state (cursor invalidated, no PLP in progress).
    pub(crate) fn reset_row_stream(&mut self) {
        self.row_positioned = false;
        self.last_captured = None;
        self.last_variant_base = None;
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

    /// Moves the current prepared statement's live server handle into
    /// `pending_unprepare` (keeping its SQL in `prepared` so the next execute
    /// re-prepares it) so the next execute / exec-direct — or statement free —
    /// releases it with `sp_unprepare`. Called by re-prepare, rebind, and
    /// `SQLExecDirect` when the current prepared plan is superseded. No network
    /// I/O.
    pub(crate) fn orphan_prepared_handle(&mut self) {
        let Some(plan) = self.prepared.as_mut() else {
            return;
        };
        let Some(id) = plan.stmt.take_id() else {
            // No materialized handle to release; the statement stays in place.
            return;
        };
        if let Some(previous) = self.pending_unprepare.replace(id) {
            debug_assert!(
                false,
                "orphan_prepared_handle: a pending unprepare already exists"
            );
            error!(
                orphaned = ?previous,
                "orphan_prepared_handle: overwriting a pending unprepare — handle leaked until disconnect"
            );
        }
    }
    /// Resets all data-at-execution streaming state and hands back the parked
    /// client, if the sequence still held one. Call after a DAE sequence
    /// completes, is cancelled, or fails.
    ///
    /// The prepared plan and orphaned handle the sequence was holding are
    /// restored in the same step, so the statement is never observable as idle
    /// but unprepared. The caller owns the returned client and must dispose of
    /// it — `None` means a `SQLPutData` / `SQLParamData` call has it checked
    /// out and will dispose of it itself.
    #[must_use = "the parked client must be cancelled and returned to the connection"]
    pub(crate) fn take_dae(&mut self) -> Option<TdsClient> {
        let dae = self.dae.take()?;
        self.prepared = dae.prepared;
        self.pending_unprepare = dae.orphaned;
        dae.client
    }

    /// `true` while the statement is in the ODBC "Need Data" state, suspended
    /// until `SQLPutData` / `SQLParamData` supply the data-at-execution
    /// parameter values.
    pub(crate) fn needs_data(&self) -> bool {
        self.dae.is_some()
    }

    /// The application's `ParameterValuePtr` for the open DAE parameter — the
    /// token `SQLParamData` hands back so the application knows which parameter
    /// to supply. Null when no parameter is open or its binding is gone.
    pub(crate) fn dae_current_value_ptr(&self) -> SqlPointer {
        self.dae_current_bound_param()
            .map_or(std::ptr::null_mut(), |param| param.parameter_value_ptr)
    }

    /// The bound C type of the open DAE parameter, which `SQLPutData` needs to
    /// size an `SQL_NTS` chunk.
    pub(crate) fn dae_current_c_type(&self) -> Option<SqlSmallInt> {
        self.dae_current_bound_param().map(|param| param.c_type)
    }

    fn dae_current_bound_param(&self) -> Option<&BoundParam> {
        let dae_param = self.dae.as_ref()?.current_param()?;
        self.bound_params.get(dae_param.bound_index)?.as_ref()
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
            ard: handle_to_raw(Box::new(DescHandle::new(
                DescKind::AppRow,
                SQL_DESC_ALLOC_AUTO,
                parent_dbc,
            ))),
            apd: handle_to_raw(Box::new(DescHandle::new(
                DescKind::AppParam,
                SQL_DESC_ALLOC_AUTO,
                parent_dbc,
            ))),
            ird: handle_to_raw(Box::new(DescHandle::new(
                DescKind::ImpRow,
                SQL_DESC_ALLOC_AUTO,
                parent_dbc,
            ))),
            ipd: handle_to_raw(Box::new(DescHandle::new(
                DescKind::ImpParam,
                SQL_DESC_ALLOC_AUTO,
                parent_dbc,
            ))),
            inner: Mutex::new(StmtState {
                diag_records: Vec::new(),
                column_metadata: Vec::new(),
                prepared: None,
                parameter_metadata: Vec::new(),
                bound_params: Vec::new(),
                pending_unprepare: None,
                row_positioned: false,
                last_captured: None,
                last_variant_base: None,
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
                row_bind_offset_ptr: std::ptr::null_mut(),
                bindings: Vec::new(),
                active_ard: None,
                active_apd: None,
                state_flags: 0,
                dae: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_C_SLONG};

    fn binding(column_number: SqlUSmallInt, target_type: SqlSmallInt) -> ColumnBinding {
        ColumnBinding {
            column_number,
            target_type,
            target_value_ptr: std::ptr::null_mut(),
            buffer_length: 0,
            strlen_or_ind_ptr: std::ptr::null_mut(),
        }
    }

    /// Runs `f` against a fresh statement's state. The handle owns descriptor
    /// allocations it frees on drop, so it has to outlive the borrow.
    fn with_state(f: impl FnOnce(&mut StmtState)) {
        let handle = StmtHandle::new(std::ptr::null_mut());
        let mut state = handle.inner.lock().unwrap();
        f(&mut state);
    }

    /// A column can only be bound once, so rebinding replaces the entry rather
    /// than shadowing it — otherwise the fetch loop would write the column
    /// twice, once through a stale pointer.
    #[test]
    fn rebinding_a_column_replaces_its_entry() {
        with_state(|s| {
            s.set_binding(binding(1, SQL_C_SLONG));
            s.set_binding(binding(2, SQL_C_SLONG));
            s.set_binding(binding(1, SQL_C_CHAR));

            assert_eq!(s.bindings.len(), 2);
            let first = s.bindings.iter().find(|b| b.column_number == 1).unwrap();
            assert_eq!(first.target_type, SQL_C_CHAR);
        });
    }

    /// Unbinding one column leaves the others in place; this is what SQLBindCol
    /// does with a null TargetValuePtr.
    #[test]
    fn clearing_one_binding_leaves_the_others() {
        with_state(|s| {
            s.set_binding(binding(1, SQL_C_SLONG));
            s.set_binding(binding(2, SQL_C_SLONG));
            s.clear_binding(1);

            assert_eq!(s.bindings.len(), 1);
            assert_eq!(s.bindings[0].column_number, 2);
            // Unbinding a column that was never bound is a no-op, not a panic.
            s.clear_binding(99);
            assert_eq!(s.bindings.len(), 1);
        });
    }

    /// SQLFreeStmt(SQL_UNBIND) drops the whole table.
    #[test]
    fn clearing_all_bindings_empties_the_table() {
        with_state(|s| {
            s.set_binding(binding(1, SQL_C_SLONG));
            s.set_binding(binding(2, SQL_C_SLONG));
            s.clear_bindings();
            assert!(s.bindings.is_empty());
        });
    }

    /// The fill loop reads a forward-only cursor, so it depends on this order
    /// rather than re-establishing it; an application may bind in any order.
    #[test]
    fn bindings_stay_ordered_by_column_however_they_were_bound() {
        with_state(|s| {
            for col in [5, 1, 3, 2, 4] {
                s.set_binding(binding(col, SQL_C_SLONG));
            }
            let cols: Vec<_> = s.bindings.iter().map(|b| b.column_number).collect();
            assert_eq!(cols, vec![1, 2, 3, 4, 5]);

            // A rebind replaces in place and keeps the order.
            s.set_binding(binding(3, SQL_C_CHAR));
            let cols: Vec<_> = s.bindings.iter().map(|b| b.column_number).collect();
            assert_eq!(cols, vec![1, 2, 3, 4, 5]);
            assert_eq!(s.bindings[2].target_type, SQL_C_CHAR);
        });
    }

    /// Bindings start empty, which is what makes an unbound fetch legal.
    #[test]
    fn a_fresh_statement_has_no_bindings() {
        with_state(|s| {
            assert!(s.bindings.is_empty());
            assert!(s.row_bind_offset_ptr.is_null());
        });
    }
}
