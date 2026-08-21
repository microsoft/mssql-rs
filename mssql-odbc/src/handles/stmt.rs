// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use tracing::error;

use mssql_tds::connection::tds_client::{PreparedStatement, StatementId};

use super::desc::{DescHandle, DescKind};
use super::{DbcHandle, HandleType, HasObjectType, free_handle, handle_to_raw};
use crate::api::odbc_types::{self, SqlInteger, SqlSmallInt, SqlULen, SqlUSmallInt};
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
    /// Statement lifecycle/status flags used for ODBC API state checks.
    pub(crate) state_flags: u32,
    /// `SQL_ATTR_QUERY_TIMEOUT` in seconds; `0` (the ODBC default) means no
    /// timeout. Seeded at allocation from the parent connection's
    /// [`DbcState::stmt_query_timeout`].
    ///
    /// Stored and reported only — enforcement against a running query is
    /// tracked separately (AB#46385), so a non-zero value does not yet cancel
    /// anything. msodbcsql does enforce it and answers `HYT00` on expiry.
    pub(crate) query_timeout: u32,
    /// `SQL_ATTR_MAX_ROWS`: cap on the number of rows returned from each result
    /// set; `0` (the ODBC default) means no cap.
    ///
    /// Unlike the rest of the S4 attributes this one is genuinely enforced.
    /// msodbcsql stops the cursor at the cap — a `SELECT TOP 10` under
    /// `MAX_ROWS = 3` yields exactly three rows — so merely storing the value
    /// would quietly hand the application seven rows it asked not to receive.
    pub(crate) max_rows: SqlULen,
    /// Rows already handed to the application from the current result set.
    /// Counted solely to enforce [`Self::max_rows`], and restarted by
    /// [`Self::begin_result_set`] because the cap is per result set.
    pub(crate) rows_returned: SqlULen,
    /// Statement attributes msodbcsql accepts and round-trips but that have no
    /// effect on this driver's forward-only, read-only cursor.
    pub(crate) inert_attrs: InertStmtAttrs,
    /// SQL Server vendor statement attributes (`SQL_SOPT_SS_*`), which unlike
    /// the inert set validate their value before storing it.
    pub(crate) vendor_attrs: VendorStmtAttrs,
    /// Query-notification message text (`SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT`)
    /// and options (`..._OPTIONS`). The only two string-valued statement
    /// attributes, so they sit beside [`Self::vendor_attrs`] rather than in it.
    /// Stored and round-tripped; query notifications themselves are a separate
    /// feature.
    pub(crate) qn_msgtext: String,
    pub(crate) qn_options: String,
    /// Ordinal of the command being processed within the current batch, which
    /// is what `SQL_SOPT_SS_CURRENT_COMMAND` reports: `0` before execute, then
    /// one per result set begun. msodbcsql holds the final value once the batch
    /// is exhausted rather than advancing past it.
    pub(crate) current_command: SqlULen,
}

/// Statement attributes msodbcsql stores and round-trips without acting on,
/// paired with the default it reports before any set.
///
/// Every entry was measured against msodbcsql 18 rather than taken from the
/// ODBC headers, because several defaults are driver choices rather than
/// specification values: `SQL_ATTR_SIMULATE_CURSOR` reports `SQL_SC_UNIQUE`,
/// `SQL_ROWSET_SIZE` reports 1, and `SQL_ATTR_CURSOR_SENSITIVITY` reports
/// `SQL_INSENSITIVE` even though the header default is `SQL_UNSPECIFIED`.
/// See `docs/attributes_plan.md` §8.
const INERT_STMT_ATTRS: &[(SqlInteger, SqlULen)] = &[
    (
        odbc_types::SQL_ATTR_CURSOR_SENSITIVITY,
        odbc_types::SQL_INSENSITIVE,
    ),
    (odbc_types::SQL_ATTR_NOSCAN, 0),
    (odbc_types::SQL_ATTR_MAX_LENGTH, 0),
    (odbc_types::SQL_ATTR_ASYNC_ENABLE, 0),
    (odbc_types::SQL_ATTR_KEYSET_SIZE, 0),
    (odbc_types::SQL_ROWSET_SIZE, 1),
    (
        odbc_types::SQL_ATTR_SIMULATE_CURSOR,
        odbc_types::SQL_SC_UNIQUE,
    ),
    (odbc_types::SQL_ATTR_RETRIEVE_DATA, odbc_types::SQL_RD_ON),
    (odbc_types::SQL_ATTR_USE_BOOKMARKS, 0),
    (odbc_types::SQL_ATTR_ENABLE_AUTO_IPD, 0),
    (odbc_types::SQL_ATTR_FETCH_BOOKMARK_PTR, 0),
    (odbc_types::SQL_ATTR_PARAM_BIND_OFFSET_PTR, 0),
    (odbc_types::SQL_ATTR_PARAM_BIND_TYPE, 0),
    (odbc_types::SQL_ATTR_PARAM_OPERATION_PTR, 0),
    (odbc_types::SQL_ATTR_PARAM_STATUS_PTR, 0),
    (odbc_types::SQL_ATTR_PARAMS_PROCESSED_PTR, 0),
    (odbc_types::SQL_ATTR_ROW_BIND_OFFSET_PTR, 0),
    (odbc_types::SQL_ATTR_ROW_OPERATION_PTR, 0),
    (odbc_types::SQL_ATTR_METADATA_ID, 0),
];

