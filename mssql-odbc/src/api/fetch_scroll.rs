// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLFetchScroll: block fetch of a rowset into the columns
//! bound by `SQLBindCol`.
//!
//! This is the columnar path `mssql-python` uses for `fetchmany` / `fetchall`:
//! `SQL_ATTR_ROW_ARRAY_SIZE` rows are pulled in one call and written into the
//! application's per-column arrays, with `*rows_fetched_ptr` reporting how many
//! arrived and the row status array reporting each row's outcome.
//!
//! Only `SQL_FETCH_NEXT` is served — the cursor is forward-only — and only
//! column-wise binding, which is the ODBC default and what `mssql-python` uses.
//!
//! Values are converted by the same core `SQLGetData` uses, so a column reads
//! the same either way. The difference is cadence: `SQLGetData` may return a
//! long value in chunks across repeated calls, whereas a bound column gets one
//! shot at a fixed-size buffer and reports `01004` if the value does not fit.

use std::borrow::Cow;

use tracing::{debug, error};

use mssql_tds::connection::tds_client::{BufferedRowPoll, CursorColumn, CursorPoll, ResultSet};
use mssql_tds::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::row_writer::RowWriter;
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sql_vector::SqlVector;
use mssql_tds::datatypes::sqldatatypes::TdsDataType;
use mssql_tds::error::Error as TdsError;
use mssql_tds::query::metadata::PlpEncoding;
use uuid::Uuid;

use super::sqlstate::*;
use crate::api::exec_common::release_busy_if_row_exhausted;
use crate::api::get_data::{
    TextError, column_value_to_text, convert_typed_c, is_typed_c_target, utf16le_chunk_to_utf8,
};
use crate::api::odbc_types::{
    SQL_BIND_BY_COLUMN, SQL_C_BIT, SQL_C_CHAR, SQL_C_DOUBLE, SQL_C_FLOAT, SQL_C_GUID,
    SQL_C_SBIGINT, SQL_C_SLONG, SQL_C_SS_TIME2, SQL_C_SS_TIMESTAMPOFFSET, SQL_C_SSHORT,
    SQL_C_STINYINT, SQL_C_TINYINT, SQL_C_TYPE_DATE, SQL_C_TYPE_TIME, SQL_C_TYPE_TIMESTAMP,
    SQL_C_UBIGINT, SQL_C_ULONG, SQL_C_USHORT, SQL_C_UTINYINT, SQL_C_WCHAR, SQL_ERROR,
    SQL_FETCH_NEXT, SQL_INVALID_HANDLE, SQL_NO_DATA, SQL_NO_TOTAL, SQL_NULL_DATA, SQL_ROW_ERROR,
    SQL_ROW_NOROW, SQL_ROW_SUCCESS, SQL_ROW_SUCCESS_WITH_INFO, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO,
    SqlDateStruct, SqlGuid, SqlHandle, SqlLen, SqlPointer, SqlReturn, SqlSmallInt,
    SqlSsTime2Struct, SqlSsTimestampoffsetStruct, SqlTimestampStruct, SqlULen, SqlUSmallInt,
    SqlWChar,
};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::conversion::error::{ConvError, ConvOk};
use crate::conversion::fetch_convert::{
    DateTimeParts, date_parts, datetime2_parts, datetimeoffset_parts, time_parts,
};
use crate::error::{free_errors, post_sql_error};
use crate::handles::stmt::{
    BufferedGetDataRow, ColumnBinding, STMT_STATE_CURSOR_OPEN, STMT_STATE_FETCH_IN_PROGRESS,
    StmtState,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

impl BufferedGetDataRow {
    fn empty(reusable: &mut Option<Self>, column_count: usize) -> Self {
        let mut row = reusable.take().unwrap_or_else(|| Self {
            values: Vec::new(),
            variant_bases: Vec::new(),
            consumed: 0,
            wire_deferred: false,
        });
        if row.values.len() != column_count {
            row.values.clear();
            row.values.resize_with(column_count, || None);
        }
        if row.variant_bases.len() != column_count {
            row.variant_bases.clear();
            row.variant_bases.resize(column_count, None);
        } else {
            row.variant_bases.fill(None);
        }
        row.consumed = 0;
        row.wire_deferred = false;
        row
    }

    pub(crate) fn discard_before(&mut self, col: usize) {
        let end = col.min(self.values.len());
        for value in self.values.iter_mut().take(end).skip(self.consumed) {
            *value = None;
        }
        self.consumed = self.consumed.max(end);
    }

    fn write_value(&mut self, col: usize, value: ColumnValues) {
        if let Some(slot) = self.values.get_mut(col) {
            *slot = Some(value);
        }
    }

    fn defer_from(&mut self, col: usize) {
        for slot in self.values.iter_mut().skip(col) {
            *slot = None;
        }
        for slot in self.variant_bases.iter_mut().skip(col) {
            *slot = None;
        }
    }
}

impl RowWriter for BufferedGetDataRow {
    fn write_null(&mut self, col: usize) {
        self.write_value(col, ColumnValues::Null);
    }

    fn write_variant_base_type(&mut self, col: usize, base: TdsDataType) {
        if let Some(slot) = self.variant_bases.get_mut(col) {
            *slot = Some(base);
        }
    }

    fn write_bool(&mut self, col: usize, val: bool) {
        self.write_value(col, ColumnValues::Bit(val));
    }

    fn write_u8(&mut self, col: usize, val: u8) {
        self.write_value(col, ColumnValues::TinyInt(val));
    }

    fn write_i16(&mut self, col: usize, val: i16) {
        self.write_value(col, ColumnValues::SmallInt(val));
    }

    fn write_i32(&mut self, col: usize, val: i32) {
        self.write_value(col, ColumnValues::Int(val));
    }

    fn write_i64(&mut self, col: usize, val: i64) {
        self.write_value(col, ColumnValues::BigInt(val));
    }

    fn write_f32(&mut self, col: usize, val: f32) {
        self.write_value(col, ColumnValues::Real(val));
    }

    fn write_f64(&mut self, col: usize, val: f64) {
        self.write_value(col, ColumnValues::Float(val));
    }

    fn write_string(&mut self, col: usize, bytes: Cow<'_, [u8]>, encoding_type: EncodingType) {
        if let Some(Some(ColumnValues::String(existing))) = self.values.get_mut(col)
            && existing.encoding_type() == &encoding_type
        {
            existing.bytes.clear();
            existing.bytes.extend_from_slice(&bytes);
            return;
        }
        self.write_value(
            col,
            ColumnValues::String(SqlString::new(bytes.into_owned(), encoding_type)),
        );
    }

    fn write_bytes(&mut self, col: usize, bytes: Cow<'_, [u8]>) {
        self.write_value(col, ColumnValues::Bytes(bytes.into_owned()));
    }

    fn write_decimal(&mut self, col: usize, val: DecimalParts) {
        self.write_value(col, ColumnValues::Decimal(val));
    }

    fn write_numeric(&mut self, col: usize, val: DecimalParts) {
        self.write_value(col, ColumnValues::Numeric(val));
    }

    fn write_date(&mut self, col: usize, val: SqlDate) {
        self.write_value(col, ColumnValues::Date(val));
    }

    fn write_time(&mut self, col: usize, val: SqlTime) {
        self.write_value(col, ColumnValues::Time(val));
    }

    fn write_datetime(&mut self, col: usize, val: SqlDateTime) {
        self.write_value(col, ColumnValues::DateTime(val));
    }

    fn write_smalldatetime(&mut self, col: usize, val: SqlSmallDateTime) {
        self.write_value(col, ColumnValues::SmallDateTime(val));
    }

    fn write_datetime2(&mut self, col: usize, val: SqlDateTime2) {
        self.write_value(col, ColumnValues::DateTime2(val));
    }

    fn write_datetimeoffset(&mut self, col: usize, val: SqlDateTimeOffset) {
        self.write_value(col, ColumnValues::DateTimeOffset(val));
    }

    fn write_money(&mut self, col: usize, val: SqlMoney) {
        self.write_value(col, ColumnValues::Money(val));
    }

    fn write_smallmoney(&mut self, col: usize, val: SqlSmallMoney) {
        self.write_value(col, ColumnValues::SmallMoney(val));
    }

    fn write_uuid(&mut self, col: usize, val: Uuid) {
        self.write_value(col, ColumnValues::Uuid(val));
    }

    fn write_xml(&mut self, col: usize, val: SqlXml) {
        self.write_value(col, ColumnValues::Xml(val));
    }

    fn write_json(&mut self, col: usize, val: SqlJson) {
        self.write_value(col, ColumnValues::Json(val));
    }

    fn write_vector(&mut self, col: usize, val: SqlVector) {
        self.write_value(col, ColumnValues::Vector(val));
    }

    fn end_row(&mut self) {}
}

/// Implements SQLFetchScroll for the current forward-only result set.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null.
pub(crate) unsafe fn sql_fetch_scroll(
    statement_handle: SqlHandle,
    fetch_orientation: SqlSmallInt,
    fetch_offset: SqlLen,
) -> SqlReturn {
    crate::ffi_entry!("SQLFetchScroll", unsafe {
        sql_fetch_scroll_impl(statement_handle, fetch_orientation, fetch_offset)
    })
}

pub(crate) unsafe fn sql_fetch_scroll_impl(
    statement_handle: SqlHandle,
    fetch_orientation: SqlSmallInt,
    fetch_offset: SqlLen,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLFetchScroll: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }
    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(stmt.object_type, HandleType::Stmt);
    fetch_scroll_safe(statement_handle, stmt, fetch_orientation, fetch_offset)
}

/// Why a bound column write did not land exactly, so the row can report the
/// same SQLSTATE `SQLGetData` would have for the identical value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowIssue {
    /// 01004 — the value did not fit the bound buffer.
    StringTruncated,
    /// 01S07 — fractional digits were dropped to fit the target.
    FractionalTruncated,
    /// 22003 — numeric value out of the target's range.
    OutOfRange,
    /// 07006 — the source type cannot convert to the requested target.
    Restricted,
    /// 22018 — the payload is not a valid literal for the target.
    InvalidCharacter,
    /// 22002 — NULL arrived with no indicator to report it through.
    IndicatorRequired,
    /// HYC00 — a target or source this driver does not deliver yet.
    Unsupported,
}

impl RowIssue {
    fn post(self, stmt_state: &mut StmtState) {
        match self {
            RowIssue::StringTruncated => post_diag(stmt_state, WARN_STRING_TRUNCATION),
            RowIssue::FractionalTruncated => post_diag(stmt_state, WARN_FRACTIONAL_TRUNCATION),
            RowIssue::OutOfRange => post_diag(stmt_state, ERR_NUMERIC_OUT_OF_RANGE),
            RowIssue::Restricted => post_diag(stmt_state, ERR_RESTRICTED_DATA_TYPE),
            RowIssue::InvalidCharacter => post_diag(stmt_state, ERR_INVALID_CHARACTER_VALUE),
            RowIssue::IndicatorRequired => post_diag(stmt_state, ERR_INDICATOR_REQUIRED),
            RowIssue::Unsupported => post_sql_error(
                stmt_state,
                SQLSTATE_HYC00,
                0,
                "Column type conversion not yet implemented",
            ),
        }
    }
}

/// The per-row outcome recorded in the row status array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowOutcome {
    Success,
    Info(RowIssue),
    Error(RowIssue),
}

impl RowOutcome {
    fn status(self) -> SqlUSmallInt {
        match self {
            RowOutcome::Success => SQL_ROW_SUCCESS,
            RowOutcome::Info(_) => SQL_ROW_SUCCESS_WITH_INFO,
            RowOutcome::Error(_) => SQL_ROW_ERROR,
        }
    }

    fn issue(self) -> Option<RowIssue> {
        match self {
            RowOutcome::Success => None,
            RowOutcome::Info(i) | RowOutcome::Error(i) => Some(i),
        }
    }

    /// Keeps the worst outcome seen while filling one row, so a row that both
    /// truncated one column and failed another reports the failure.
    fn merge(self, other: RowOutcome) -> RowOutcome {
        match (self, other) {
            (e @ RowOutcome::Error(_), _) => e,
            (_, e @ RowOutcome::Error(_)) => e,
            (i @ RowOutcome::Info(_), _) => i,
            (_, i @ RowOutcome::Info(_)) => i,
            _ => RowOutcome::Success,
        }
    }
}

/// Writes one decoded TDS row into the application buffers bound by column.
///
/// `next_binding` and `last_column_read` survive a packet-boundary continuation,
/// so the same instance must be passed back when the TDS decoder pauses mid-row.
struct BoundRowWriter<'a> {
    /// Ordered snapshot of the statement's bound columns.
    bindings: &'a [ColumnBinding],
    /// Next binding that may match an incoming wire column.
    next_binding: usize,
    /// Zero-based destination row within each bound column array.
    row_index: usize,
    /// Byte displacement from `SQL_ATTR_ROW_BIND_OFFSET_PTR`.
    bind_offset: usize,
    /// Worst conversion outcome observed for this row.
    outcome: RowOutcome,
    /// Highest bound column ordinal consumed from the row.
    last_column_read: usize,
}

// The ODBC call owns the bound buffers for the duration of the synchronous
// fetch. Moving its decode future between runtime workers cannot make the
// pointers concurrently accessible.
unsafe impl Send for BoundRowWriter<'_> {}

impl<'a> BoundRowWriter<'a> {
    /// Starts writing one row at its rowset slot and bind-offset displacement.
    fn new(
        bindings: &'a [ColumnBinding],
        row_index: usize,
        bind_offset: usize,
    ) -> BoundRowWriter<'a> {
        BoundRowWriter {
            bindings,
            next_binding: 0,
            row_index,
            bind_offset,
            outcome: RowOutcome::Success,
            last_column_read: 0,
        }
    }

    /// Advances the ordered binding cursor to `col`, returning its binding when
    /// present and skipping unbound columns without losing the wire ordinal.
    fn take_binding(&mut self, col: usize) -> Option<ColumnBinding> {
        let ordinal = col + 1;
        while self
            .bindings
            .get(self.next_binding)
            .is_some_and(|binding| usize::from(binding.column_number) < ordinal)
        {
            self.next_binding += 1;
        }
        let binding = *self.bindings.get(self.next_binding)?;
        if usize::from(binding.column_number) != ordinal {
            return None;
        }
        self.next_binding += 1;
        self.last_column_read = ordinal;
        Some(binding)
    }

    /// Sends a materialized value through the established conversion path.
    fn write_value(&mut self, col: usize, value: ColumnValues) {
        let Some(binding) = self.take_binding(col) else {
            return;
        };
        let delivered =
            unsafe { deliver_bound(&binding, self.row_index, self.bind_offset, &value) };
        self.outcome = self.outcome.merge(delivered);
    }

    /// Writes `value` directly when the bound C type is exact, otherwise lazily
    /// materializes the equivalent `ColumnValues` for normal conversion.
    fn write_exact<T, F>(&mut self, col: usize, target_type: SqlSmallInt, value: T, fallback: F)
    where
        T: Copy,
        F: FnOnce() -> ColumnValues,
    {
        let Some(binding) = self.take_binding(col) else {
            return;
        };
        let delivered = if binding.target_type == target_type {
            unsafe { deliver_fixed_bound(&binding, self.row_index, self.bind_offset, value) }
        } else {
            unsafe { deliver_bound(&binding, self.row_index, self.bind_offset, &fallback()) }
        };
        self.outcome = self.outcome.merge(delivered);
    }

    /// Converts temporal parts directly into the matching ODBC struct, retaining
    /// the normal conversion path for other C targets and range failures.
    fn write_temporal<T, P, C, V>(
        &mut self,
        col: usize,
        target_type: SqlSmallInt,
        parts: P,
        convert: C,
        value: V,
    ) where
        T: Copy,
        P: FnOnce() -> Option<DateTimeParts>,
        C: FnOnce(DateTimeParts) -> T,
        V: FnOnce() -> ColumnValues,
    {
        let Some(binding) = self.take_binding(col) else {
            return;
        };
        let delivered = if binding.target_type == target_type {
            match parts() {
                Some(parts) => unsafe {
                    deliver_fixed_bound(&binding, self.row_index, self.bind_offset, convert(parts))
                },
                None => RowOutcome::Error(RowIssue::Restricted),
            }
        } else {
            unsafe { deliver_bound(&binding, self.row_index, self.bind_offset, &value()) }
        };
        self.outcome = self.outcome.merge(delivered);
    }
}

