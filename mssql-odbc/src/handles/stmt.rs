// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use tracing::error;

use mssql_tds::connection::tds_client::{PreparedStatement, StatementId, TdsClient};
use mssql_tds::error::{Error as TdsError, SqlInfoMessage};

use super::desc::{DescHandle, DescKind, DescRecord, DescState};
use super::{DbcHandle, HandleType, HasObjectType, free_handle, handle_to_raw};
use crate::api::odbc_types::{
    self, SQL_DESC_ALLOC_AUTO, SqlInteger, SqlLen, SqlPointer, SqlSmallInt, SqlULen, SqlUSmallInt,
};
use crate::api::set_desc_field::datetime_interval_code_for;
use crate::error::{DiagRecord, HasDiagnostics};
use crate::params::BoundParam;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::encoding_rs::Decoder;
use mssql_tds::query::metadata::{ColumnMetadata, PlpEncoding};

/// State for a PLP column being streamed across repeated SQLGetData calls.
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
    /// Incremental decoder for the narrow-text -> `SQL_C_WCHAR` widening path
    /// (`varchar(max)`/`json` delivered as UTF-16LE). `None` for every other
    /// combination.
    ///
    /// A decoder rather than a byte carry because the column's codepage can be
    /// multi-byte (`lcid_to_encoding` reaches SHIFT_JIS, GBK, BIG5, EUC-KR and
    /// UTF-8), so a chunk boundary can split one character across two reads.
    /// `encoding_rs::Decoder` already holds that partial sequence internally,
    /// which keeps the boundary rule in one place instead of one per codepage.
    pub(crate) narrow_to_wide: Option<Decoder>,
    /// Code units already decoded on a previous call that did not fit the
    /// caller's buffer, delivered before any further wire bytes.
    ///
    /// The decoder writes into a scratch area sized for its own needs rather
    /// than the caller's, because some decoders refuse to emit anything with
    /// less than two units of room (`encoding_rs::GBK` returns `OutputFull`
    /// having consumed nothing). Holding the surplus here lets a caller ask for
    /// one character at a time without stalling the stream.
    pub(crate) pending_units: Vec<u16>,
}

impl ActivePlpStream {
    /// Opens a stream for `column`. Every carry field starts empty, so a call
    /// site names only what identifies the stream — and a carry field added
    /// later cannot break an initializer written in parallel.
    pub(crate) fn new(
        column: usize,
        encoding: PlpEncoding,
        narrow_to_wide: Option<Decoder>,
    ) -> Self {
        Self {
            column,
            encoding,
            pending_byte: None,
            pending_high_surrogate: None,
            narrow_to_wide,
            pending_units: Vec::new(),
        }
    }
}

impl std::fmt::Debug for ActivePlpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `encoding_rs::Decoder` is not Debug; report whether one is active.
        f.debug_struct("ActivePlpStream")
            .field("column", &self.column)
            .field("encoding", &self.encoding)
            .field("pending_byte", &self.pending_byte)
            .field("pending_high_surrogate", &self.pending_high_surrogate)
            .field("narrow_to_wide", &self.narrow_to_wide.is_some())
            .field("pending_units", &self.pending_units.len())
            .finish()
    }
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
    /// Receives the NULL indicator for each row, or null if the application
    /// does not want one. Independent of `octet_length_ptr` per the ODBC
    /// "Deferred Fields" spec, though `SQLBindCol` writes the same pointer to
    /// both — see [`Self::write_to_record`].
    pub(crate) strlen_or_ind_ptr: *mut SqlLen,
    /// Receives the returned data's length for each row, or null if the
    /// application does not want length information for this column
    /// (`SQL_DESC_OCTET_LENGTH_PTR`).
    pub(crate) octet_length_ptr: *mut SqlLen,
}