/// Values for the [`INERT_STMT_ATTRS`] identifiers, positionally aligned with
/// that table.
///
/// A flat array keyed by a linear scan is deliberate: the set is small and
/// fixed, and holding the identifiers and their defaults in one auditable
/// table keeps a measured default from drifting away from the attribute it
/// belongs to, which nineteen separate struct fields would invite.
#[derive(Debug, Clone)]
pub(crate) struct InertStmtAttrs([SqlULen; INERT_STMT_ATTRS.len()]);

impl Default for InertStmtAttrs {
    fn default() -> Self {
        let mut values = [0; INERT_STMT_ATTRS.len()];
        for (slot, (_, default)) in values.iter_mut().zip(INERT_STMT_ATTRS) {
            *slot = *default;
        }
        Self(values)
    }
}

impl InertStmtAttrs {
    /// The identifiers this store covers, in table order. Test-only: it exists
    /// so a new entry cannot be added without an asserted msodbcsql default.
    #[cfg(test)]
    pub(crate) fn identifiers() -> impl Iterator<Item = SqlInteger> {
        INERT_STMT_ATTRS.iter().map(|(id, _)| *id)
    }

    fn index_of(attribute: SqlInteger) -> Option<usize> {
        INERT_STMT_ATTRS.iter().position(|(id, _)| *id == attribute)
    }
    /// Returns the stored value, or `None` when `attribute` is not one of the
    /// inert identifiers.
    pub(crate) fn get(&self, attribute: SqlInteger) -> Option<SqlULen> {
        Self::index_of(attribute).map(|i| self.0[i])
    }

    /// Stores `value`, returning whether `attribute` is an inert identifier.
    pub(crate) fn set(&mut self, attribute: SqlInteger, value: SqlULen) -> bool {
        match Self::index_of(attribute) {
            Some(i) => {
                self.0[i] = value;
                true
            }
            None => false,
        }
    }

    /// Reads the byte offset `SQL_ATTR_PARAM_BIND_OFFSET_PTR` currently points
    /// at, or 0 when the application has not set one.
    ///
    /// The attribute holds a *pointer to* the offset rather than the offset
    /// itself, so the application can move every binding by writing one
    /// `SQLLEN` between executions. It is therefore read at execute time, not
    /// at set time.
    ///
    /// # Safety
    /// When set, the pointer must address a live, aligned `SQLLEN` for the
    /// duration of the execution, per the `SQLSetStmtAttr` contract.
    pub(crate) unsafe fn param_bind_offset(&self) -> isize {
        let ptr = self
            .get(odbc_types::SQL_ATTR_PARAM_BIND_OFFSET_PTR)
            .unwrap_or(0) as *const odbc_types::SqlLen;
        if ptr.is_null() {
            return 0;
        }
        unsafe { ptr.read() }
    }
}