impl RowWriter for BoundRowWriter<'_> {
    /// Reports NULL through the bound indicator without disturbing fixed-width data.
    fn write_null(&mut self, col: usize) {
        self.write_value(col, ColumnValues::Null);
    }

    /// Writes a `bit` directly when the target is `SQL_C_BIT`.
    fn write_bool(&mut self, col: usize, val: bool) {
        self.write_exact(col, SQL_C_BIT, u8::from(val), || ColumnValues::Bit(val));
    }

    /// Writes a `tinyint` directly when the target is `SQL_C_UTINYINT`.
    fn write_u8(&mut self, col: usize, val: u8) {
        self.write_exact(col, SQL_C_UTINYINT, val, || ColumnValues::TinyInt(val));
    }

    /// Writes a `smallint` directly when the target is `SQL_C_SSHORT`.
    fn write_i16(&mut self, col: usize, val: i16) {
        self.write_exact(col, SQL_C_SSHORT, val, || ColumnValues::SmallInt(val));
    }

    /// Writes an `int` directly when the target is `SQL_C_SLONG`.
    fn write_i32(&mut self, col: usize, val: i32) {
        self.write_exact(col, SQL_C_SLONG, val, || ColumnValues::Int(val));
    }

    /// Writes a `bigint` directly when the target is `SQL_C_SBIGINT`.
    fn write_i64(&mut self, col: usize, val: i64) {
        self.write_exact(col, SQL_C_SBIGINT, val, || ColumnValues::BigInt(val));
    }

    /// Writes a `real` directly when the target is `SQL_C_FLOAT`.
    fn write_f32(&mut self, col: usize, val: f32) {
        self.write_exact(col, SQL_C_FLOAT, val, || ColumnValues::Real(val));
    }

    /// Writes a `float` directly when the target is `SQL_C_DOUBLE`.
    fn write_f64(&mut self, col: usize, val: f64) {
        self.write_exact(col, SQL_C_DOUBLE, val, || ColumnValues::Float(val));
    }

    /// Delivers borrowed wire text directly when its encoding matches the target.
    fn write_string(&mut self, col: usize, bytes: Cow<'_, [u8]>, encoding: EncodingType) {
        let Some(binding) = self.take_binding(col) else {
            return;
        };
        let delivered = unsafe {
            deliver_encoded_string(&binding, self.row_index, self.bind_offset, bytes, encoding)
        };
        self.outcome = self.outcome.merge(delivered);
    }

    /// Materializes binary data for the established conversion path.
    fn write_bytes(&mut self, col: usize, bytes: Cow<'_, [u8]>) {
        self.write_value(col, ColumnValues::Bytes(bytes.into_owned()));
    }

    /// Delivers a decoded `decimal` through the established conversion path.
    fn write_decimal(&mut self, col: usize, val: DecimalParts) {
        self.write_value(col, ColumnValues::Decimal(val));
    }

    /// Delivers a decoded `numeric` through the established conversion path.
    fn write_numeric(&mut self, col: usize, val: DecimalParts) {
        self.write_value(col, ColumnValues::Numeric(val));
    }

    /// Writes a `date` directly into `SQL_DATE_STRUCT` when requested.
    fn write_date(&mut self, col: usize, val: SqlDate) {
        self.write_temporal(
            col,
            SQL_C_TYPE_DATE,
            || Some(date_parts(&val)),
            |parts| SqlDateStruct {
                year: parts.year,
                month: parts.month,
                day: parts.day,
            },
            || ColumnValues::Date(val.clone()),
        );
    }

    /// Writes a `time` directly into `SQL_SS_TIME2_STRUCT` when requested.
    fn write_time(&mut self, col: usize, val: SqlTime) {
        self.write_temporal(
            col,
            SQL_C_SS_TIME2,
            || Some(time_parts(&val)),
            |parts| SqlSsTime2Struct {
                hour: parts.hour,
                minute: parts.minute,
                second: parts.second,
                fraction: parts.fraction_ns,
            },
            || ColumnValues::Time(val.clone()),
        );
    }

    /// Delivers legacy `datetime` through the established temporal converter.
    fn write_datetime(&mut self, col: usize, val: SqlDateTime) {
        self.write_value(col, ColumnValues::DateTime(val));
    }

    /// Delivers `smalldatetime` through the established temporal converter.
    fn write_smalldatetime(&mut self, col: usize, val: SqlSmallDateTime) {
        self.write_value(col, ColumnValues::SmallDateTime(val));
    }

    /// Writes `datetime2` directly into `SQL_TIMESTAMP_STRUCT` when requested.
    fn write_datetime2(&mut self, col: usize, val: SqlDateTime2) {
        self.write_temporal(
            col,
            SQL_C_TYPE_TIMESTAMP,
            || Some(datetime2_parts(&val)),
            |parts| SqlTimestampStruct {
                year: parts.year,
                month: parts.month,
                day: parts.day,
                hour: parts.hour,
                minute: parts.minute,
                second: parts.second,
                fraction: parts.fraction_ns,
            },
            || ColumnValues::DateTime2(val.clone()),
        );
    }

    /// Writes directly into `SQL_SS_TIMESTAMPOFFSET_STRUCT` when requested.
    fn write_datetimeoffset(&mut self, col: usize, val: SqlDateTimeOffset) {
        self.write_temporal(
            col,
            SQL_C_SS_TIMESTAMPOFFSET,
            || datetimeoffset_parts(&val),
            |parts| SqlSsTimestampoffsetStruct {
                year: parts.year,
                month: parts.month,
                day: parts.day,
                hour: parts.hour,
                minute: parts.minute,
                second: parts.second,
                fraction: parts.fraction_ns,
                timezone_hour: parts.tz_hour,
                timezone_minute: parts.tz_minute,
            },
            || ColumnValues::DateTimeOffset(val.clone()),
        );
    }

    /// Delivers `money` through the established numeric conversion path.
    fn write_money(&mut self, col: usize, val: SqlMoney) {
        self.write_value(col, ColumnValues::Money(val));
    }

    /// Delivers `smallmoney` through the established numeric conversion path.
    fn write_smallmoney(&mut self, col: usize, val: SqlSmallMoney) {
        self.write_value(col, ColumnValues::SmallMoney(val));
    }

    /// Writes a GUID directly in the ODBC `SQLGUID` field layout when requested.
    fn write_uuid(&mut self, col: usize, val: Uuid) {
        let (data1, data2, data3, data4) = val.as_fields();
        self.write_exact(
            col,
            SQL_C_GUID,
            SqlGuid {
                data1,
                data2,
                data3,
                data4: *data4,
            },
            || ColumnValues::Uuid(val),
        );
    }

    /// Delivers XML through the established character conversion path.
    fn write_xml(&mut self, col: usize, val: SqlXml) {
        self.write_value(col, ColumnValues::Xml(val));
    }

    /// Delivers JSON through the established character conversion path.
    fn write_json(&mut self, col: usize, val: SqlJson) {
        self.write_value(col, ColumnValues::Json(val));
    }

    /// Delivers vector data through the established character conversion path.
    fn write_vector(&mut self, col: usize, val: SqlVector) {
        self.write_value(col, ColumnValues::Vector(val));
    }

    /// Bound fetches do not separately expose a `sql_variant` base type.
    fn write_variant_base_type(&mut self, _col: usize, _base: TdsDataType) {}

    /// Row completion is accounted for by the surrounding rowset loop.
    fn end_row(&mut self) {}
}

fn fetch_scroll_safe(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    fetch_orientation: SqlSmallInt,
    _fetch_offset: SqlLen,
) -> SqlReturn {
    // Snapshot the rowset controls and the binding table, then release the
    // statement lock: the fill loop below blocks on the network and must not
    // hold it. The application is not allowed to rebind concurrently with a
    // fetch on the same statement, so the snapshot cannot go stale under us.
    let (
        row_array_size,
        bindings,
        rows_fetched_ptr,
        row_status_ptr,
        column_count,
        row_bind_offset_ptr,
        reusable_get_data_row,
        buffer_trailing_utf16_plp,
    ) = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFetchScroll: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);

        if fetch_orientation != SQL_FETCH_NEXT {
            error!(
                fetch_orientation,
                "SQLFetchScroll: only SQL_FETCH_NEXT is supported on a forward-only cursor"
            );
            post_diag(&mut stmt_state, ERR_FETCH_TYPE_OUT_OF_RANGE);
            return SQL_ERROR;
        }
        if !stmt_state.has_state(STMT_STATE_CURSOR_OPEN) {
            error!("SQLFetchScroll: no open cursor on this statement");
            post_diag(&mut stmt_state, ERR_INVALID_CURSOR_STATE);
            return SQL_ERROR;
        }
        if stmt_state.has_state(STMT_STATE_FETCH_IN_PROGRESS) {
            error!("SQLFetchScroll: a fetch is already in progress on this statement");
            post_diag(&mut stmt_state, ERR_FUNCTION_SEQUENCE);
            return SQL_ERROR;
        }
        if stmt_state.row_bind_type != SQL_BIND_BY_COLUMN {
            error!(
                row_bind_type = stmt_state.row_bind_type,
                "SQLFetchScroll: row-wise binding is not implemented"
            );
            post_sql_error(
                &mut stmt_state,
                SQLSTATE_HYC00,
                0,
                "Row-wise binding is not yet implemented",
            );
            return SQL_ERROR;
        }

        // A previous fetch already confirmed (possibly via a peek past the
        // last row it delivered) that this cursor has no more rows. The
        // answer is already known and needs no connection access at all —
        // report it even if another statement currently owns the connection.
        if stmt_state.result_set_exhausted {
            let rows_fetched_ptr = stmt_state.rows_fetched_ptr;
            let row_status_ptr = stmt_state.row_status_ptr;
            let row_array_size = stmt_state.row_array_size;
            stmt_state.reset_row_stream();
            // That same peek can have found a trailing SQL Server error
            // instead of a clean end of set (see
            // `release_busy_if_row_exhausted`): the call that found it had
            // already committed to delivering its own row successfully, so
            // the diagnostic was deferred here, to the call that would have
            // hit it directly without the peek's read-ahead. This branch's
            // `SQL_ERROR` (unlike the sibling `SQL_NO_DATA` below) can carry
            // extra diagnostic records, so any INFO message stashed
            // alongside it (`StmtState::pending_fetch_info`) is surfaced here
            // too — this closes the cursor, so `SQLCloseCursor`/
            // `SQLFreeStmt(SQL_CLOSE)` can no longer reach it afterward.
            let rc = if let Some(e) = stmt_state.pending_fetch_error.take() {
                stmt_state.clear_state(STMT_STATE_CURSOR_OPEN);
                post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
                let pending_info = std::mem::take(&mut stmt_state.pending_fetch_info);
                post_tds_info_messages(&mut stmt_state, &pending_info);
                SQL_ERROR
            } else {
                SQL_NO_DATA
            };
            drop(stmt_state);
            unsafe { write_if_some(rows_fetched_ptr, 0) };
            mark_no_rows(row_status_ptr, 0, row_array_size);
            debug!(?rc, "SQLFetchScroll: result set already known exhausted");
            return rc;
        }

        let bindings: Vec<ColumnBinding> = stmt_state.bindings.clone();
        // The buffers in that snapshot belong to the application, and the fill
        // loop writes through them after this lock is released. Claiming the
        // statement here is what stops a concurrent SQLBindCol from freeing one
        // mid-write; the mutating entry points refuse while this is set.
        stmt_state.set_state(STMT_STATE_FETCH_IN_PROGRESS);
        let reusable_get_data_row = stmt_state.spare_get_data_row.take();
        let buffer_trailing_utf16_plp = stmt_state
            .column_metadata
            .last()
            .and_then(|metadata| metadata.plp_encoding())
            == Some(PlpEncoding::Utf16Text);
        (
            stmt_state.row_array_size,
            bindings,
            stmt_state.rows_fetched_ptr,
            stmt_state.row_status_ptr,
            stmt_state.column_metadata.len(),
            stmt_state.row_bind_offset_ptr,
            reusable_get_data_row,
            buffer_trailing_utf16_plp,
        )
    };

    let rc = fill_rowset(
        statement_handle,
        stmt,
        &bindings,
        row_array_size,
        column_count,
        rows_fetched_ptr,
        row_status_ptr,
        row_bind_offset_ptr,
        reusable_get_data_row,
        buffer_trailing_utf16_plp,
    );

    // Single clearing point for the guard, so every early return inside the
    // fill loop still releases it.
    if let Ok(mut stmt_state) = stmt.inner.lock() {
        stmt_state.clear_state(STMT_STATE_FETCH_IN_PROGRESS);
    }
    debug!(?rc, "SQLFetchScroll returning");
    rc
}