impl ColumnBinding {
    /// Writes this binding's fields into `record`, the ARD shape `SQLBindCol`
    /// leaves behind (AB#47437: the ARD is the storage `SQLBindCol` and
    /// `SQLSetDescFieldW` share, not a separate copy). `SQL_DESC_INDICATOR_PTR`
    /// and `SQL_DESC_OCTET_LENGTH_PTR` both receive the same pointer here —
    /// `SQLBindCol`'s one `StrLen_or_Ind` argument feeds both descriptor
    /// fields at once — but they stay two independent fields on the record,
    /// since `SQLSetDescFieldW`/`SQLSetDescRec` can set them to different
    /// buffers.
    ///
    /// Does not reset `SQL_DESC_LENGTH`/`PRECISION`/`SCALE`; msodbcsql's
    /// `SetADRec` resets them to `SetTypeDefaults`'s C-type-keyed defaults on
    /// every `SQLBindCol` call — tracked as a known, narrow
    /// (metadata-introspection-only) gap in
    /// [#470](https://github.com/microsoft/mssql-rs/issues/470).
    pub(crate) fn write_to_record(&self, record: &mut DescRecord) {
        record.concise_type = self.target_type;
        record.datetime_interval_code = datetime_interval_code_for(self.target_type);
        record.data_ptr = self.target_value_ptr;
        record.octet_length = self.buffer_length;
        record.indicator_ptr = self.strlen_or_ind_ptr as SqlPointer;
        record.octet_length_ptr = self.octet_length_ptr as SqlPointer;
    }

    /// Reconstructs the binding an ARD record represents, or `None` when
    /// `SQL_DESC_DATA_PTR` is null — this driver's convention (mirroring
    /// `SQLBindCol`'s own null-pointer-unbinds rule) for "no binding here",
    /// whether the record was never touched, was explicitly unbound, or was
    /// grown by a `SQL_DESC_COUNT` write that never supplied a data pointer.
    pub(crate) fn from_record(column_number: SqlUSmallInt, record: &DescRecord) -> Option<Self> {
        if record.data_ptr.is_null() {
            return None;
        }
        Some(Self {
            column_number,
            target_type: record.concise_type,
            target_value_ptr: record.data_ptr,
            buffer_length: record.octet_length,
            strlen_or_ind_ptr: record.indicator_ptr as *mut SqlLen,
            octet_length_ptr: record.octet_length_ptr as *mut SqlLen,
        })
    }

    /// Every currently-bound column in `ard_state`'s records, in column-number
    /// order — the fetch loop's input, derived fresh from the ARD each fetch
    /// rather than cached, so a descriptor-field bind and a `SQLBindCol` bind
    /// are indistinguishable to it.
    pub(crate) fn all_from_ard_state(ard_state: &DescState) -> Vec<Self> {
        ard_state
            .records
            .iter()
            .enumerate()
            .filter_map(|(i, record)| {
                let column_number = SqlUSmallInt::try_from(i + 1).ok()?;
                Self::from_record(column_number, record)
            })
            .collect()
    }
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
    /// Set once a fetch has confirmed — possibly by peeking one token past
    /// the row it just delivered — that no further rows exist for the
    /// current cursor. Distinct from `STMT_STATE_CURSOR_OPEN`, which stays
    /// set until an explicit close: this only means a later
    /// `SQLFetch`/`SQLFetchScroll` can report `SQL_NO_DATA` without needing
    /// the connection (another statement may have claimed it in the
    /// meantime — the answer is already known and needs no more wire access).
    /// Reset by [`StmtState::clear_exhaustion_state`] whenever a fresh
    /// execute positions a new result (see also `SQLMoreResults`'s
    /// `Rows`/`NoRows` arms, which reset it individually while landing on a
    /// further result within the same batch).
    pub(crate) result_set_exhausted: bool,
    /// Set alongside `result_set_exhausted`, but only when the wire has
    /// confirmed nothing remains anywhere in the batch — i.e. exactly when
    /// `release_busy_if_row_exhausted` (`exec_common.rs`) actually released
    /// the busy claim, not merely when the *current* result set ran out.
    /// `result_set_exhausted` alone is insufficient here: it is also set when
    /// a further result set is still pending (`DONE` carrying `MORE`), a case
    /// where this statement still owns the connection and `SQLMoreResults`
    /// must genuinely advance rather than fast-path.
    ///
    /// Lets `SQLMoreResults` report `SQL_NO_DATA` without touching the
    /// connection at all once this is `true` — matching msodbcsql, whose
    /// `SQLMoreResults` has no busy check of its own (`GetBatchCtxOrRecover`
    /// just falls through to `SQL_NO_DATA_FOUND` once the batch context is
    /// gone) and so is never blocked by a different statement that has since
    /// claimed the connection. Reset by [`StmtState::clear_exhaustion_state`]
    /// alongside `result_set_exhausted`.
    pub(crate) batch_exhausted: bool,
    /// A SQL Server error a read-ahead peek discovered past a row this
    /// statement had already finished delivering to the caller (see
    /// `release_busy_if_row_exhausted` in `exec_common.rs`). The call that
    /// found it has already committed to a success return for the row it
    /// delivered, so posting the diagnostic there would be silently lost —
    /// no return code tells the caller to look. Stashed here instead, for
    /// the next call that would otherwise short-circuit past the wire —
    /// `SQLFetch`'s `result_set_exhausted` fast path, `SQLMoreResults`, or a
    /// cursor close (`SQLCloseCursor` / `SQLFreeStmt(SQL_CLOSE)`, whose own
    /// drain can no longer discover it on the wire because the peek already
    /// consumed the terminating ERROR token) — to drain and fail on. The
    /// connection-scoped close sweep (`SQLEndTran`/autocommit/isolation
    /// change, `close_cursor_for_connection_op`) is the one exception: it
    /// still posts this to the statement's diagnostics but does not fail its
    /// own return, since that return code specifically means "the stream
    /// failed to drain," which a stale diagnostic on an already-closed batch
    /// does not make true. Cleared by [`StmtState::clear_exhaustion_state`],
    /// since both it and `result_set_exhausted` describe facts about the
    /// same now-superseded result set.
    pub(crate) pending_fetch_error: Option<TdsError>,
    /// Server INFO messages a read-ahead peek drained from the client when
    /// `release_busy_if_row_exhausted` released the busy claim on a zero-row
    /// fetch (`row_delivered == false`). `fill_rowset`'s own `SQL_NO_DATA`
    /// can't carry `SQL_SUCCESS_WITH_INFO`, so these are stashed here instead
    /// of posted immediately — for `SQLMoreResults`'s `batch_exhausted` fast
    /// path or a cursor close to surface, exactly as the deferred-error
    /// twin above. Both fast paths release the connection without the
    /// caller re-touching the wire, so nothing else would ever drain them.
    /// Cleared by [`StmtState::clear_exhaustion_state`] alongside
    /// `batch_exhausted`.
    pub(crate) pending_fetch_info: Vec<SqlInfoMessage>,
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
    /// `SQL_ATTR_QUERY_TIMEOUT` in seconds; `0` (the ODBC default) means no
    /// timeout. Seeded at allocation from the parent connection's
    /// [`DbcState::stmt_query_timeout`].
    ///
    /// Enforced against a running query (AB#46385): threaded into
    /// [`ExecuteOptions`](mssql_tds::connection::tds_client::ExecuteOptions)
    /// for every `execute*` call, so a non-zero value bounds the wait and
    /// surfaces `HYT00` on expiry, matching msodbcsql.
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