/// How msodbcsql validates a vendor statement attribute's value.
///
/// Each rule was established by sweeping the value space against msodbcsql 18
/// and recording where it flipped from success to `HY024`, not by reading the
/// documented constants — two attributes accept values the headers do not name,
/// and two reject every value the headers do name.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ValueRule {
    /// Accepts exactly these values.
    OneOf(&'static [SqlULen]),
    /// Accepts any value in this inclusive range.
    Range(SqlULen, SqlULen),
    /// Recognized, but refuses every value. Both attributes in this class name
    /// features the driver does not implement (table-valued parameters, Always
    /// Encrypted), and msodbcsql itself refuses them here too — including their
    /// documented "off" value, and including `SQL_SOPT_SS_COLUMN_ENCRYPTION` on
    /// a connection opened with `ColumnEncryption=Enabled`.
    Rejected,
    /// Reported by the get path but not settable. Set is left to fall through
    /// to the identifier-rejection path, because msodbcsql answers `HY092`
    /// ("invalid attribute identifier") rather than `HY024` for these.
    GetOnly,
}

/// Vendor statement attributes, their msodbcsql defaults, and the values
/// msodbcsql accepts for each.
///
/// Held as one table for the same reason as [`INERT_STMT_ATTRS`]: the default
/// and the accept rule are both measurements, and keeping them adjacent to the
/// identifier stops one from drifting without the other.
const VENDOR_STMT_ATTRS: &[(SqlInteger, SqlULen, ValueRule)] = &[
    (
        odbc_types::SQL_SOPT_SS_TEXTPTR_LOGGING,
        odbc_types::SQL_TL_ON,
        ValueRule::OneOf(&[0, 1]),
    ),
    (
        odbc_types::SQL_SOPT_SS_CURRENT_COMMAND,
        0,
        ValueRule::GetOnly,
    ),
    (
        odbc_types::SQL_SOPT_SS_HIDDEN_COLUMNS,
        0,
        ValueRule::OneOf(&[0, 1]),
    ),
    (
        odbc_types::SQL_SOPT_SS_NOBROWSETABLE,
        0,
        ValueRule::OneOf(&[0, 1]),
    ),
    (
        odbc_types::SQL_SOPT_SS_REGIONALIZE,
        0,
        ValueRule::OneOf(&[0, 1]),
    ),
    (
        odbc_types::SQL_SOPT_SS_CURSOR_OPTIONS,
        0,
        ValueRule::Range(0, odbc_types::SQL_CO_MAX),
    ),
    (
        odbc_types::SQL_SOPT_SS_NOCOUNT_STATUS,
        odbc_types::SQL_NC_ON,
        ValueRule::GetOnly,
    ),
    (
        odbc_types::SQL_SOPT_SS_DEFER_PREPARE,
        odbc_types::SQL_DP_ON,
        ValueRule::OneOf(&[0, 1]),
    ),
    (
        odbc_types::SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT,
        odbc_types::SQL_QN_TIMEOUT_DEFAULT,
        // Zero is rejected, unlike most ODBC timeouts where it means "no limit".
        ValueRule::Range(1, u32::MAX as SqlULen),
    ),
    (odbc_types::SQL_SOPT_SS_PARAM_FOCUS, 0, ValueRule::Rejected),
    (
        odbc_types::SQL_SOPT_SS_NAME_SCOPE,
        0,
        ValueRule::Range(0, odbc_types::SQL_SS_NAME_SCOPE_MAX),
    ),
    (
        odbc_types::SQL_SOPT_SS_COLUMN_ENCRYPTION,
        0,
        ValueRule::Rejected,
    ),
];

/// Values for the [`VENDOR_STMT_ATTRS`] identifiers, positionally aligned with
/// that table.
#[derive(Debug, Clone)]
pub(crate) struct VendorStmtAttrs([SqlULen; VENDOR_STMT_ATTRS.len()]);

impl Default for VendorStmtAttrs {
    fn default() -> Self {
        let mut values = [0; VENDOR_STMT_ATTRS.len()];
        for (slot, (_, default, _)) in values.iter_mut().zip(VENDOR_STMT_ATTRS) {
            *slot = *default;
        }
        Self(values)
    }
}

impl VendorStmtAttrs {
    /// The identifiers this store covers, in table order. Test-only: it exists
    /// so a new entry cannot be added without an asserted msodbcsql default.
    #[cfg(test)]
    pub(crate) fn identifiers() -> impl Iterator<Item = SqlInteger> {
        VENDOR_STMT_ATTRS.iter().map(|(id, _, _)| *id)
    }