#[allow(clippy::too_many_arguments)]
fn fill_rowset(
    statement_handle: SqlHandle,
    stmt: &StmtHandle,
    bindings: &[ColumnBinding],
    row_array_size: SqlULen,
    column_count: usize,
    rows_fetched_ptr: *mut SqlULen,
    row_status_ptr: *mut SqlUSmallInt,
    row_bind_offset_ptr: *mut SqlULen,
    mut reusable_get_data_row: Option<BufferedGetDataRow>,
    buffer_trailing_utf16_plp: bool,
) -> SqlReturn {
    // The application asked for at most `SQL_ATTR_MAX_ROWS` rows from this
    // result set. Once that many have been returned the cursor stops without
    // pulling another row, matching msodbcsql. The cursor deliberately stays
    // open and the connection stays busy on this statement: the rest of the
    // result set is still on the wire and SQLMoreResults / SQLCloseCursor
    // drain it as usual.
    let row_budget = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLFetchScroll: stmt mutex poisoned checking SQL_ATTR_MAX_ROWS");
            return SQL_ERROR;
        };
        if stmt_state.max_rows_reached() {
            // Measured: past the cap msodbcsql answers 24000 to both
            // SQL_ATTR_ROW_NUMBER and SQLGetData, exactly as it does at the
            // natural end of a result set, so the previous row has to stop
            // being readable here rather than linger.
            stmt_state.reset_row_stream();
            drop(stmt_state);
            unsafe { write_if_some(rows_fetched_ptr, 0) };
            mark_no_rows(row_status_ptr, 0, row_array_size);
            debug!("SQLFetchScroll: SQL_ATTR_MAX_ROWS reached; returning SQL_NO_DATA");
            return SQL_NO_DATA;
        }
        row_budget(
            stmt_state.max_rows,
            stmt_state.rows_returned,
            row_array_size,
        )
    };

    let dbc = stmt.parent_dbc();

    let mut client = {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLFetchScroll: dbc mutex poisoned");
            return SQL_ERROR;
        };

        if let Some(busy_stmt) = dbc_state.active_stmt
            && busy_stmt != statement_handle
        {
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_CONNECTION_BUSY);
            }
            return SQL_ERROR;
        }

        if dbc_state.active_stmt.is_none() {
            // Already drained by an earlier fetch; the cursor stays open until
            // it is explicitly closed, so this is SQL_NO_DATA rather than an
            // error. Report a zero-row rowset so the caller sees the count.
            drop(dbc_state);
            let Ok(mut stmt_state) = stmt.inner.lock() else {
                error!("SQLFetchScroll: stmt mutex poisoned resetting drained cursor");
                return SQL_ERROR;
            };
            stmt_state.reset_row_stream();
            drop(stmt_state);
            unsafe { write_if_some(rows_fetched_ptr, 0) };
            mark_no_rows(row_status_ptr, 0, row_array_size);
            debug!("SQLFetchScroll: cursor already drained; returning SQL_NO_DATA");
            return SQL_NO_DATA;
        }

        let Some(client) = dbc_state.client.take() else {
            drop(dbc_state);
            if let Ok(mut ss) = stmt.inner.lock() {
                post_diag(&mut ss, ERR_NO_ACTIVE_TDS_CLIENT);
            }
            return SQL_ERROR;
        };
        client
    };

    // A no-row statement result (DDL / DML / PRINT) is positioned with zero
    // columns; there is nothing to fetch, so 24000 matches SQLFetch.
    if column_count == 0 {
        error!("SQLFetchScroll: current result has no columns (no-row statement)");
        if let Ok(mut ds) = dbc.inner.lock() {
            ds.client = Some(client);
        }
        if let Ok(mut ss) = stmt.inner.lock() {
            post_diag(&mut ss, ERR_INVALID_CURSOR_STATE);
        }
        return SQL_ERROR;
    }

    let mut rows_filled: SqlULen = 0;
    let mut worst = RowOutcome::Success;
    let mut fetch_error: Option<TdsError> = None;
    let mut last_column_read = 0usize;
    let mut buffered_get_data_row = None;

    // Snapshot the per-column PLP encodings once. Taking the statement lock
    // inside the fill loop would make a poisoned mutex indistinguishable from a
    // column that simply is not PLP, which would silently downgrade a supported
    // column to "unsupported" and drain it.
    let plp_encodings: Option<Vec<Option<PlpEncoding>>> = {
        let Ok(ss) = stmt.inner.lock() else {
            error!("SQLFetchScroll: stmt mutex poisoned reading column metadata");
            if let Ok(mut ds) = dbc.inner.lock() {
                ds.client = Some(client);
            }
            return SQL_ERROR;
        };
        ss.column_metadata
            .iter()
            .any(|metadata| metadata.is_plp())
            .then(|| {
                ss.column_metadata
                    .iter()
                    .map(|metadata| metadata.plp_encoding())
                    .collect()
            })
    };
    // Allocate only if a bound PLP value is actually reached. Fixed rowsets,
    // especially row-array size 1, otherwise paid this 8 KiB zero-fill per fetch.
    let mut plp_scratch = None;

    // Read once per fetch, not once per bind, so an application can move the
    // whole rowset between calls by updating the pointed-to value.
    let bind_offset = unsafe { read_bind_offset(row_bind_offset_ptr) };
    let can_write_complete_rows = bindings
        .last()
        .is_some_and(|binding| binding.column_number as usize == column_count)
        && client.current_result_supports_row_into();
    let can_buffer_get_data_row =
        row_array_size == 1 && bindings.is_empty() && client.current_result_supports_row_into();
    let buffered_prefix_len =
        if row_array_size == 1 && bindings.is_empty() && !can_buffer_get_data_row {
            client.current_result_bufferable_prefix_len()
        } else {
            None
        };

    dispatch_rows(row_budget, || {
        let mut outcome = RowOutcome::Success;
        let mut columns_read = 0usize;
        if can_buffer_get_data_row {
            let mut writer = BufferedGetDataRow::empty(&mut reusable_get_data_row, column_count);
            let result = match client.try_next_buffered_row_into(&mut writer) {
                Ok(BufferedRowPoll::Complete) => Ok(()),
                Ok(BufferedRowPoll::Partial) => {
                    dbc.runtime.block_on(client.finish_row_into(&mut writer))
                }
                Ok(BufferedRowPoll::Exhausted) => return false,
                Ok(BufferedRowPoll::Pending) => {
                    let cursor_poll = client.try_next_row_cursor();
                    match cursor_poll.and_then(|poll| {
                        poll.resolve(|| dbc.runtime.block_on(client.next_row_cursor()))
                    }) {
                        Ok(true) => match client.try_finish_row_into(&mut writer) {
                            Ok(true) => Ok(()),
                            Ok(false) => dbc.runtime.block_on(client.finish_row_into(&mut writer)),
                            Err(error) => Err(error),
                        },
                        Ok(false) => return false,
                        Err(error) => {
                            fetch_error = Some(error);
                            return false;
                        }
                    }
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                fetch_error = Some(error);
                outcome = RowOutcome::Error(RowIssue::Restricted);
            } else {
                buffered_get_data_row = Some(writer);
                columns_read = column_count;
            }
        } else if let Some(prefix_len) = buffered_prefix_len {
            let mut writer = BufferedGetDataRow::empty(&mut reusable_get_data_row, column_count);
            let mut result = match client.try_next_buffered_row_prefix_into(prefix_len, &mut writer)
            {
                Ok(BufferedRowPoll::Complete) => Ok(()),
                Ok(BufferedRowPoll::Partial) => dbc
                    .runtime
                    .block_on(client.finish_row_prefix_into(prefix_len, &mut writer)),
                Ok(BufferedRowPoll::Exhausted) => return false,
                Ok(BufferedRowPoll::Pending) => {
                    let cursor_poll = client.try_next_row_cursor();
                    match cursor_poll.and_then(|poll| {
                        poll.resolve(|| dbc.runtime.block_on(client.next_row_cursor()))
                    }) {
                        Ok(true) => {
                            match client.try_finish_row_prefix_into(prefix_len, &mut writer) {
                                Ok(true) => Ok(()),
                                Ok(false) => dbc.runtime.block_on(
                                    client.finish_row_prefix_into(prefix_len, &mut writer),
                                ),
                                Err(error) => Err(error),
                            }
                        }
                        Ok(false) => return false,
                        Err(error) => {
                            fetch_error = Some(error);
                            return false;
                        }
                    }
                }
                Err(error) => Err(error),
            };
            let mut buffered_columns = prefix_len;
            if result.is_ok()
                && buffer_trailing_utf16_plp
                && prefix_len.checked_add(1) == Some(column_count)
            {
                let mut payload = [0_u8; 256];
                match client.try_read_row_plp_complete(prefix_len, &mut payload) {
                    Ok(CursorPoll::Ready(Some(chunk))) => {
                        writer.write_string(
                            prefix_len,
                            Cow::Borrowed(&payload[..chunk.read]),
                            EncodingType::Utf16,
                        );
                        buffered_columns = column_count;
                    }
                    Ok(CursorPoll::Ready(None)) => {
                        writer.write_null(prefix_len);
                        buffered_columns = column_count;
                    }
                    Ok(CursorPoll::Pending) => {}
                    Err(error) => result = Err(error),
                }
            }
            writer.defer_from(buffered_columns);
            if let Err(error) = result {
                fetch_error = Some(error);
                outcome = RowOutcome::Error(RowIssue::Restricted);
            } else {
                // msodbcsql materializes fixed columns during fetch while
                // UpdateTxtPtrOnly leaves unbound long data deferred
                // (`sqlccurs.cpp`; consumed later by `sqlcdata.cpp::SQLGetData`).
                buffered_get_data_row = Some(writer);
                columns_read = buffered_columns;
            }
        } else if can_write_complete_rows {
            let mut writer = BoundRowWriter::new(bindings, rows_filled as usize, bind_offset);
            let result = match client.try_next_buffered_row_into(&mut writer) {
                Ok(BufferedRowPoll::Complete) => Ok(()),
                Ok(BufferedRowPoll::Partial) => {
                    dbc.runtime.block_on(client.finish_row_into(&mut writer))
                }
                Ok(BufferedRowPoll::Exhausted) => return false,
                Ok(BufferedRowPoll::Pending) => {
                    let cursor_poll = client.try_next_row_cursor();
                    match cursor_poll.and_then(|poll| {
                        poll.resolve(|| dbc.runtime.block_on(client.next_row_cursor()))
                    }) {
                        Ok(true) => match client.try_finish_row_into(&mut writer) {
                            Ok(true) => Ok(()),
                            Ok(false) => dbc.runtime.block_on(client.finish_row_into(&mut writer)),
                            Err(error) => Err(error),
                        },
                        Ok(false) => return false,
                        Err(error) => {
                            fetch_error = Some(error);
                            return false;
                        }
                    }
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                fetch_error = Some(error);
                outcome = outcome.merge(RowOutcome::Error(RowIssue::Restricted));
            } else {
                columns_read = writer.last_column_read;
                outcome = outcome.merge(writer.outcome);
            }
        } else {
            let cursor_poll = client.try_next_row_cursor();
            match cursor_poll
                .and_then(|poll| poll.resolve(|| dbc.runtime.block_on(client.next_row_cursor())))
            {
                Ok(true) => {}
                Ok(false) => return false,
                Err(error) => {
                    fetch_error = Some(error);
                    return false;
                }
            }

            for binding in bindings {
                let column = binding.column_number as usize;
                if column == 0 || column > column_count {
                    // msodbcsql skips a binding whose ordinal is past the end of
                    // this result set and reports nothing -- a binding left over
                    // from a wider one is not an error there, so it is not one
                    // here either.
                    continue;
                }
                let target = column - 1;
                let cursor_poll = client.try_read_row_column(target);
                let pulled = cursor_poll.and_then(|poll| {
                    poll.resolve(|| dbc.runtime.block_on(client.read_row_column(target)))
                });
                columns_read = column;
                let result = match pulled {
                    Ok(CursorColumn::Value { value, .. }) => unsafe {
                        deliver_bound(binding, rows_filled as usize, bind_offset, &value)
                    },
                    Ok(CursorColumn::PlpStreaming { .. }) => {
                        let scratch = plp_scratch.get_or_insert_with(|| vec![0u8; PLP_BOUND_CHUNK]);
                        let delivered = unsafe {
                            deliver_bound_plp(
                                &mut client,
                                &dbc.runtime,
                                binding,
                                rows_filled as usize,
                                bind_offset,
                                plp_encodings
                                    .as_ref()
                                    .and_then(|encodings| encodings.get(column - 1))
                                    .copied()
                                    .flatten(),
                                scratch,
                            )
                        };
                        match delivered {
                            Ok(outcome) => outcome,
                            Err(e) => {
                                fetch_error = Some(e);
                                RowOutcome::Error(RowIssue::Restricted)
                            }
                        }
                    }
                    // Reading ascending and once per column, neither of these is
                    // reachable; treat them as a row error rather than assuming.
                    Ok(CursorColumn::RowEnded) | Ok(CursorColumn::AlreadyConsumed) => {
                        RowOutcome::Error(RowIssue::Restricted)
                    }
                    Err(e) => {
                        fetch_error = Some(e);
                        RowOutcome::Error(RowIssue::Restricted)
                    }
                };
                outcome = outcome.merge(result);
                if fetch_error.is_some() {
                    break;
                }
            }
        }
        last_column_read = columns_read;

        unsafe { write_row_status(row_status_ptr, rows_filled, outcome.status()) };
        worst = worst.merge(outcome);
        rows_filled += 1;

        fetch_error.is_none()
    });

    // A zero-row end of set returns SQL_NO_DATA, which cannot carry
    // SQL_SUCCESS_WITH_INFO, so anything drained here would be posted under a
    // code most applications never inspect and cleared by the next call. Leave
    // those messages on the client for SQLMoreResults or the cursor close to
    // surface, exactly as SQLFetch does.
    let info_messages = if rows_filled > 0 || fetch_error.is_some() {
        client.take_info_messages()
    } else {
        Vec::new()
    };

    // Hand the connection back before touching the statement, mirroring
    // SQLFetch's lock order. A failed protocol read leaves the connection
    // with no usable cursor, so it stops being busy with this statement
    // outright. Otherwise, release the busy claim the moment the wire is
    // actually idle rather than only once this cursor is explicitly closed
    // (matches msodbcsql's wire-state busy gate; see AB#47508) — safe to
    // check now when either the wire already reported end-of-set (the loop
    // broke via a natural `Ok(false)`, which only happens after
    // `next_row_cursor` has itself drained the previous row, so nothing is
    // left unread to protect), or every column of the row just filled was
    // read, so a peek cannot discard a column a following `SQLGetData`
    // could still legitimately retrieve. A block fetch (`row_array_size !=
    // 1`) never leaves a row positioned for `SQLGetData` regardless (see the
    // mixed-access comment below), so it is always safe to check there.
    let peek_is_safe =
        !client.maybe_has_unread_rows() || row_array_size != 1 || last_column_read == column_count;

    if fetch_error.is_some() {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLFetchScroll: dbc mutex poisoned returning client");
            return SQL_ERROR;
        };
        dbc_state.client = Some(client);
        if dbc_state.active_stmt == Some(statement_handle) {
            dbc_state.active_stmt = None;
        }
    } else if peek_is_safe {
        release_busy_if_row_exhausted(dbc, stmt, statement_handle, client, rows_filled > 0);
    } else {
        let Ok(mut dbc_state) = dbc.inner.lock() else {
            error!("SQLFetchScroll: dbc mutex poisoned returning client");
            return SQL_ERROR;
        };
        dbc_state.client = Some(client);
        dbc_state.active_stmt = Some(statement_handle);
    }

    let Ok(mut stmt_state) = stmt.inner.lock() else {
        error!("SQLFetchScroll: stmt mutex poisoned recording rowset");
        return SQL_ERROR;
    };

    // Charged even when the rowset ended in an error: the rows before it were
    // still delivered to the application, so they count against the cap.
    stmt_state.rows_returned += rows_filled;

    unsafe { write_if_some(rows_fetched_ptr, rows_filled) };
    mark_no_rows(row_status_ptr, rows_filled, row_array_size);

    if let Some(e) = fetch_error {
        error!(%e, "SQLFetchScroll: row fetch failed");
        // The cursor cannot be resumed after a protocol failure, so tear the
        // row stream down rather than leaving it addressable.
        stmt_state.reset_row_stream();
        stmt_state.clear_state(STMT_STATE_CURSOR_OPEN);
        // Fans a server error out into one diagnostic per record, keeping each
        // SQLSTATE and native error rather than flattening them into HY000.
        post_tds_error(&mut stmt_state, &e, SQLSTATE_HY000);
        post_tds_info_messages(&mut stmt_state, &info_messages);
        return SQL_ERROR;
    }

    let has_server_info = post_tds_info_messages(&mut stmt_state, &info_messages);

    if rows_filled == 0 {
        stmt_state.reset_row_stream();
        debug!("SQLFetchScroll: end of rowset");
        return SQL_NO_DATA;
    }

    // Mixed SQLGetData access is only well defined when the rowset holds a
    // single row. With a wider rowset ODBC expects SQLSetPos to nominate the
    // current row first, and that is not implemented, so leave the cursor
    // unpositioned rather than silently handing back the last row of the block.
    if row_array_size == 1 {
        // The cursor is left on the row just read, with the bound columns
        // already consumed, so a following SQLGetData continues from there
        // rather than re-reading a column the fill loop took.
        stmt_state.begin_row();
        stmt_state.current_row_last_col = if buffered_get_data_row.is_some() {
            0
        } else {
            last_column_read
        };
        stmt_state.buffered_get_data_row = buffered_get_data_row;
    } else {
        stmt_state.reset_row_stream();
    }

    // Report why the rowset was imperfect with the SQLSTATE that value would
    // have produced through SQLGetData, rather than a blanket truncation
    // warning. Per-row detail lives in the row status array.
    // msodbcsql keys the return code on the rowset size, not on how many rows
    // failed: a block fetch demotes a row error to SQL_SUCCESS_WITH_INFO and
    // leaves the detail in the row status array, while a single-row fetch lets
    // the error stand (`sqlccurs.cpp`, gated on dwRowSize > 1).
    if let Some(issue) = worst.issue() {
        issue.post(&mut stmt_state);
        if row_array_size == 1 && matches!(worst, RowOutcome::Error(_)) {
            return SQL_ERROR;
        }
        return SQL_SUCCESS_WITH_INFO;
    }
    if has_server_info {
        return SQL_SUCCESS_WITH_INFO;
    }
    SQL_SUCCESS
}