    /// Returns whether `attribute` belongs to this store without changing it.
    pub(crate) fn contains(&self, attribute: SqlInteger) -> bool {
        Self::index_of(attribute).is_some()
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
    /// When set, the pointer must address a live `SQLLEN` for the duration of
    /// the execution, per the `SQLSetStmtAttr` contract. ODBC does not require
    /// application pointers to be aligned.
    pub(crate) unsafe fn param_bind_offset(&self) -> isize {
        let ptr = self
            .get(odbc_types::SQL_ATTR_PARAM_BIND_OFFSET_PTR)
            .unwrap_or(0) as *const odbc_types::SqlLen;
        if ptr.is_null() {
            return 0;
        }
        unsafe { ptr.read_unaligned() }
    }
}

/// One data-at-execution parameter: the token `SQLParamData` returns, how
/// many bytes the application promised for it, and the C/SQL type pairing
/// `dae_placeholder_type` resolved at execute time.
///
/// Keeping these fields together means the execution-time token and declared
/// length cannot drift away from the binding they describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DaeParam {
    /// `ParameterValuePtr` with the execution's bind offset already applied.
    pub(crate) value_ptr: SqlPointer,
    /// Total byte count declared by `SQL_LEN_DATA_AT_EXEC(n)`; `None` for
    /// `SQL_DATA_AT_EXEC`, where the application promised no total.
    pub(crate) expected_len: Option<usize>,
    /// Set from `dae_placeholder_type`'s report at the same time the streamed
    /// wire type was decided. `SQLPutData` buffers this parameter's chunks in
    /// [`DaeProgress::pending_bytes`] instead of writing them to the wire, and
    /// they are transcoded once, as a whole, when the parameter closes.
    pub(crate) needs_transcode: bool,
    /// `BoundParam::c_type` / `BoundParam::sql_type`, snapshotted here at the
    /// same time as `needs_transcode` rather than re-read from
    /// `StmtState::bound_params` at close time. `SQLFreeStmt(SQL_RESET_PARAMS)`
    /// can clear a binding while its data-at-execution sequence is still open,
    /// and a rebind could change the encoding after the wire placeholder was
    /// already fixed; the snapshot can neither vanish nor drift underneath the
    /// sequence it describes. This is the *only* place the sequence reads
    /// either type from -- no consumer indexes back into `bound_params`, so
    /// there is one source of truth for the whole sequence, not two that
    /// could disagree.
    pub(crate) c_type: SqlSmallInt,
    pub(crate) sql_type: SqlSmallInt,
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
    /// Raw `SQLPutData` bytes accumulated so far, in the C type's own
    /// encoding. Only appended to -- instead of being streamed to the wire
    /// immediately -- when [`DaeParam::needs_transcode`] is set; empty and
    /// unused otherwise.
    pub(crate) pending_bytes: Vec<u8>,
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
        // Zero is rejected, unlike most ODBC timeouts where it means "no limit",
        // and the ceiling is i32::MAX rather than the full SQLULEN width:
        // msodbcsql answers HY024 from 2^31 upwards on 64-bit.
        ValueRule::Range(1, i32::MAX as SqlULen),
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

