// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Populates the IRD (Implementation Row Descriptor) from result-set column
//! metadata after a successful execute or `SQLMoreResults` advance.
//!
//! AB#47437: before this, the IRD stayed permanently empty and
//! `SQLGetDescFieldW`/`SQLGetDescRecW` against it could never see a result
//! set's columns. Every field here is computed with the exact same mapping
//! functions `SQLDescribeColW` (`describe_col.rs`) and `SQLColAttributeW`
//! (`col_attribute.rs`) already use, so the IRD cannot disagree with either
//! of them for the same column.

use tracing::error;

use mssql_tds::query::metadata::ColumnMetadata;

use crate::api::col_attribute::{desc_length, octet_length, precision};
use crate::api::describe_col::{decimal_digits, odbc_sql_type};
use crate::api::odbc_types::{SQL_NO_NULLS, SQL_NULLABLE, SqlSmallInt, SqlULen};
use crate::api::set_desc_field::datetime_interval_code_for;
use crate::handles::desc::DescRecord;
use crate::handles::{DescHandle, StmtHandle, handle_from_raw};

/// Rewrites `stmt`'s IRD to describe `metadata`, growing or shrinking its
/// record count to match. Call after the STMT lock that advanced
/// `column_metadata` has already been dropped — this crate never holds a
/// STMT lock while acquiring a DESC lock (see bind_col.rs's rationale).
///
/// Wired into every place `column_metadata` advances to a new result set
/// (`finish_execute`, `SQLMoreResults`'s three advancing arms), including
/// with empty `metadata` for a no-column result — but deliberately *not*
/// into `reset_cursor_state`'s cursor-close path (`SQLCloseCursor`,
/// `SQLFreeStmt(SQL_CLOSE)`, and `SQLMoreResults`'s batch-end/error arms).
/// Those already leave the IRD's data stale rather than emptied, matching
/// the pre-existing gap that `get_desc_field.rs`/`set_desc_field.rs` answer
/// every descriptor kind — IRD included — with no awareness of the owning
/// statement's cursor state at all: `SQLDescribeColW`/`SQLColAttributeW`
/// already refuse a closed cursor via `STMT_STATE_EXEC_CONTEXT` before ever
/// reading `column_metadata`, so a well-behaved caller has no path to
/// observe stale IRD data through the equivalent descriptor-field call
/// either.
///
/// A poisoned IRD mutex is logged and otherwise ignored: the IRD is a
/// read-only convenience view of `column_metadata`, so failing to refresh it
/// does not affect `SQLDescribeColW`/`SQLColAttributeW`/fetch, which read
/// `column_metadata` directly and never consult the IRD.
pub(super) fn populate_ird(stmt: &StmtHandle, metadata: &[ColumnMetadata]) {
    let desc = unsafe { handle_from_raw::<DescHandle>(stmt.ird) };
    let Ok(mut desc_state) = desc.inner.lock() else {
        error!("populating IRD: mutex poisoned; result-set metadata left stale");
        return;
    };
    desc_state.set_record_count(metadata.len(), desc.kind);
    for (i, meta) in metadata.iter().enumerate() {
        let record_number = SqlSmallInt::try_from(i + 1).unwrap_or(SqlSmallInt::MAX);
        if let Some(record) = desc_state.record_mut(record_number) {
            *record = ird_record_from_metadata(meta);
        }
    }
}

fn ird_record_from_metadata(meta: &ColumnMetadata) -> DescRecord {
    let concise_type = odbc_sql_type(meta);
    DescRecord {
        concise_type,
        datetime_interval_code: datetime_interval_code_for(concise_type),
        length: SqlULen::try_from(desc_length(meta)).unwrap_or(0),
        octet_length: octet_length(meta),
        precision: precision(meta),
        scale: decimal_digits(meta),
        nullable: if meta.is_nullable() {
            SQL_NULLABLE
        } else {
            SQL_NO_NULLS
        },
        name: meta.column_name.clone(),
        parameter_type: 0,
        data_ptr: std::ptr::null_mut(),
        indicator_ptr: std::ptr::null_mut(),
        octet_length_ptr: std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_INTEGER;
    use crate::test_support::TestHandles;
    use mssql_tds::test_client_support::int_columns;

    fn ird_records(h: &TestHandles) -> Vec<DescRecord> {
        let desc = unsafe { handle_from_raw::<DescHandle>(h.ird()) };
        desc.inner.lock().unwrap().records.clone()
    }

    /// Every field lands where `SQLDescribeColW`/`SQLColAttributeW` would
    /// report it for the same column, since they share these exact mapping
    /// functions.
    #[test]
    fn populate_ird_writes_records_consistent_with_describe_col() {
        let h = TestHandles::with_env_dbc_stmt();
        let metadata = int_columns(2);
        populate_ird(unsafe { handle_from_raw::<StmtHandle>(h.stmt) }, &metadata);

        let records = ird_records(&h);
        assert_eq!(records.len(), 2);
        for (i, record) in records.iter().enumerate() {
            assert_eq!(record.concise_type, SQL_INTEGER);
            assert_eq!(record.nullable, SQL_NULLABLE);
            assert_eq!(record.name, format!("c{}", i + 1));
            assert_eq!(record.datetime_interval_code, 0, "int is not a datetime");
        }
    }

    /// A later result set with fewer columns must shrink the IRD, not just
    /// overwrite the first N records and leave stale trailing ones behind.
    #[test]
    fn populate_ird_shrinks_to_a_narrower_later_result_set() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        populate_ird(stmt, &int_columns(3));
        assert_eq!(ird_records(&h).len(), 3);

        populate_ird(stmt, &int_columns(1));
        assert_eq!(ird_records(&h).len(), 1);
    }

    /// A no-column result (DDL/DML, or a statement-wise no-row result) must
    /// empty the IRD, not just leave a stale previous result set's columns
    /// behind.
    #[test]
    fn populate_ird_with_empty_metadata_empties_the_ird() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        populate_ird(stmt, &int_columns(2));
        assert_eq!(ird_records(&h).len(), 2);

        populate_ird(stmt, &[]);
        assert!(ird_records(&h).is_empty());
    }
}