/// How many rows this fetch may deliver.
///
/// `SQL_ATTR_MAX_ROWS = 0` is the ODBC default and means unlimited, so the whole
/// rowset is available. Otherwise the cap is a row budget rather than a rowset
/// boundary: measured against msodbcsql, a cap of 5 under a rowset of 4 yields
/// 4 rows and then a *partial* rowset of 1, not two full rowsets or one.
fn row_budget(max_rows: SqlULen, rows_returned: SqlULen, row_array_size: SqlULen) -> SqlULen {
    if max_rows == 0 {
        return row_array_size;
    }
    row_array_size.min(max_rows.saturating_sub(rows_returned))
}

/// Calls `dispatch` for consecutive row slots until the budget is spent or the
/// callback reports that no later row should be attempted.
fn dispatch_rows(row_budget: SqlULen, mut dispatch: impl FnMut() -> bool) {
    for _ in 0..row_budget {
        if !dispatch() {
            break;
        }
    }
}

/// Writes `SQL_ROW_NOROW` into the unused tail of the row status array.
fn mark_no_rows(row_status_ptr: *mut SqlUSmallInt, from: SqlULen, row_array_size: SqlULen) {
    if row_status_ptr.is_null() {
        return;
    }
    for i in from..row_array_size {
        unsafe { write_row_status(row_status_ptr, i, SQL_ROW_NOROW) };
    }
}

/// # Safety
/// `row_status_ptr` must be null or valid for `row_array_size` elements.
unsafe fn write_row_status(row_status_ptr: *mut SqlUSmallInt, row: SqlULen, status: SqlUSmallInt) {
    if row_status_ptr.is_null() {
        return;
    }
    unsafe { row_status_ptr.add(row).write_unaligned(status) };
}

/// Byte stride between consecutive elements of a column-wise bound array.
///
/// ODBC ignores `BufferLength` for a fixed-width target — an application may
/// legitimately pass anything, including 0 — so the stride there comes from the
/// C type. Only the character and binary targets are sized by the application.
fn element_stride(target_type: SqlSmallInt, buffer_length: SqlLen) -> usize {
    match target_type {
        SQL_C_BIT | SQL_C_TINYINT | SQL_C_STINYINT | SQL_C_UTINYINT => 1,
        SQL_C_SSHORT | SQL_C_USHORT => 2,
        SQL_C_SLONG | SQL_C_ULONG | SQL_C_FLOAT => 4,
        SQL_C_SBIGINT | SQL_C_UBIGINT | SQL_C_DOUBLE => 8,
        SQL_C_GUID => 16,
        SQL_C_TYPE_DATE | SQL_C_TYPE_TIME => 6,
        SQL_C_TYPE_TIMESTAMP => 16,
        SQL_C_SS_TIME2 => 12,
        SQL_C_SS_TIMESTAMPOFFSET => 20,
        // Character and binary: the application sizes the slot.
        _ => buffer_length.max(0) as usize,
    }
}

/// Current value of `SQL_ATTR_ROW_BIND_OFFSET_PTR`, in bytes.
///
/// Read unaligned: the offset displaces application pointers by an arbitrary
/// byte count, so nothing guarantees the result is aligned for `SqlULen`, and a
/// misaligned `read` is UB in Rust on every target rather than merely slow.
///
/// # Safety
/// `ptr` must be null or point to a readable `SqlULen`.
unsafe fn read_bind_offset(ptr: *mut SqlULen) -> usize {
    if ptr.is_null() {
        return 0;
    }
    unsafe { ptr.read_unaligned() }
}

/// Wire read size when draining a bound PLP column. Bounds peak memory: the
/// value itself is never materialized, only one chunk at a time.
const PLP_BOUND_CHUNK: usize = 8 * 1024;

/// The length a bound PLP delivery reports in the indicator.
///
/// Extracted so the rule can be tested without a live server: it was derived by
/// probing msodbcsql, and all four of its cases are pinned in unit tests below.
fn plp_indicator(
    produced_bytes: usize,
    truncated: bool,
    transcoded: bool,
    wire_total: Option<u64>,
) -> SqlLen {
    if !truncated {
        // Everything arrived, so the produced length is exact whatever the
        // encoding did.
        produced_bytes as SqlLen
    } else if transcoded {
        // Transcoding makes the wire byte count the wrong unit, and the
        // delivered count is not the total. msodbcsql reports SQL_NO_TOTAL here
        // for the same reason.
        SQL_NO_TOTAL
    } else {
        wire_total.map_or(SQL_NO_TOTAL, |t| t as SqlLen)
    }
}

fn trim_partial_utf8(bytes: &mut Vec<u8>) {
    if let Err(error) = std::str::from_utf8(bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
}

/// Drains an active PLP (max/LOB) stream into a bound column's fixed buffer.
///
/// Unlike `SQLGetData`, which hands back one chunk per call and lets the caller
/// come back for more, a bound column is filled once: everything past the
/// buffer is discarded, but still read off the wire so the row stays in sync.
/// Only one chunk is held at a time, and once the buffer is full the remaining
/// chunks are read but not decoded, so an oversized LOB costs the wire read
/// rather than the value.
///
/// Truncation stops on a character boundary, never mid-sequence: a partial
/// UTF-8 sequence or a lone surrogate would leave the caller holding text that
/// does not decode. msodbcsql trims the same way (`TrimPartialCodePt`, and
/// `GetColDataSurrogateSafe` for the wide target).
///
/// # Safety
/// Same contract as `deliver_bound`.
unsafe fn deliver_bound_plp(
    client: &mut mssql_tds::connection::tds_client::TdsClient,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    binding: &ColumnBinding,
    row_index: usize,
    bind_offset: usize,
    encoding: Option<PlpEncoding>,
    scratch: &mut [u8],
) -> Result<RowOutcome, TdsError> {
    let stride = element_stride(binding.target_type, binding.buffer_length);
    let indicator = if binding.strlen_or_ind_ptr.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe {
            (binding.strlen_or_ind_ptr as *mut u8)
                .add(bind_offset)
                .cast::<SqlLen>()
                .add(row_index)
        }
    };
    let slot =
        unsafe { (binding.target_value_ptr as *mut u8).add(bind_offset + row_index * stride) };

    // Same pairings SQLGetData supports. Three refusals are deliberate and
    // tracked, not oversights: bound binary delivery (AB#47239), widening
    // varchar(max)/json to SQL_C_WCHAR, and a max column bound to a typed C
    // target -- the last two are AB#47767, and both are cases msodbcsql
    // delivers. The typed one means varchar(50) and varchar(max) differ for the
    // same bind, since only the non-max path reaches convert_typed_c.
    let target = binding.target_type;
    let compatible = matches!(
        (target, encoding),
        (SQL_C_WCHAR, Some(PlpEncoding::Utf16Text))
            | (SQL_C_CHAR, Some(PlpEncoding::SingleByteText))
            | (SQL_C_CHAR, Some(PlpEncoding::Utf8Text))
            | (SQL_C_CHAR, Some(PlpEncoding::Utf16Text))
    );
    if !compatible {
        // The stream still has to be consumed, or the next column decodes from
        // the middle of this value. A failure here is the caller's problem, not
        // this column's: it leaves the cursor mid-LOB.
        drain_plp_to_end(client, runtime, scratch)?;
        return Ok(RowOutcome::Error(RowIssue::Unsupported));
    }

    let transcode = target == SQL_C_CHAR && matches!(encoding, Some(PlpEncoding::Utf16Text));
    let buf_elements = char_buf_elements(target, stride);
    // Room for the payload, less the terminator the copy always writes.
    let capacity_elements = buf_elements.saturating_sub(1);

    let mut out_bytes: Vec<u8> = Vec::new();
    let mut out_units: Vec<u16> = Vec::new();
    let mut pending_byte: Option<u8> = None;
    let mut pending_high_surrogate: Option<u16> = None;
    let mut truncated = false;
    let mut wire_total: Option<u64>;

    loop {
        let chunk = runtime.block_on(client.read_active_plp_chunk(scratch))?;
        wire_total = chunk.known_total;

        // Once the buffer is full the rest of the value still has to come off
        // the wire, but decoding it would be pure waste.
        if truncated {
            if chunk.reached_end {
                break;
            }
            continue;
        }

        if target == SQL_C_WCHAR {
            // Whole code units only; an odd tail is carried to the next chunk.
            let mut bytes = Vec::with_capacity(chunk.read + 1);
            if let Some(b) = pending_byte.take() {
                bytes.push(b);
            }
            bytes.extend_from_slice(&scratch[..chunk.read]);
            let even = bytes.len() & !1;
            if even != bytes.len() {
                pending_byte = Some(bytes[even]);
            }
            for pair in bytes[..even].chunks_exact(2) {
                let unit = u16::from_le_bytes([pair[0], pair[1]]);
                let is_high_surrogate = (0xD800..0xDC00).contains(&unit);
                // A high surrogate is only worth keeping if its low half fits
                // too; alone it is not a character.
                let need = if is_high_surrogate { 2 } else { 1 };
                if out_units.len() + need <= capacity_elements {
                    out_units.push(unit);
                } else {
                    truncated = true;
                    break;
                }
            }
        } else if transcode {
            let utf8 = utf16le_chunk_to_utf8(
                &scratch[..chunk.read],
                chunk.reached_end,
                &mut pending_byte,
                &mut pending_high_surrogate,
            );
            // Whole characters only: a partial UTF-8 sequence in the caller's
            // buffer would not decode.
            for ch in utf8.chars() {
                let need = ch.len_utf8();
                if out_bytes.len() + need <= capacity_elements {
                    let mut enc = [0u8; 4];
                    out_bytes.extend_from_slice(ch.encode_utf8(&mut enc).as_bytes());
                } else {
                    truncated = true;
                    break;
                }
            }
        } else if matches!(encoding, Some(PlpEncoding::Utf8Text)) {
            for b in &scratch[..chunk.read] {
                if out_bytes.len() < capacity_elements {
                    out_bytes.push(*b);
                } else {
                    truncated = true;
                    trim_partial_utf8(&mut out_bytes);
                    break;
                }
            }
        } else {
            for b in &scratch[..chunk.read] {
                if out_bytes.len() < capacity_elements {
                    out_bytes.push(*b);
                } else {
                    truncated = true;
                    break;
                }
            }
        }

        if chunk.reached_end {
            break;
        }
    }

    let produced_bytes = if target == SQL_C_WCHAR {
        out_units.len() * std::mem::size_of::<SqlWChar>()
    } else {
        out_bytes.len()
    };
    unsafe {
        write_if_some(
            indicator,
            plp_indicator(produced_bytes, truncated, transcode, wire_total),
        )
    };

    if target == SQL_C_WCHAR {
        unsafe { copy_with_nul(slot as *mut SqlWChar, buf_elements, &out_units) };
    } else {
        unsafe { copy_with_nul(slot, buf_elements, &out_bytes) };
    }

    Ok(if truncated {
        RowOutcome::Info(RowIssue::StringTruncated)
    } else {
        RowOutcome::Success
    })
}

/// Consumes whatever is left of the active PLP stream and discards it.
fn drain_plp_to_end(
    client: &mut mssql_tds::connection::tds_client::TdsClient,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    scratch: &mut [u8],
) -> Result<(), TdsError> {
    loop {
        if runtime
            .block_on(client.read_active_plp_chunk(scratch))?
            .reached_end
        {
            return Ok(());
        }
    }
}

/// Writes already-encoded character data into one bound row slot.
///
/// # Safety
///
/// The binding's target and indicator pointers, after applying `bind_offset`
/// and `row_index`, must be valid for their declared slot sizes. The writable
/// target slot must not overlap `bytes`.
unsafe fn deliver_encoded_string(
    binding: &ColumnBinding,
    row_index: usize,
    bind_offset: usize,
    bytes: Cow<'_, [u8]>,
    encoding: EncodingType,
) -> RowOutcome {
    let direct_char = binding.target_type == SQL_C_CHAR
        && (matches!(encoding, EncodingType::Utf8) && std::str::from_utf8(&bytes).is_ok()
            || matches!(encoding, EncodingType::LcidBased(_)) && bytes.is_ascii());
    let direct_wchar = binding.target_type == SQL_C_WCHAR
        && matches!(encoding, EncodingType::Utf16)
        && bytes.len().is_multiple_of(2)
        && std::char::decode_utf16(
            bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]])),
        )
        .all(|unit| unit.is_ok());

    if !direct_char && !direct_wchar {
        return unsafe {
            deliver_bound(
                binding,
                row_index,
                bind_offset,
                &ColumnValues::String(SqlString::new(bytes.into_owned(), encoding)),
            )
        };
    }

    let stride = element_stride(binding.target_type, binding.buffer_length);
    let indicator = if binding.strlen_or_ind_ptr.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe {
            (binding.strlen_or_ind_ptr as *mut u8)
                .add(bind_offset)
                .cast::<SqlLen>()
                .add(row_index)
        }
    };
    let slot =
        unsafe { (binding.target_value_ptr as *mut u8).add(bind_offset + row_index * stride) };

    if direct_char {
        unsafe { write_if_some(indicator, bytes.len() as SqlLen) };
        return if unsafe { copy_with_nul(slot, stride, &bytes) } {
            RowOutcome::Info(RowIssue::StringTruncated)
        } else {
            RowOutcome::Success
        };
    }

    let source_len = bytes.len() / 2;
    unsafe { write_if_some(indicator, bytes.len() as SqlLen) };
    if slot.is_null() {
        return RowOutcome::Success;
    }
    let buf_elements = char_buf_elements(binding.target_type, stride);
    if buf_elements == 0 {
        return if source_len == 0 {
            RowOutcome::Success
        } else {
            RowOutcome::Info(RowIssue::StringTruncated)
        };
    }
    let destination = slot.cast::<SqlWChar>();
    let copy_len = source_len.min(buf_elements - 1);
    for (index, chunk) in bytes.chunks_exact(2).take(copy_len).enumerate() {
        unsafe {
            destination
                .add(index)
                .write_unaligned(u16::from_le_bytes([chunk[0], chunk[1]]))
        };
    }
    unsafe { destination.add(copy_len).write_unaligned(0) };
    if copy_len < source_len {
        RowOutcome::Info(RowIssue::StringTruncated)
    } else {
        RowOutcome::Success
    }
}