    fn index_of(attribute: SqlInteger) -> Option<usize> {
        VENDOR_STMT_ATTRS
            .iter()
            .position(|(id, _, _)| *id == attribute)
    }

    /// Whether `attribute` is a vendor attribute the set path owns.
    ///
    /// False for the get-only identifiers, so they fall through to the
    /// identifier-rejection path and answer `HY092` rather than `HY024`.
    pub(crate) fn is_settable(attribute: SqlInteger) -> bool {
        Self::index_of(attribute)
            .is_some_and(|i| !matches!(VENDOR_STMT_ATTRS[i].2, ValueRule::GetOnly))
    }

    /// Returns the stored value, or `None` when `attribute` is not a vendor
    /// statement attribute.
    pub(crate) fn get(&self, attribute: SqlInteger) -> Option<SqlULen> {
        Self::index_of(attribute).map(|i| self.0[i])
    }

    /// Validates `value` against the attribute's measured rule, storing it and
    /// returning `true` on success.
    ///
    /// A rejected value leaves the previous one in place, which is what
    /// msodbcsql does — a failed set is not a reset. Callers are expected to
    /// have checked [`Self::is_settable`]; anything else is reported as
    /// rejected rather than silently accepted.
    pub(crate) fn set(&mut self, attribute: SqlInteger, value: SqlULen) -> bool {
        let Some(i) = Self::index_of(attribute) else {
            return false;
        };
        let accepted = match VENDOR_STMT_ATTRS[i].2 {
            ValueRule::OneOf(allowed) => allowed.contains(&value),
            ValueRule::Range(lo, hi) => (lo..=hi).contains(&value),
            ValueRule::Rejected | ValueRule::GetOnly => false,
        };
        if accepted {
            self.0[i] = value;
        }
        accepted
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

    /// Makes `metadata` the current result set, restarting the
    /// `SQL_ATTR_MAX_ROWS` budget and advancing the batch command ordinal.
    ///
    /// The cap is per result set, so every path that advances onto a new one
    /// must come through here; assigning `column_metadata` directly would leave
    /// a spent budget in place and truncate the next result set.
    ///
    /// This is the *advance* form, for `SQLMoreResults`. The first result set of
    /// a new execution goes through [`Self::begin_batch`] instead, so that the
    /// command ordinal restarts rather than climbing across executions.
    pub(crate) fn begin_result_set(&mut self, metadata: Vec<ColumnMetadata>) {
        self.column_metadata = metadata;
        self.rows_returned = 0;
        self.current_command += 1;
    }

    /// Makes `metadata` the first result set of a new execution.
    ///
    /// Identical to [`Self::begin_result_set`] except that the batch command
    /// ordinal restarts, so `SQL_SOPT_SS_CURRENT_COMMAND` answers 1 on the
    /// first result set of every execution. msodbcsql restarts it per execute
    /// and holds the final value across `SQLCloseCursor` /
    /// `SQLFreeStmt(SQL_CLOSE)`, so a statement handle reused for a second
    /// query reports 1 again rather than continuing to climb.
    pub(crate) fn begin_batch(&mut self, metadata: Vec<ColumnMetadata>) {
        self.current_command = 0;
        self.begin_result_set(metadata);
    }

    /// Whether `SQL_ATTR_MAX_ROWS` has already been satisfied for the current
    /// result set, meaning the cursor must report end-of-data without pulling
    /// another row.
    pub(crate) fn max_rows_reached(&self) -> bool {
        self.max_rows != 0 && self.rows_returned >= self.max_rows
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
    /// `query_timeout` is the parent connection's current
    /// [`DbcState::stmt_query_timeout`](crate::handles::dbc::DbcState); a
    /// statement starts at the connection-level default rather than always at
    /// zero (msodbcsql `sqlcfunc.cpp:173`).
    pub(crate) fn new(parent_dbc: *mut c_void, query_timeout: u32) -> Self {
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
                state_flags: 0,
                query_timeout,
                max_rows: 0,
                rows_returned: 0,
                inert_attrs: InertStmtAttrs::default(),
                vendor_attrs: VendorStmtAttrs::default(),
                qn_msgtext: String::new(),
                qn_options: String::new(),
                current_command: 0,
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