    /// Clears everything AB#47508's read-ahead peek can leave behind, so a
    /// fresh result set never inherits a previous one's exhaustion state or
    /// deferred diagnostics. Called from every `finish_execute` terminal
    /// branch and `close_cursor.rs`'s `reset_cursor_state` — folded into one
    /// method so the invariant lives in a single place rather than four
    /// call sites that could each independently drift or be missed.
    pub(crate) fn clear_exhaustion_state(&mut self) {
        self.result_set_exhausted = false;
        self.batch_exhausted = false;
        self.pending_fetch_error = None;
        self.pending_fetch_info.clear();
    }

    /// The statement's currently *effective* ARD: the explicit descriptor
    /// associated via `SQLSetStmtAttrW(SQL_ATTR_APP_ROW_DESC, ...)`, or the
    /// permanent implicit `stmt.ard` when none is associated. This is the
    /// descriptor `SQLBindCol` writes into and `SQLFetchScroll` reads from
    /// (AB#47437) — the same resolution `SQLGetStmtAttrW` already reports
    /// back to the application (`set_stmt_attr.rs`).
    pub(crate) fn effective_ard(&self, stmt: &StmtHandle) -> *mut c_void {
        self.active_ard.unwrap_or(stmt.ard)
    }

    /// The statement's currently *effective* APD. See [`Self::effective_ard`].
    pub(crate) fn effective_apd(&self, stmt: &StmtHandle) -> *mut c_void {
        self.active_apd.unwrap_or(stmt.apd)
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
    /// to supply. Null when no parameter is open.
    pub(crate) fn dae_current_value_ptr(&self) -> SqlPointer {
        self.dae
            .as_ref()
            .and_then(DaeState::current_param)
            .map_or(std::ptr::null_mut(), |param| param.value_ptr)
    }

    /// The C type of the open DAE parameter, which `SQLPutData` needs to size
    /// an `SQL_NTS` chunk. Reads [`DaeParam::c_type`] -- the snapshot taken at
    /// execute time -- rather than `bound_params`, so it agrees with the type
    /// `SQLParamData` transcodes with even if `SQLFreeStmt(SQL_RESET_PARAMS)`
    /// or a rebind changes or clears the live binding while the sequence is
    /// open.
    pub(crate) fn dae_current_c_type(&self) -> Option<SqlSmallInt> {
        self.dae.as_ref()?.current_param().map(|param| param.c_type)
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
                result_set_exhausted: false,
                batch_exhausted: false,
                pending_fetch_error: None,
                pending_fetch_info: Vec::new(),
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
                active_ard: None,
                active_apd: None,
                state_flags: 0,
                dae: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_CHAR, SQL_C_SLONG};
    use crate::handles::desc::{DescHeader, DescKind};

    fn binding(column_number: SqlUSmallInt, target_type: SqlSmallInt) -> ColumnBinding {
        ColumnBinding {
            column_number,
            target_type,
            // A non-null sentinel: `data_ptr.is_null()` is this driver's
            // "unbound" signal (`ColumnBinding::from_record`), so a genuine
            // binding under test must use a non-null value here.
            target_value_ptr: 0x1 as SqlPointer,
            buffer_length: 0,
            strlen_or_ind_ptr: std::ptr::null_mut(),
            octet_length_ptr: std::ptr::null_mut(),
        }
    }

    /// Runs `f` against a fresh, empty ARD-shaped `DescState`.
    fn with_ard_state(f: impl FnOnce(&mut DescState)) {
        let mut state = DescState {
            diag_records: Vec::new(),
            header: DescHeader::default(),
            records: Vec::new(),
        };
        f(&mut state);
    }

    /// Grows `state` to `column_number` records (if needed) and writes
    /// `binding` into the one at that position — the same two-step shape
    /// `sql_bind_col_safe` uses.
    fn bind(state: &mut DescState, binding: ColumnBinding) {
        let column_number = binding.column_number;
        let target_count = state.records.len().max(usize::from(column_number));
        state.set_record_count(target_count, DescKind::AppRow);
        let record = state
            .record_mut(SqlSmallInt::try_from(column_number).unwrap())
            .expect("just grew the record list to include this column_number");
        binding.write_to_record(record);
    }

    /// A column can only be bound once, so rebinding overwrites the same
    /// record in place rather than adding a second one — otherwise the fetch
    /// loop would write the column twice, once through a stale pointer.
    #[test]
    fn rebinding_a_column_replaces_its_entry() {
        with_ard_state(|s| {
            bind(s, binding(1, SQL_C_SLONG));
            bind(s, binding(2, SQL_C_SLONG));
            bind(s, binding(1, SQL_C_CHAR));

            let bindings = ColumnBinding::all_from_ard_state(s);
            assert_eq!(bindings.len(), 2);
            let first = bindings.iter().find(|b| b.column_number == 1).unwrap();
            assert_eq!(first.target_type, SQL_C_CHAR);
        });
    }

    /// Unbinding one column (a null `data_ptr`, `SQLBindCol`'s own
    /// null-pointer-unbinds convention) leaves the others in place.
    #[test]
    fn clearing_one_binding_leaves_the_others() {
        with_ard_state(|s| {
            bind(s, binding(1, SQL_C_SLONG));
            bind(s, binding(2, SQL_C_SLONG));
            s.record_mut(1).unwrap().data_ptr = std::ptr::null_mut();

            let bindings = ColumnBinding::all_from_ard_state(s);
            assert_eq!(bindings.len(), 1);
            assert_eq!(bindings[0].column_number, 2);
            // Unbinding a column that was never bound (no record yet) is a
            // no-op, not a panic — record_mut(99) is simply None.
            assert!(s.record_mut(99).is_none());
        });
    }

    /// SQLFreeStmt(SQL_UNBIND) drops every binding by nulling every record's
    /// `data_ptr`.
    #[test]
    fn clearing_all_bindings_empties_the_table() {
        with_ard_state(|s| {
            bind(s, binding(1, SQL_C_SLONG));
            bind(s, binding(2, SQL_C_SLONG));
            for record in &mut s.records {
                record.data_ptr = std::ptr::null_mut();
            }
            assert!(ColumnBinding::all_from_ard_state(s).is_empty());
        });
    }

    /// The fill loop reads a forward-only cursor, so it depends on this order
    /// rather than re-establishing it; an application may bind in any order —
    /// guaranteed here by `all_from_ard_state` walking records by position,
    /// not by insertion order.
    #[test]
    fn bindings_stay_ordered_by_column_however_they_were_bound() {
        with_ard_state(|s| {
            for col in [5, 1, 3, 2, 4] {
                bind(s, binding(col, SQL_C_SLONG));
            }
            let cols: Vec<_> = ColumnBinding::all_from_ard_state(s)
                .iter()
                .map(|b| b.column_number)
                .collect();
            assert_eq!(cols, vec![1, 2, 3, 4, 5]);

            // A rebind replaces in place and keeps the order.
            bind(s, binding(3, SQL_C_CHAR));
            let bindings = ColumnBinding::all_from_ard_state(s);
            let cols: Vec<_> = bindings.iter().map(|b| b.column_number).collect();
            assert_eq!(cols, vec![1, 2, 3, 4, 5]);
            assert_eq!(bindings[2].target_type, SQL_C_CHAR);
        });
    }

    /// A fresh ARD has no records at all, which is what makes an unbound
    /// fetch legal.
    #[test]
    fn a_fresh_statement_has_no_bindings() {
        with_ard_state(|s| {
            assert!(ColumnBinding::all_from_ard_state(s).is_empty());
        });
        let handle = StmtHandle::new(std::ptr::null_mut(), 0);
        assert!(handle.inner.lock().unwrap().row_bind_offset_ptr.is_null());
    }
}