/// Writes one column value into its bound buffer slot for row `row_index`.
///
/// # Safety
/// The binding's pointers, displaced by `bind_offset`, must address at least
/// `row_index + 1` elements — the contract `SQLBindCol` places on the
/// application together with `SQL_ATTR_ROW_ARRAY_SIZE`.
unsafe fn deliver_bound(
    binding: &ColumnBinding,
    row_index: usize,
    bind_offset: usize,
    value: &ColumnValues,
) -> RowOutcome {
    // A zero stride only arises from a character or binary binding with
    // BufferLength 0, which msodbcsql treats as a length probe: the indicator
    // gets the available length and the buffer is left alone. The copy below
    // already does exactly that, so this is not rejected up front.
    let stride = element_stride(binding.target_type, binding.buffer_length);

    // SQL_ATTR_ROW_BIND_OFFSET_PTR displaces both bases by the same byte count.
    let indicator = if binding.strlen_or_ind_ptr.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe {
            (binding.strlen_or_ind_ptr as *mut u8)
                .add(bind_offset)
                .cast::<SqlLen>()
                .add(row_index)
        }
    };

    let is_null = matches!(value, ColumnValues::Null);
    if is_null && indicator.is_null() {
        // There is nowhere to report the NULL, and leaving the slot untouched
        // would read back as the previous row's value.
        return RowOutcome::Error(RowIssue::IndicatorRequired);
    }

    let slot =
        unsafe { (binding.target_value_ptr as *mut u8).add(bind_offset + row_index * stride) };

    if is_null {
        unsafe { write_if_some(indicator, SQL_NULL_DATA) };
        // A character target still gets a terminator so the slot does not read
        // back as whatever the previous row left there.
        let buf_elements = char_buf_elements(binding.target_type, stride);
        if binding.target_type == SQL_C_WCHAR {
            unsafe { copy_with_nul(slot as *mut SqlWChar, buf_elements, &[]) };
        } else if binding.target_type == SQL_C_CHAR {
            unsafe { copy_with_nul(slot, buf_elements, &[]) };
        }
        return RowOutcome::Success;
    }

    if is_typed_c_target(binding.target_type) {
        let converted =
            unsafe { convert_typed_c(value, binding.target_type, slot as SqlPointer, indicator) };
        return match converted {
            Ok(ConvOk::Exact) => RowOutcome::Success,
            Ok(ConvOk::Truncated) => RowOutcome::Info(RowIssue::FractionalTruncated),
            Err(ConvError::OutOfRange) => RowOutcome::Error(RowIssue::OutOfRange),
            Err(ConvError::Restricted) => RowOutcome::Error(RowIssue::Restricted),
            Err(ConvError::InvalidCharacterValue) => RowOutcome::Error(RowIssue::InvalidCharacter),
            Err(ConvError::NotHandledHere) => RowOutcome::Error(RowIssue::Unsupported),
        };
    }

    if binding.target_type != SQL_C_CHAR && binding.target_type != SQL_C_WCHAR {
        // SQL_C_BINARY delivery is still unimplemented (AB#47239); anything else
        // is an unsupported target.
        return RowOutcome::Error(RowIssue::Unsupported);
    }

    let text = match column_value_to_text(value) {
        Ok(t) => t,
        Err(TextError::Malformed) => return RowOutcome::Error(RowIssue::InvalidCharacter),
        Err(TextError::Unsupported) => return RowOutcome::Error(RowIssue::Unsupported),
    };

    let buf_elements = char_buf_elements(binding.target_type, stride);
    if binding.target_type == SQL_C_WCHAR {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        unsafe { write_if_some(indicator, (utf16.len() * 2) as SqlLen) };
        let truncated = unsafe { copy_with_nul(slot as *mut SqlWChar, buf_elements, &utf16) };
        if truncated {
            return RowOutcome::Info(RowIssue::StringTruncated);
        }
    } else {
        let bytes = text.as_bytes();
        unsafe { write_if_some(indicator, bytes.len() as SqlLen) };
        let truncated = unsafe { copy_with_nul(slot, buf_elements, bytes) };
        if truncated {
            return RowOutcome::Info(RowIssue::StringTruncated);
        }
    }
    RowOutcome::Success
}

/// Writes one exact fixed-width value and its byte-count indicator.
///
/// # Safety
///
/// The binding's target pointer, after applying `bind_offset` and `row_index`,
/// must be null or writable for `T`; its displaced indicator pointer must be
/// null or writable for one `SqlLen`.
unsafe fn deliver_fixed_bound<T: Copy>(
    binding: &ColumnBinding,
    row_index: usize,
    bind_offset: usize,
    value: T,
) -> RowOutcome {
    let stride = element_stride(binding.target_type, binding.buffer_length);
    let indicator = if binding.strlen_or_ind_ptr.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe {
            (binding.strlen_or_ind_ptr as *mut u8)
                .add(bind_offset)
                .cast::<SqlLen>()
                .add(row_index)
        }
    };
    if !binding.target_value_ptr.is_null() {
        let slot =
            unsafe { (binding.target_value_ptr as *mut u8).add(bind_offset + row_index * stride) };
        unsafe { slot.cast::<T>().write_unaligned(value) };
    }
    let size = SqlLen::try_from(std::mem::size_of::<T>()).unwrap_or(SqlLen::MAX);
    unsafe { write_if_some(indicator, size) };
    RowOutcome::Success
}

/// Capacity of one bound slot in target elements, so a `SQL_C_WCHAR` buffer is
/// measured in UTF-16 code units rather than bytes.
fn char_buf_elements(target_type: SqlSmallInt, stride: usize) -> usize {
    if target_type == SQL_C_WCHAR {
        stride / std::mem::size_of::<SqlWChar>()
    } else {
        stride
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{
        SQL_C_BINARY, SQL_C_SLONG, SQL_FETCH_ABSOLUTE, SQL_FETCH_FIRST, SQL_FETCH_LAST,
        SQL_FETCH_PRIOR, SQL_FETCH_RELATIVE,
    };
    use crate::api::sqlstate::SQLSTATE_HY106;
    use crate::api::sqlstate::{ERR_CONNECTION_BUSY, SQLSTATE_24000, SQLSTATE_HY000};
    use crate::handles::dbc::DbcHandle;
    use crate::handles::stmt::STMT_STATE_CURSOR_OPEN;
    use crate::test_support::TestHandles;
    use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
    use mssql_tds::test_client_support::{
        col_metadata_empty, done_no_more, int_columns, mixed_lob_columns, tds_client_from_int_rows,
        tds_client_from_mixed_lob_prefix_rows, tds_client_from_partial_int_rows,
        tds_client_from_tokens,
    };

    fn binding(
        column_number: SqlUSmallInt,
        target_type: SqlSmallInt,
        target_value_ptr: SqlPointer,
        buffer_length: SqlLen,
        strlen_or_ind_ptr: *mut SqlLen,
    ) -> ColumnBinding {
        ColumnBinding {
            column_number,
            target_type,
            target_value_ptr,
            buffer_length,
            strlen_or_ind_ptr,
        }
    }

    fn open_cursor(h: &TestHandles) {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut s = stmt.inner.lock().unwrap();
        s.set_state(STMT_STATE_CURSOR_OPEN);
    }

    #[test]
    fn buffered_row_discards_each_leading_slot_once() {
        let mut reusable = None;
        let mut row = BufferedGetDataRow::empty(&mut reusable, 4);
        row.values = vec![
            Some(ColumnValues::Int(1)),
            Some(ColumnValues::Int(2)),
            Some(ColumnValues::Int(3)),
            None,
        ];

        row.discard_before(1);
        row.values[0] = Some(ColumnValues::Int(99));
        row.discard_before(2);

        assert_eq!(row.values[0], Some(ColumnValues::Int(99)));
        assert_eq!(row.values[1], None);
        assert_eq!(row.values[2], Some(ColumnValues::Int(3)));
        assert_eq!(row.consumed, 2);
    }

    #[test]
    fn buffered_row_reuses_matching_string_storage() {
        let mut reusable = None;
        let mut row = BufferedGetDataRow::empty(&mut reusable, 1);
        let mut bytes = Vec::with_capacity(32);
        bytes.extend_from_slice(b"existing");
        row.values[0] = Some(ColumnValues::String(SqlString::new(
            bytes,
            EncodingType::Utf8,
        )));
        let original = match row.values[0].as_ref().unwrap() {
            ColumnValues::String(value) => value.bytes.as_ptr(),
            value => panic!("unexpected value: {value:?}"),
        };

        row.write_string(0, Cow::Borrowed(b"new"), EncodingType::Utf8);

        let value = match row.values[0].as_ref().unwrap() {
            ColumnValues::String(value) => value,
            value => panic!("unexpected value: {value:?}"),
        };
        assert_eq!(value.bytes, b"new");
        assert_eq!(value.bytes.as_ptr(), original);
    }

    #[test]
    fn deferred_columns_drop_recycled_row_data() {
        let mut reusable = None;
        let mut row = BufferedGetDataRow::empty(&mut reusable, 8);
        for col in 6..8 {
            row.values[col] = Some(ColumnValues::String(SqlString::new(
                b"previous result".to_vec(),
                EncodingType::Utf8,
            )));
        }
        row.variant_bases[6] = Some(TdsDataType::VarBinary);
        row.variant_bases[7] = Some(TdsDataType::NVarChar);
        reusable = Some(row);

        let mut next = BufferedGetDataRow::empty(&mut reusable, 8);
        next.values[5] = Some(ColumnValues::Int(42));
        next.defer_from(6);

        assert_eq!(next.values[5], Some(ColumnValues::Int(42)));
        assert_eq!(next.values[6], None);
        assert_eq!(next.values[7], None);
        assert_eq!(next.variant_bases[6], None);
        assert_eq!(next.variant_bases[7], None);
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let rc = unsafe { sql_fetch_scroll(ptr::null_mut(), SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    // ---- SQL_ATTR_MAX_ROWS ------------------------------------------------

    /// Builds a statement whose cursor is open on a one-column result set that
    /// has already yielded `rows_returned` rows under a `max_rows` cap.
    fn stmt_at_max_rows(h: &TestHandles, max_rows: SqlULen, rows_returned: SqlULen) {
        use mssql_tds::test_client_support::{done_no_more, tds_client_from_tokens};

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut stmt_state = stmt_handle.inner.lock().unwrap();
            stmt_state.set_state(STMT_STATE_CURSOR_OPEN);
            stmt_state.begin_result_set(int_columns(1));
            stmt_state.max_rows = max_rows;
            stmt_state.rows_returned = rows_returned;
            // A statement that has already returned rows is positioned on the
            // last one, which is what makes the cutoff's row-stream reset
            // observable rather than vacuously true.
            stmt_state.row_positioned = rows_returned > 0;
        }

        let dbc_handle = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut dbc_state = dbc_handle.inner.lock().unwrap();
        dbc_state.client = Some(tds_client_from_tokens(vec![done_no_more()]));
        dbc_state.active_stmt = Some(h.stmt);
    }

    /// The cap is reached, so the fetch must report end-of-data *without*
    /// pulling a row — the scripted client holds no rows, so a fetch that got
    /// past the cutoff would fail rather than silently pass.
    #[test]
    fn fetch_at_max_rows_returns_no_data_without_pulling() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_at_max_rows(&h, 3, 3);

        let mut rows_fetched: SqlULen = 99;
        {
            let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt_handle.inner.lock().unwrap();
            s.rows_fetched_ptr = &mut rows_fetched;
        }

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_NO_DATA
        );
        assert_eq!(rows_fetched, 0, "the cutoff still reports its rowset size");

        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let stmt_state = stmt_handle.inner.lock().unwrap();
        assert!(stmt_state.diag_records.is_empty(), "cap is not an error");
        // The cursor stays open and the connection stays busy on this statement
        // so the rest of the result set can still be drained.
        assert!(stmt_state.has_state(STMT_STATE_CURSOR_OPEN));
        // Measured on msodbcsql: past the cap the previous row stops being
        // readable, so SQL_ATTR_ROW_NUMBER and SQLGetData both answer 24000 —
        // the same state as the natural end of a result set.
        assert!(!stmt_state.row_positioned);
        drop(stmt_state);

        let dbc_handle = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let dbc_state = dbc_handle.inner.lock().unwrap();
        assert!(dbc_state.client.is_some());
        assert_eq!(dbc_state.active_stmt, Some(h.stmt));
    }

    /// One row below the cap the cutoff must not fire, so the fetch reaches the
    /// wire. Asserted through the connection hand-back rather than the return
    /// code: the cap path is the only one that leaves the statement owning the
    /// connection, so `active_stmt` distinguishes the two paths without
    /// depending on what the scripted result happens to answer.
    #[test]
    fn fetch_below_max_rows_is_not_cut_off() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_at_max_rows(&h, 3, 2);

        unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };

        let dbc_handle = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let dbc_state = dbc_handle.inner.lock().unwrap();
        assert_eq!(
            dbc_state.active_stmt, None,
            "the fetch reached the client rather than being cut off"
        );
    }

    /// `SQL_ATTR_MAX_ROWS = 0` is the ODBC default and means unlimited, so a
    /// row count far above it must not be treated as having reached a cap.
    #[test]
    fn fetch_with_max_rows_zero_is_unlimited() {
        let h = TestHandles::with_env_dbc_stmt();
        stmt_at_max_rows(&h, 0, 1000);

        unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };

        let dbc_handle = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let dbc_state = dbc_handle.inner.lock().unwrap();
        assert_eq!(
            dbc_state.active_stmt, None,
            "the fetch reached the client rather than being cut off"
        );
    }

    /// The cap truncates a rowset rather than rounding to a rowset boundary.
    /// Measured against msodbcsql: a cap of 5 with `SQL_ATTR_ROW_ARRAY_SIZE = 4`
    /// over a 20-row result yields 4 rows, then 1, then SQL_NO_DATA.
    #[test]
    fn the_cap_truncates_a_rowset_rather_than_rounding_it() {
        // Unlimited leaves the whole rowset available however many rows have
        // already gone out.
        assert_eq!(row_budget(0, 1000, 4), 4);
        // Well inside the cap the rowset is untouched.
        assert_eq!(row_budget(20, 0, 4), 4);
        assert_eq!(row_budget(8, 4, 4), 4);
        // The cap lands inside the rowset, so the fetch is cut short.
        assert_eq!(row_budget(5, 4, 4), 1);
        assert_eq!(row_budget(6, 4, 4), 2);
        assert_eq!(row_budget(3, 0, 4), 3);
        // A cap already spent yields nothing; `max_rows_reached` short-circuits
        // before this, so the budget only has to stay total, not underflow.
        assert_eq!(row_budget(5, 5, 4), 0);
        assert_eq!(row_budget(5, 9, 4), 0);
    }

    #[test]
    fn max_rows_bounds_the_whole_row_dispatch_loop() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let mut values = [0_i32; 4];
        let mut indicators = [0 as SqlLen; 4];
        let mut statuses = [SQL_ROW_NOROW; 4];
        let mut rows_fetched = 0;
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.begin_result_set(int_columns(1));
            state.max_rows = 5;
            state.rows_returned = 4;
            state.row_array_size = 4;
            state.rows_fetched_ptr = &mut rows_fetched;
            state.row_status_ptr = statuses.as_mut_ptr();
            state.set_binding(binding(
                1,
                SQL_C_SLONG,
                values.as_mut_ptr().cast(),
                0,
                indicators.as_mut_ptr(),
            ));
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = tds_client_from_int_rows(vec![vec![10], vec![20], vec![30]]);
        dbc.runtime
            .block_on(client.execute("SELECT buffered rows".to_string(), ()))
            .unwrap();
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(client);
            state.active_stmt = Some(h.stmt);
        }

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );
        assert_eq!(rows_fetched, 1);
        assert_eq!(values, [10, 0, 0, 0]);
        assert_eq!(
            statuses,
            [SQL_ROW_SUCCESS, SQL_ROW_NOROW, SQL_ROW_NOROW, SQL_ROW_NOROW]
        );
        assert_eq!(stmt.inner.lock().unwrap().rows_returned, 5);

        {
            let mut state = stmt.inner.lock().unwrap();
            state.max_rows = 0;
            state.rows_returned = 0;
            state.row_array_size = 1;
        }
        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );
        assert_eq!(
            values[0], 20,
            "the capped fetch consumed only the first row"
        );
    }

    #[test]
    fn partial_binding_uses_the_column_cursor_path() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let mut value = 0_i32;
        let mut indicator = 0;
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.begin_result_set(int_columns(2));
            state.set_binding(binding(
                1,
                SQL_C_SLONG,
                (&mut value as *mut i32).cast(),
                0,
                &mut indicator,
            ));
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = tds_client_from_int_rows(vec![vec![10, 20]]);
        dbc.runtime
            .block_on(client.execute("SELECT partial binding".to_string(), ()))
            .unwrap();
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(client);
            state.active_stmt = Some(h.stmt);
        }

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );
        assert_eq!(value, 10);
        assert_eq!(indicator, 4);

        let mut second = 0_i32;
        let mut second_indicator = 0;
        assert_eq!(
            unsafe {
                crate::api::get_data::sql_get_data(
                    h.stmt,
                    2,
                    SQL_C_SLONG,
                    (&mut second as *mut i32).cast(),
                    0,
                    &mut second_indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(second, 20);
        assert_eq!(second_indicator, 4);
    }

    fn mixed_lob_stmt(h: &TestHandles, rows: Vec<Vec<i32>>) {
        h.mark_dbc_connected();
        let prefix_columns = rows.first().map_or(0, Vec::len);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.begin_result_set(mixed_lob_columns(prefix_columns));
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = tds_client_from_mixed_lob_prefix_rows(rows);
        dbc.runtime
            .block_on(client.execute("SELECT mixed LOB row".to_string(), ()))
            .unwrap();
        let mut state = dbc.inner.lock().unwrap();
        state.client = Some(client);
        state.active_stmt = Some(h.stmt);
    }

    #[test]
    fn fetch_captures_inline_prefix_and_defers_lob() {
        let h = TestHandles::with_env_dbc_stmt();
        mixed_lob_stmt(&h, vec![vec![10, 20]]);

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let row = state.buffered_get_data_row.as_ref().unwrap();
        assert_eq!(
            row.values,
            vec![
                Some(ColumnValues::Int(10)),
                Some(ColumnValues::Int(20)),
                None
            ]
        );
        assert_eq!(state.current_row_last_col, 0);
    }

    #[test]
    fn fetch_clears_recycled_value_when_lob_remains_deferred() {
        let h = TestHandles::with_env_dbc_stmt();
        mixed_lob_stmt(&h, vec![vec![10, 20, 30, 40, 50, 60, 70]]);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.spare_get_data_row = Some(BufferedGetDataRow {
                values: vec![
                    Some(ColumnValues::Int(-1)),
                    Some(ColumnValues::Int(-2)),
                    Some(ColumnValues::Int(-3)),
                    Some(ColumnValues::Int(-4)),
                    Some(ColumnValues::Int(-5)),
                    Some(ColumnValues::Int(-6)),
                    Some(ColumnValues::Int(-7)),
                    Some(ColumnValues::String(SqlString::new(
                        b"previous result".to_vec(),
                        EncodingType::Utf16,
                    ))),
                ],
                variant_bases: vec![None; 8],
                consumed: 0,
                wire_deferred: false,
            });
        }

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );

        let state = stmt.inner.lock().unwrap();
        let row = state.buffered_get_data_row.as_ref().unwrap();
        assert_eq!(row.values[0], Some(ColumnValues::Int(10)));
        assert_eq!(row.values[6], Some(ColumnValues::Int(70)));
        assert_eq!(row.values[7], None);
    }

    #[test]
    fn leading_lob_bypasses_prefix_buffering() {
        let h = TestHandles::with_env_dbc_stmt();
        mixed_lob_stmt(&h, vec![vec![]]);

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt.inner.lock().unwrap().buffered_get_data_row.is_none());
    }

    #[test]
    fn bound_and_multi_row_fetches_bypass_prefix_buffering() {
        let bound = TestHandles::with_env_dbc_stmt();
        mixed_lob_stmt(&bound, vec![vec![10, 20]]);
        let mut value = 0_i32;
        let mut indicator = 0;
        let bound_stmt = unsafe { handle_from_raw::<StmtHandle>(bound.stmt) };
        bound_stmt.inner.lock().unwrap().set_binding(binding(
            1,
            SQL_C_SLONG,
            (&mut value as *mut i32).cast(),
            0,
            &mut indicator,
        ));
        assert_eq!(
            unsafe { sql_fetch_scroll(bound.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );
        assert_eq!(value, 10);
        assert!(
            bound_stmt
                .inner
                .lock()
                .unwrap()
                .buffered_get_data_row
                .is_none()
        );

        let multi = TestHandles::with_env_dbc_stmt();
        mixed_lob_stmt(&multi, vec![vec![10, 20], vec![30, 40]]);
        let multi_stmt = unsafe { handle_from_raw::<StmtHandle>(multi.stmt) };
        multi_stmt.inner.lock().unwrap().row_array_size = 2;
        assert_eq!(
            unsafe { sql_fetch_scroll(multi.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );
        assert!(
            multi_stmt
                .inner
                .lock()
                .unwrap()
                .buffered_get_data_row
                .is_none()
        );
    }

    #[test]
    fn terminal_fetch_invalidates_fetch_buffered_get_data_row() {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.begin_result_set(int_columns(2));
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = tds_client_from_int_rows(vec![vec![10, 17]]);
        dbc.runtime
            .block_on(client.execute("SELECT buffered row".to_string(), ()))
            .unwrap();
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(client);
            state.active_stmt = Some(h.stmt);
        }

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );

        let mut value = 0_i32;
        let mut indicator = 0;
        assert_eq!(
            unsafe {
                crate::api::get_data::sql_get_data(
                    h.stmt,
                    1,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(value, 10);

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_NO_DATA
        );

        value = 0;
        assert_eq!(
            unsafe {
                crate::api::get_data::sql_get_data(
                    h.stmt,
                    2,
                    SQL_C_SLONG,
                    (&mut value as *mut i32).cast(),
                    0,
                    &mut indicator,
                )
            },
            SQL_ERROR
        );
        assert_eq!(value, 0, "the stale buffered value must not be replayed");
        let state = stmt.inner.lock().unwrap();
        assert_eq!(state.diag_records.last().unwrap().sql_state, SQLSTATE_24000);
        assert!(!state.row_positioned);
        assert!(state.buffered_get_data_row.is_none());
        drop(state);
        assert!(dbc.inner.lock().unwrap().active_stmt.is_none());
    }

    fn assert_partial_buffered_row_delivery(buffered_prefix_columns: usize) {
        let h = TestHandles::with_env_dbc_stmt();
        h.mark_dbc_connected();
        let mut first = [0_i32; 2];
        let mut second = [0_i32; 2];
        let mut first_indicators = [0 as SqlLen; 2];
        let mut second_indicators = [0 as SqlLen; 2];
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_CURSOR_OPEN);
            state.begin_result_set(int_columns(2));
            state.set_binding(binding(
                1,
                SQL_C_SLONG,
                first.as_mut_ptr().cast(),
                0,
                first_indicators.as_mut_ptr(),
            ));
            state.set_binding(binding(
                2,
                SQL_C_SLONG,
                second.as_mut_ptr().cast(),
                0,
                second_indicators.as_mut_ptr(),
            ));
        }
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client =
            tds_client_from_partial_int_rows(vec![vec![10, 20]], buffered_prefix_columns);
        dbc.runtime
            .block_on(client.execute("SELECT partial buffered row".to_string(), ()))
            .unwrap();
        {
            let mut state = dbc.inner.lock().unwrap();
            state.client = Some(client);
            state.active_stmt = Some(h.stmt);
        }

        assert_eq!(
            unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) },
            SQL_SUCCESS
        );
        assert_eq!(first[0], 10);
        assert_eq!(second[0], 20);
        assert_eq!(first_indicators[0], 4);
        assert_eq!(second_indicators[0], 4);
    }

    #[test]
    fn bound_row_writer_survives_continuation_after_row_header() {
        assert_partial_buffered_row_delivery(0);
    }

    #[test]
    fn bound_row_writer_survives_continuation_after_first_column() {
        assert_partial_buffered_row_delivery(1);
    }

    /// The cap is per result set, so advancing onto a new one must restart the
    /// budget; otherwise a spent cap would truncate every later result set.
    #[test]
    fn begin_result_set_restarts_the_max_rows_budget() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_handle = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let mut stmt_state = stmt_handle.inner.lock().unwrap();

        stmt_state.max_rows = 3;
        stmt_state.rows_returned = 3;
        assert!(stmt_state.max_rows_reached());

        stmt_state.begin_result_set(int_columns(2));
        assert_eq!(stmt_state.rows_returned, 0);
        assert!(!stmt_state.max_rows_reached());
        // The cap itself is a statement attribute and survives the new result
        // set — only the count restarts.
        assert_eq!(stmt_state.max_rows, 3);
    }

    /// The cursor is forward-only, so every other orientation is rejected
    /// rather than silently treated as SQL_FETCH_NEXT.
    #[test]
    fn only_fetch_next_is_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        for orientation in [
            SQL_FETCH_FIRST,
            SQL_FETCH_LAST,
            SQL_FETCH_PRIOR,
            SQL_FETCH_ABSOLUTE,
            SQL_FETCH_RELATIVE,
        ] {
            let rc = unsafe { sql_fetch_scroll(h.stmt, orientation, 0) };
            assert_eq!(rc, SQL_ERROR, "orientation {orientation}");
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let s = stmt.inner.lock().unwrap();
            assert_eq!(s.diag_records.last().unwrap().sql_state, SQLSTATE_HY106);
        }
    }

    #[test]
    fn fetch_without_an_open_cursor_is_a_cursor_state_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_INVALID_CURSOR_STATE.state
        );
    }

    /// A cursor already known exhausted (set by a prior fetch's peek past its
    /// last row) reports SQL_NO_DATA without ever touching the connection —
    /// not even to discover it has no client at all. This is what lets the
    /// answer stay correct regardless of what else is happening on the DBC.
    #[test]
    fn exhausted_cursor_fast_path_never_touches_the_connection() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.result_set_exhausted = true;
        }
        // DBC is left disconnected with no client at all: if the fast path
        // reached for the connection this would fail with a different
        // SQLSTATE (not connected / no active client) instead of SQL_NO_DATA.
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_NO_DATA);
    }

    /// A prior fetch's read-ahead peek can have discovered a trailing SQL
    /// Server error instead of a clean end of set (see AB#47508's
    /// `release_busy_if_row_exhausted`), deferred via `pending_fetch_error`
    /// since that fetch call had already committed to delivering its own
    /// row successfully. The next `SQLFetch`/`SQLFetchScroll` — the call
    /// that would have hit this error directly without the peek's
    /// read-ahead — must drain and report it instead of silently reporting
    /// `SQL_NO_DATA`, and must not need the connection to do so.
    #[test]
    fn exhausted_cursor_fast_path_surfaces_a_pending_fetch_error() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.result_set_exhausted = true;
            s.pending_fetch_error = Some(TdsError::ProtocolError(
                "simulated trailing SQL Server error".to_string(),
            ));
        }
        // Same as the sibling test: no connection is configured at all, so a
        // fallthrough to the normal fetch path would fail differently.
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert!(
            s.diag_records
                .iter()
                .any(|d| d.message.contains("simulated trailing SQL Server error")),
            "the deferred error must be posted, not silently dropped as SQL_NO_DATA"
        );
        assert!(
            s.pending_fetch_error.is_none(),
            "must be taken so it cannot leak into a later call"
        );
        assert!(
            !s.has_state(STMT_STATE_CURSOR_OPEN),
            "matches the ordinary fetch-error tail: the cursor cannot be resumed after this"
        );
    }

    /// The deferred-error fast path above closes the cursor before
    /// returning, so `SQLCloseCursor`/`SQLFreeStmt(SQL_CLOSE)` can no longer
    /// reach a `StmtState::pending_fetch_info` stashed alongside the error by
    /// the same peek (see AB#47508's `release_busy_if_row_exhausted`, which
    /// can set both together — a trailing INFO message read on the way to a
    /// batch-ending SQL Server error). Since this branch's `SQL_ERROR` can
    /// carry extra diagnostic records (unlike the sibling `SQL_NO_DATA`), the
    /// stashed message must be surfaced here rather than silently discarded.
    #[test]
    fn exhausted_cursor_fast_path_surfaces_a_pending_fetch_info_alongside_the_error() {
        use mssql_tds::error::SqlInfoMessage;

        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.result_set_exhausted = true;
            s.pending_fetch_error = Some(TdsError::ProtocolError(
                "simulated trailing SQL Server error".to_string(),
            ));
            s.pending_fetch_info = vec![SqlInfoMessage {
                message: "trailing PRINT message".to_string(),
                state: 1,
                class: 0,
                number: 0,
                server_name: None,
                proc_name: None,
                line_number: None,
            }];
        }
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert!(
            s.diag_records
                .iter()
                .any(|d| d.message.contains("trailing PRINT message")),
            "the stashed INFO message must be surfaced alongside the deferred error"
        );
        assert!(
            s.pending_fetch_info.is_empty(),
            "must be taken so it cannot leak into a later call"
        );
    }

    /// Given the *consequence* of AB#47508's fix (a cursor whose fetch has
    /// already released the busy claim and marked its result set
    /// exhausted — the two post-conditions `release_busy_if_row_exhausted`
    /// produces, taken as a precondition here since driving a real
    /// one-row `fill_rowset` fetch through this crate's downstream
    /// scripted-token test harness cannot itself produce a positioned row,
    /// see `real_zero_row_fetch_through_fill_rowset_releases_active_stmt`
    /// below for the closest achievable real-fetch equivalent): statement B
    /// must then be able to claim the connection without seeing "Connection
    /// is busy with results for another command", and A must still be able
    /// to report SQL_NO_DATA afterward — including while B is actively
    /// using the connection.
    #[test]
    fn one_statements_exhausted_cursor_does_not_block_another_statements_execute() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let stmt_b = h.alloc_extra_stmt();
        h.mark_dbc_connected();

        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(tds_client_from_tokens(vec![]));
            // active_stmt is None: statement A's fetch already released it.
        }
        {
            let stmt_a = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut sa = stmt_a.inner.lock().unwrap();
            sa.set_state(STMT_STATE_CURSOR_OPEN);
            sa.result_set_exhausted = true;
        }

        let stmt_b_handle = unsafe { handle_from_raw::<StmtHandle>(stmt_b) };
        let claimed = crate::api::exec_common::claim_connection(dbc, stmt_b_handle, stmt_b, "test");
        assert!(
            claimed.is_ok(),
            "statement B must claim the connection instead of seeing HY000 busy"
        );

        // Put B's claim in place, as the real execute path would, then confirm
        // A can still report SQL_NO_DATA without contending for the connection.
        dbc.inner.lock().unwrap().active_stmt = Some(stmt_b);
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(
            rc, SQL_NO_DATA,
            "A's exhausted cursor must not contend with B's active claim"
        );
    }

    /// The real-fetch counterpart to the test above: drives an actual
    /// zero-row result set (`SELECT ... WHERE 1=0`) through `sql_fetch_scroll`
    /// → `fill_rowset` → the real `peek_is_safe` branch →
    /// `release_busy_if_row_exhausted`, and observes `active_stmt` become
    /// `None` as a *produced effect* of that real code path rather than a
    /// hand-set precondition. Fails on `main`, which has no early-release
    /// logic at all.
    ///
    /// This is the closest equivalent this crate's test harness can drive:
    /// [`mssql_tds::test_client_support`]'s scripted-token replay has no real
    /// row bytes (see its module doc), so `next_row_cursor` can only ever
    /// return `Ok(false)`/`Err` for it, never `Ok(true)` positioned on an
    /// actual row — a genuine *one*-row variant of this test is not
    /// reachable from mssql-odbc today without extending that harness (in
    /// `mssql-tds`) with a "positioned row" scripted-token variant.
    #[test]
    fn real_zero_row_fetch_through_fill_rowset_releases_active_stmt() {
        let h = TestHandles::with_env_dbc_stmt();
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.set_state(STMT_STATE_CURSOR_OPEN);
            s.column_metadata = int_columns(1);
        }
        h.mark_dbc_connected();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let mut client = tds_client_from_tokens(vec![col_metadata_empty(), done_no_more()]);
        dbc.runtime
            .block_on(client.execute("SELECT 1 WHERE 1=0;".to_string(), ()))
            .unwrap();
        {
            let mut ds = dbc.inner.lock().unwrap();
            ds.client = Some(client);
            ds.active_stmt = Some(h.stmt);
        }

        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };

        assert_eq!(rc, SQL_NO_DATA);
        assert!(
            dbc.inner.lock().unwrap().active_stmt.is_none(),
            "a real fetch through fill_rowset that finds the result set \
             already exhausted must release active_stmt itself (AB#47508) — \
             this never happens on main"
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(stmt.inner.lock().unwrap().result_set_exhausted);
    }

    /// Row-wise binding is not implemented, and reporting HYC00 is better than
    /// filling the application's struct array as if it were column-wise.
    #[test]
    fn row_wise_binding_is_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.row_bind_type = 64; // a row-struct size
        }
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert_eq!(s.diag_records.last().unwrap().sql_state, *b"HYC00");
    }

    /// An application may pass BufferLength 0 for a fixed-width target, so the
    /// stride has to come from the C type in that case.
    #[test]
    fn element_stride_falls_back_to_the_c_type_size() {
        assert_eq!(element_stride(SQL_C_SLONG, 0), 4);
        assert_eq!(element_stride(SQL_C_SBIGINT, 0), 8);
        assert_eq!(element_stride(SQL_C_GUID, 0), 16);
        assert_eq!(element_stride(SQL_C_TYPE_TIMESTAMP, 0), 16);
        assert_eq!(element_stride(SQL_C_SS_TIMESTAMPOFFSET, 0), 20);
        // An explicit buffer length always wins.
        assert_eq!(element_stride(SQL_C_SLONG, 4), 4);
        assert_eq!(element_stride(SQL_C_CHAR, 32), 32);
        // A character target with no buffer length has nowhere to write.
        assert_eq!(element_stride(SQL_C_CHAR, 0), 0);
    }

    /// Each row lands at its own offset in the bound array, which is the whole
    /// point of a block fetch.
    #[test]
    fn bound_values_land_at_their_row_offset() {
        let mut buf = [0i32; 4];
        let mut ind = [0isize as SqlLen; 4];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );
        for (row, value) in [10i32, 20, 30].iter().enumerate() {
            let outcome = unsafe { deliver_bound(&b, row, 0, &ColumnValues::Int(*value)) };
            assert!(matches!(outcome, RowOutcome::Success));
        }
        assert_eq!(buf, [10, 20, 30, 0]);
        assert_eq!(ind[0], 4);
    }

    #[test]
    fn bound_row_writer_delivers_borrowed_character_data() {
        let mut narrow = [0u8; 8];
        let mut narrow_ind = [0 as SqlLen; 1];
        let narrow_binding = binding(
            1,
            SQL_C_CHAR,
            narrow.as_mut_ptr() as SqlPointer,
            narrow.len() as SqlLen,
            narrow_ind.as_mut_ptr(),
        );
        let mut wide = [0u16; 8];
        let mut wide_ind = [0 as SqlLen; 1];
        let wide_binding = binding(
            2,
            SQL_C_WCHAR,
            wide.as_mut_ptr() as SqlPointer,
            std::mem::size_of_val(&wide) as SqlLen,
            wide_ind.as_mut_ptr(),
        );
        let bindings = [narrow_binding, wide_binding];
        let mut writer = BoundRowWriter::new(&bindings, 0, 0);

        writer.write_string(0, Cow::Borrowed(b"hello"), EncodingType::Utf8);
        writer.write_string(1, Cow::Borrowed(b"h\0i\0"), EncodingType::Utf16);

        assert_eq!(&narrow[..6], b"hello\0");
        assert_eq!(narrow_ind[0], 5);
        assert_eq!(&wide[..3], &[b'h' as u16, b'i' as u16, 0]);
        assert_eq!(wide_ind[0], 4);
        assert!(matches!(writer.outcome, RowOutcome::Success));
        assert_eq!(writer.last_column_read, 2);
    }

    #[test]
    fn bound_row_writer_skips_unbound_columns_without_losing_ordinal() {
        let mut first = [0i32; 1];
        let mut third = [0i32; 1];
        let bindings = [
            binding(
                1,
                SQL_C_SLONG,
                first.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            ),
            binding(
                3,
                SQL_C_SLONG,
                third.as_mut_ptr() as SqlPointer,
                0,
                ptr::null_mut(),
            ),
        ];
        let mut writer = BoundRowWriter::new(&bindings, 0, 0);

        writer.write_i32(0, 10);
        writer.write_i32(1, 20);
        writer.write_i32(2, 30);

        assert_eq!(first[0], 10);
        assert_eq!(third[0], 30);
        assert_eq!(writer.last_column_read, 3);
    }

    #[test]
    fn bound_row_writer_matches_established_conversion_path() {
        macro_rules! check {
            ($target:expr, $target_rust_type:ty, $column_value:expr, $write:expr) => {{
                let value = $column_value;
                let mut direct_data = [0xA5_u8; 256];
                let mut baseline_data = direct_data;
                let mut direct_ind = [0xA5_u8; 32];
                let mut baseline_ind = direct_ind;
                let direct_binding = binding(
                    1,
                    $target,
                    unsafe { direct_data.as_mut_ptr().add(1) }.cast(),
                    64,
                    unsafe { direct_ind.as_mut_ptr().add(1) }.cast(),
                );
                let baseline_binding = binding(
                    1,
                    $target,
                    unsafe { baseline_data.as_mut_ptr().add(1) }.cast(),
                    64,
                    unsafe { baseline_ind.as_mut_ptr().add(1) }.cast(),
                );
                let bindings = [direct_binding];
                let mut writer = BoundRowWriter::new(&bindings, 1, 1);

                $write(&mut writer);
                let expected = unsafe { deliver_bound(&baseline_binding, 1, 1, &value) };
                let slot_offset = 2 + element_stride($target, 64);
                let direct_value = unsafe {
                    direct_data
                        .as_ptr()
                        .add(slot_offset)
                        .cast::<$target_rust_type>()
                        .read_unaligned()
                };
                let baseline_value = unsafe {
                    baseline_data
                        .as_ptr()
                        .add(slot_offset)
                        .cast::<$target_rust_type>()
                        .read_unaligned()
                };

                assert_eq!(writer.outcome, expected);
                assert_eq!(direct_value, baseline_value);
                assert_eq!(direct_ind, baseline_ind);
            }};
        }

        check!(
            SQL_C_BIT,
            u8,
            ColumnValues::Bit(true),
            |w: &mut BoundRowWriter<'_>| w.write_bool(0, true)
        );
        check!(
            SQL_C_UTINYINT,
            u8,
            ColumnValues::TinyInt(0xFE),
            |w: &mut BoundRowWriter<'_>| w.write_u8(0, 0xFE)
        );
        check!(
            SQL_C_SSHORT,
            i16,
            ColumnValues::SmallInt(-1234),
            |w: &mut BoundRowWriter<'_>| w.write_i16(0, -1234)
        );
        check!(
            SQL_C_SLONG,
            i32,
            ColumnValues::Int(-123_456),
            |w: &mut BoundRowWriter<'_>| w.write_i32(0, -123_456)
        );
        check!(
            SQL_C_SBIGINT,
            i64,
            ColumnValues::BigInt(-9_876_543_210),
            |w: &mut BoundRowWriter<'_>| w.write_i64(0, -9_876_543_210)
        );
        check!(
            SQL_C_FLOAT,
            f32,
            ColumnValues::Real(12.5),
            |w: &mut BoundRowWriter<'_>| w.write_f32(0, 12.5)
        );
        check!(
            SQL_C_DOUBLE,
            f64,
            ColumnValues::Float(-42.25),
            |w: &mut BoundRowWriter<'_>| w.write_f64(0, -42.25)
        );

        let date = SqlDate::create(738_000).unwrap();
        check!(
            SQL_C_TYPE_DATE,
            SqlDateStruct,
            ColumnValues::Date(date.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_date(0, date.clone())
        );
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::Date(date.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_date(0, date.clone())
        );
        let time = SqlTime {
            time_nanoseconds: 45_296_123_456_700,
            scale: 7,
        };
        check!(
            SQL_C_SS_TIME2,
            SqlSsTime2Struct,
            ColumnValues::Time(time.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_time(0, time.clone())
        );
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::Time(time.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_time(0, time.clone())
        );
        let datetime2 = SqlDateTime2 {
            days: 738_000,
            time: time.clone(),
        };
        check!(
            SQL_C_TYPE_TIMESTAMP,
            SqlTimestampStruct,
            ColumnValues::DateTime2(datetime2.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_datetime2(0, datetime2.clone())
        );
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::DateTime2(datetime2.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_datetime2(0, datetime2.clone())
        );
        let datetimeoffset = SqlDateTimeOffset {
            datetime2: datetime2.clone(),
            offset: -420,
        };
        check!(
            SQL_C_SS_TIMESTAMPOFFSET,
            SqlSsTimestampoffsetStruct,
            ColumnValues::DateTimeOffset(datetimeoffset.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_datetimeoffset(0, datetimeoffset.clone())
        );
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::DateTimeOffset(datetimeoffset.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_datetimeoffset(0, datetimeoffset.clone())
        );
        let uuid = Uuid::from_u128(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
        check!(
            SQL_C_GUID,
            SqlGuid,
            ColumnValues::Uuid(uuid),
            |w: &mut BoundRowWriter<'_>| w.write_uuid(0, uuid)
        );
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::Uuid(uuid),
            |w: &mut BoundRowWriter<'_>| w.write_uuid(0, uuid)
        );

        let decimal = DecimalParts::new(true, 18, 4, 1_234_500);
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::Decimal(decimal),
            |w: &mut BoundRowWriter<'_>| w.write_decimal(0, decimal)
        );
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::Numeric(decimal),
            |w: &mut BoundRowWriter<'_>| w.write_numeric(0, decimal)
        );
        check!(
            SQL_C_SBIGINT,
            i64,
            ColumnValues::Int(-123_456),
            |w: &mut BoundRowWriter<'_>| w.write_i32(0, -123_456)
        );
        check!(
            SQL_C_SSHORT,
            i16,
            ColumnValues::BigInt(i64::MAX),
            |w: &mut BoundRowWriter<'_>| w.write_i64(0, i64::MAX)
        );
        check!(
            SQL_C_SLONG,
            i32,
            ColumnValues::Null,
            |w: &mut BoundRowWriter<'_>| w.write_null(0)
        );
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::Bytes(vec![1, 2, 3]),
            |w: &mut BoundRowWriter<'_>| w.write_bytes(0, Cow::Borrowed(&[1, 2, 3]))
        );
        let datetime = SqlDateTime {
            days: 45_000,
            time: 12_345,
        };
        check!(
            SQL_C_TYPE_TIMESTAMP,
            SqlTimestampStruct,
            ColumnValues::DateTime(datetime.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_datetime(0, datetime.clone())
        );
        let smalldatetime = SqlSmallDateTime {
            days: 45_000,
            time: 754,
        };
        check!(
            SQL_C_TYPE_TIMESTAMP,
            SqlTimestampStruct,
            ColumnValues::SmallDateTime(smalldatetime.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_smalldatetime(0, smalldatetime.clone())
        );
        let money = SqlMoney {
            lsb_part: 123_450,
            msb_part: 0,
        };
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::Money(money.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_money(0, money.clone())
        );
        let smallmoney = SqlSmallMoney { int_val: -123_450 };
        check!(
            SQL_C_CHAR,
            [u8; 64],
            ColumnValues::SmallMoney(smallmoney.clone()),
            |w: &mut BoundRowWriter<'_>| w.write_smallmoney(0, smallmoney.clone())
        );
    }

    #[test]
    fn bound_row_writer_ignores_values_without_a_matching_binding() {
        let mut value = 0_i32;
        let bindings = [binding(
            1,
            SQL_C_SLONG,
            (&mut value as *mut i32).cast(),
            0,
            ptr::null_mut(),
        )];
        let mut writer = BoundRowWriter::new(&bindings, 0, 0);

        writer.write_i32(1, 42);
        writer.write_string(2, Cow::Borrowed(b"unused"), EncodingType::Utf8);
        writer.write_xml(3, SqlXml::from("<unused/>".to_string()));
        writer.write_json(4, SqlJson::new(b"{}".to_vec()));
        writer.write_vector(5, SqlVector::try_from_f32(vec![1.0]).unwrap());
        writer.write_variant_base_type(2, TdsDataType::Int4);
        writer.end_row();

        assert_eq!(value, 0);
    }

    #[test]
    fn bound_row_writer_covers_encoded_string_fallback_and_truncation() {
        let mut invalid = [0_u8; 8];
        let invalid_binding = binding(
            1,
            SQL_C_CHAR,
            invalid.as_mut_ptr().cast(),
            invalid.len() as SqlLen,
            ptr::null_mut(),
        );
        let bindings = [invalid_binding];
        let mut writer = BoundRowWriter::new(&bindings, 0, 0);
        writer.write_string(0, Cow::Borrowed(&[0xFF]), EncodingType::Utf8);
        assert_eq!(
            writer.outcome,
            RowOutcome::Error(RowIssue::InvalidCharacter)
        );

        let mut probe = [0_u16; 1];
        let mut probe_indicator = 0;
        let probe_binding = binding(
            1,
            SQL_C_WCHAR,
            probe.as_mut_ptr().cast(),
            0,
            &mut probe_indicator,
        );
        let bindings = [probe_binding];
        let mut writer = BoundRowWriter::new(&bindings, 0, 0);
        writer.write_string(0, Cow::Borrowed(b"h\0"), EncodingType::Utf16);
        assert_eq!(writer.outcome, RowOutcome::Info(RowIssue::StringTruncated));
        assert_eq!(probe_indicator, 2);

        let mut truncated = [0_u16; 2];
        let truncated_binding = binding(
            1,
            SQL_C_WCHAR,
            truncated.as_mut_ptr().cast(),
            std::mem::size_of_val(&truncated) as SqlLen,
            ptr::null_mut(),
        );
        let bindings = [truncated_binding];
        let mut writer = BoundRowWriter::new(&bindings, 0, 0);
        writer.write_string(0, Cow::Borrowed(b"h\0i\0"), EncodingType::Utf16);
        assert_eq!(truncated, [u16::from(b'h'), 0]);
        assert_eq!(writer.outcome, RowOutcome::Info(RowIssue::StringTruncated));
    }

    /// NULL is reported through the indicator; the data slot is left alone for
    /// a fixed-width target.
    #[test]
    fn null_is_reported_through_the_indicator() {
        let mut buf = [7i32; 2];
        let mut ind = [0 as SqlLen; 2];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );
        let outcome = unsafe { deliver_bound(&b, 1, 0, &ColumnValues::Null) };
        assert!(matches!(outcome, RowOutcome::Success));
        assert_eq!(ind[1], SQL_NULL_DATA);
        assert_eq!(buf[1], 7, "a NULL must not disturb the data slot");
    }

    /// A bound column gets one shot at a fixed buffer, so an over-long value is
    /// truncated and reported rather than chunked the way SQLGetData does it.
    #[test]
    fn character_data_is_truncated_to_the_bound_buffer() {
        let mut buf = [0u8; 8];
        let mut ind = [0 as SqlLen; 1];
        let b = binding(
            1,
            SQL_C_CHAR,
            buf.as_mut_ptr() as SqlPointer,
            8,
            ind.as_mut_ptr(),
        );
        let value = ColumnValues::Int(1234567890);
        let outcome = unsafe { deliver_bound(&b, 0, 0, &value) };
        assert!(matches!(
            outcome,
            RowOutcome::Info(RowIssue::StringTruncated)
        ));
        // The indicator reports the untruncated length.
        assert_eq!(ind[0], 10);
        assert_eq!(&buf[..7], b"1234567");
        assert_eq!(buf[7], 0, "the buffer stays NUL-terminated");
    }

    /// The bound-PLP indicator rule, pinned in process. Every case here was
    /// first observed on msodbcsql; the e2e suite proves the wiring, this
    /// proves the decision without needing a live server.
    #[test]
    fn plp_indicator_matches_msodbcsql() {
        // Everything arrived: the produced length is exact, whether or not the
        // bytes were transcoded on the way.
        assert_eq!(plp_indicator(10, false, true, Some(20)), 10);
        assert_eq!(plp_indicator(10, false, false, Some(10)), 10);

        // Truncated, and the delivered unit matches the wire's: the full length
        // is knowable, so it is reported.
        assert_eq!(plp_indicator(30, true, false, Some(5000)), 5000);
        assert_eq!(plp_indicator(30, true, false, Some(10000)), 10000);

        // Truncated while transcoding: wire bytes are the wrong unit and the
        // produced count is not the total, so neither can be reported.
        assert_eq!(plp_indicator(31, true, true, Some(10000)), SQL_NO_TOTAL);

        // A streamed value of unknown length has no total to report either.
        assert_eq!(plp_indicator(31, true, false, None), SQL_NO_TOTAL);
    }

    #[test]
    fn utf8_truncation_drops_a_partial_character() {
        for mut bytes in [
            b"abc\xc3".to_vec(),
            b"abc\xe4\xbd".to_vec(),
            b"abc\xf0\x9f\x98".to_vec(),
        ] {
            trim_partial_utf8(&mut bytes);
            assert_eq!(bytes, b"abc");
        }

        let mut complete = "abc\u{1f600}".as_bytes().to_vec();
        trim_partial_utf8(&mut complete);
        assert_eq!(complete, "abc\u{1f600}".as_bytes());
    }

    /// A zero-length character binding is a length probe, as it is for
    /// SQLGetData: report what is available, write nothing, flag truncation.
    #[test]
    fn a_zero_length_character_binding_probes_the_length() {
        let mut buf = [b'#'; 4];
        let mut ind = [-999 as SqlLen; 1];
        let b = binding(
            1,
            SQL_C_CHAR,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );

        let value = ColumnValues::String(SqlString::new(b"hello".to_vec(), EncodingType::Utf8));
        let outcome = unsafe { deliver_bound(&b, 0, 0, &value) };

        assert!(matches!(
            outcome,
            RowOutcome::Info(RowIssue::StringTruncated)
        ));
        assert_eq!(ind[0], 5, "the available length is reported");
        assert_eq!(buf, [b'#'; 4], "a zero-length buffer is never written");
    }

    /// A drained cursor is not an error: the result set ended on an earlier
    /// fetch and the cursor stays open until it is explicitly closed. The rowset
    /// counters still have to be written so the caller sees zero rows.
    #[test]
    fn fetch_after_the_cursor_drained_reports_an_empty_rowset() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        let mut rows_fetched: SqlULen = 999;
        let mut status = [SQL_ROW_SUCCESS; 3];
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            s.row_array_size = 3;
            s.rows_fetched_ptr = &mut rows_fetched;
            s.row_status_ptr = status.as_mut_ptr();
            s.column_metadata = int_columns(1);
        }
        // active_stmt stays None: an earlier fetch drained the connection.
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_NO_DATA);
        assert_eq!(rows_fetched, 0);
        assert_eq!(status, [SQL_ROW_NOROW; 3]);
    }

    /// The connection can only serve one statement's results at a time.
    #[test]
    fn fetch_while_another_statement_owns_the_connection_is_rejected() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();
        open_cursor(&h);
        {
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let mut d = dbc.inner.lock().unwrap();
            d.active_stmt = Some(other_stmt);
        }
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        assert_eq!(
            s.diag_records.last().unwrap().sql_state,
            ERR_CONNECTION_BUSY.state
        );
    }

    /// A statement positioned on a no-row result (DDL / DML / PRINT) has no
    /// columns to fetch, which is 24000 rather than an empty rowset.
    #[test]
    fn fetch_on_a_no_column_result_is_a_cursor_state_error() {
        let h = TestHandles::with_env_dbc_stmt();
        open_cursor(&h);
        {
            let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let mut d = dbc.inner.lock().unwrap();
            d.active_stmt = Some(h.stmt);
        }
        let rc = unsafe { sql_fetch_scroll(h.stmt, SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let s = stmt.inner.lock().unwrap();
        // No TDS client is attached either, so the no-client guard fires first;
        // both are cursor-state failures rather than a silent empty rowset.
        assert!(matches!(
            s.diag_records.last().unwrap().sql_state,
            SQLSTATE_HY000 | SQLSTATE_24000
        ));
    }

    /// The exported entry point is what the Driver Manager calls, so it needs
    /// its own guard against a null handle.
    #[test]
    fn the_exported_entry_point_rejects_a_null_handle() {
        let rc =
            unsafe { crate::api::exports::SQLFetchScroll(std::ptr::null_mut(), SQL_FETCH_NEXT, 0) };
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    /// ODBC ignores BufferLength for a fixed-width target, so the stride has to
    /// come from the C type. Honouring a bogus length would place later rows
    /// outside the application's array.
    #[test]
    fn fixed_width_stride_ignores_the_buffer_length() {
        // A caller that passes the whole array size rather than one element.
        assert_eq!(element_stride(SQL_C_SLONG, 400), 4);
        assert_eq!(element_stride(SQL_C_SBIGINT, 1), 8);
        assert_eq!(element_stride(SQL_C_TYPE_TIMESTAMP, 999), 16);
        // Character and binary targets are sized by the application.
        assert_eq!(element_stride(SQL_C_CHAR, 32), 32);
        assert_eq!(element_stride(SQL_C_WCHAR, 64), 64);
        // A negative length cannot become a huge stride.
        assert_eq!(element_stride(SQL_C_CHAR, -8), 0);
    }

    /// NULL with nowhere to report it is 22002: leaving the slot untouched
    /// would read back as the previous row's value with no way to tell.
    #[test]
    fn null_without_an_indicator_is_an_error() {
        let mut buf = [7i32; 1];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ptr::null_mut(),
        );
        let outcome = unsafe { deliver_bound(&b, 0, 0, &ColumnValues::Null) };
        assert_eq!(outcome.issue(), Some(RowIssue::IndicatorRequired));
        assert_eq!(outcome.status(), SQL_ROW_ERROR);
        assert_eq!(
            buf[0], 7,
            "the stale value is left visible, not overwritten"
        );
    }

    /// A non-NULL value still delivers without an indicator; only NULL needs one.
    #[test]
    fn a_value_without_an_indicator_still_delivers() {
        let mut buf = [0i32; 1];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ptr::null_mut(),
        );
        let outcome = unsafe { deliver_bound(&b, 0, 0, &ColumnValues::Int(42)) };
        assert!(matches!(outcome, RowOutcome::Success));
        assert_eq!(buf[0], 42);
    }

    /// SQL_ATTR_ROW_BIND_OFFSET_PTR displaces the data and indicator bases by
    /// the same byte count, so the application can move a whole rowset.
    #[test]
    fn the_bind_offset_displaces_both_bases() {
        let mut buf = [0i32; 4];
        let mut ind = [0 as SqlLen; 4];
        let b = binding(
            1,
            SQL_C_SLONG,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );
        // A whole-rowset displacement, which is what the attribute is for: a
        // byte count that leaves both arrays naturally aligned.
        let offset = std::mem::size_of::<SqlLen>();
        let outcome = unsafe { deliver_bound(&b, 0, offset, &ColumnValues::Int(99)) };
        assert!(matches!(outcome, RowOutcome::Success));
        assert_eq!(buf[0], 0, "the offset must skip past the first slots");
        assert_eq!(buf[offset / std::mem::size_of::<i32>()], 99);
        assert_eq!(ind[0], 0, "the indicator base moves too");
        assert_eq!(ind[offset / std::mem::size_of::<SqlLen>()], 4);
    }

    #[test]
    fn a_zero_offset_reads_as_zero_from_a_null_pointer() {
        assert_eq!(unsafe { read_bind_offset(ptr::null_mut()) }, 0);
        let mut value: SqlULen = 24;
        assert_eq!(unsafe { read_bind_offset(&mut value) }, 24);
    }

    /// Each conversion failure keeps its own SQLSTATE rather than being
    /// flattened into one truncation warning.
    #[test]
    fn conversion_failures_map_to_their_own_sqlstate() {
        let mut buf = [0u8; 1];
        let mut ind = [0 as SqlLen; 1];
        // A bigint that cannot fit a tinyint target is 22003.
        let b = binding(
            1,
            SQL_C_TINYINT,
            buf.as_mut_ptr() as SqlPointer,
            0,
            ind.as_mut_ptr(),
        );
        let outcome = unsafe { deliver_bound(&b, 0, 0, &ColumnValues::BigInt(i64::MAX)) };
        assert_eq!(outcome.issue(), Some(RowIssue::OutOfRange));

        // A target this driver does not deliver is HYC00, not a truncation.
        let mut bin = [0u8; 8];
        let unsupported = binding(
            1,
            SQL_C_BINARY,
            bin.as_mut_ptr() as SqlPointer,
            8,
            ind.as_mut_ptr(),
        );
        let outcome = unsafe { deliver_bound(&unsupported, 0, 0, &ColumnValues::Int(1)) };
        assert_eq!(outcome.issue(), Some(RowIssue::Unsupported));
    }

    /// Each issue posts the SQLSTATE the same value would have produced through
    /// SQLGetData.
    #[test]
    fn each_issue_posts_its_own_sqlstate() {
        let cases: &[(RowIssue, [u8; 5])] = &[
            (RowIssue::StringTruncated, *b"01004"),
            (RowIssue::FractionalTruncated, *b"01S07"),
            (RowIssue::OutOfRange, *b"22003"),
            (RowIssue::Restricted, *b"07006"),
            (RowIssue::InvalidCharacter, *b"22018"),
            (RowIssue::IndicatorRequired, *b"22002"),
            (RowIssue::Unsupported, *b"HYC00"),
        ];
        for (issue, state) in cases {
            let h = TestHandles::with_env_dbc_stmt();
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let mut s = stmt.inner.lock().unwrap();
            issue.post(&mut s);
            assert_eq!(
                s.diag_records.last().unwrap().sql_state,
                *state,
                "{issue:?}"
            );
        }
    }

    /// The rows the fetch did not fill must be marked, or the application reads
    /// stale statuses from a previous, longer rowset.
    #[test]
    fn unfilled_rows_are_marked_norow() {
        let mut status = [SQL_ROW_SUCCESS; 5];
        mark_no_rows(status.as_mut_ptr(), 2, 5);
        assert_eq!(
            status,
            [
                SQL_ROW_SUCCESS,
                SQL_ROW_SUCCESS,
                SQL_ROW_NOROW,
                SQL_ROW_NOROW,
                SQL_ROW_NOROW
            ]
        );
    }

    #[test]
    fn row_status_and_outcome_merge_keeps_the_worst() {
        let info = RowOutcome::Info(RowIssue::StringTruncated);
        let err = RowOutcome::Error(RowIssue::OutOfRange);
        assert_eq!(RowOutcome::Success.status(), SQL_ROW_SUCCESS);
        assert_eq!(info.status(), SQL_ROW_SUCCESS_WITH_INFO);
        assert_eq!(err.status(), SQL_ROW_ERROR);
        assert!(matches!(
            RowOutcome::Success.merge(info),
            RowOutcome::Info(_)
        ));
        assert!(matches!(info.merge(err), RowOutcome::Error(_)));
        assert!(matches!(
            err.merge(RowOutcome::Success),
            RowOutcome::Error(_)
        ));
        // An issue survives being merged with a clean row from either side.
        assert!(matches!(
            info.merge(RowOutcome::Success),
            RowOutcome::Info(_)
        ));
        assert!(matches!(
            RowOutcome::Success.merge(RowOutcome::Success),
            RowOutcome::Success
        ));
        // The reason survives the merge so the statement can report it.
        assert_eq!(info.merge(err).issue(), Some(RowIssue::OutOfRange));
        assert_eq!(RowOutcome::Success.issue(), None);
    }
}
