// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Descriptor header/record data model (ARD/APD/IRD/IPD).
//!
//! One shared record shape (`DescRecord`) and header shape (`DescHeader`)
//! serve all four descriptor kinds, mirroring msodbcsql's common
//! `GENDESCTAG` header / `cpbaseTag` record bases plus kind-specific
//! validation in `SQLGetDescFieldW`/`SQLSetDescFieldW`
//! (`Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp`) rather than four parallel record
//! types. [`classify_field`] is that validation, collapsed into one table so
//! the get/set entry points share a single source of truth for which fields
//! apply to which kind.
//!
//! Scope note: IRD/IPD records here are the driver's own descriptor storage,
//! independent of `StmtState::column_metadata` / `parameter_metadata`.
//! Reconciling the two (populating IRD from a result set, IPD from
//! `SQLDescribeParam`) is AB#47437, not this module — until then, IRD/IPD
//! records exist and behave correctly per the ODBC field-access contract, but
//! start out empty (`SQL_DESC_COUNT == 0`) rather than reflecting a live
//! query. Catalog-style descriptive fields (`SQL_DESC_LABEL`,
//! `TABLE_NAME`/`CATALOG_NAME`/`SCHEMA_NAME`, `LITERAL_PREFIX`/`SUFFIX`,
//! `LOCAL_TYPE_NAME`, `TYPE_NAME`, `SEARCHABLE`, `UPDATABLE`,
//! `CASE_SENSITIVE`, `AUTO_UNIQUE_VALUE`, `FIXED_PREC_SCALE`, `UNSIGNED`,
//! `NUM_PREC_RADIX`, `DISPLAY_SIZE`) are likewise out of scope here: they
//! remain answerable via the already-implemented `SQLColAttributeW`
//! (`api::col_attribute`), and duplicating that mapping into descriptor
//! storage ahead of the AB#47437 IRD-population design would risk the two
//! diverging.
//!
//! Same scope note applies to four `DescHeader` fields the ODBC spec defines
//! as *aliases* of statement attributes rather than as independent storage:
//! `SQL_DESC_ARRAY_SIZE` (`SQL_ATTR_ROW_ARRAY_SIZE` / `PARAMSET_SIZE`),
//! `SQL_DESC_BIND_TYPE` (`SQL_ATTR_ROW_BIND_TYPE`), `SQL_DESC_ARRAY_STATUS_PTR`
//! (`SQL_ATTR_ROW_STATUS_PTR`), and `SQL_DESC_ROWS_PROCESSED_PTR`
//! (`SQL_ATTR_ROWS_FETCHED_PTR`). `DescHeader` stores these independently of
//! `StmtState`'s equivalent fields (`set_stmt_attr.rs`), so a
//! `SQLSetStmtAttrW`/`SQLGetDescFieldW` pair (or the reverse) on the same
//! logical value currently sees two unaliased copies. Nothing reads
//! `DescHeader`'s copies outside `get_desc_field.rs`/`set_desc_field.rs`
//! today, so this is silent rather than wrong yet — it needs wiring the two
//! together (or picking one as the sole owner) before a block-fetch consumer
//! reads `StmtState`'s copies and the descriptor's view of the same
//! attribute quietly diverges. Tracked under the same AB#47437 aliasing
//! work as the IRD/IPD record population above.

use std::sync::Mutex;

use super::{HandleType, HasObjectType};
use crate::api::odbc_types::{
    SQL_C_DEFAULT, SQL_DESC_ALLOC_AUTO, SQL_DESC_ALLOC_TYPE, SQL_DESC_ARRAY_SIZE,
    SQL_DESC_ARRAY_STATUS_PTR, SQL_DESC_BIND_OFFSET_PTR, SQL_DESC_BIND_TYPE, SQL_DESC_CONCISE_TYPE,
    SQL_DESC_COUNT, SQL_DESC_DATA_PTR, SQL_DESC_DATETIME_INTERVAL_CODE, SQL_DESC_INDICATOR_PTR,
    SQL_DESC_LENGTH, SQL_DESC_NAME, SQL_DESC_NULLABLE, SQL_DESC_OCTET_LENGTH,
    SQL_DESC_OCTET_LENGTH_PTR, SQL_DESC_PARAMETER_TYPE, SQL_DESC_PRECISION,
    SQL_DESC_ROWS_PROCESSED_PTR, SQL_DESC_SCALE, SQL_DESC_TYPE, SQL_DESC_UNNAMED, SQL_NULLABLE,
    SQL_PARAM_INPUT, SQL_ROWSET_SIZE_DEFAULT, SqlInteger, SqlLen, SqlPointer, SqlSmallInt, SqlULen,
    SqlUSmallInt,
};
use crate::error::{DiagRecord, HasDiagnostics};

/// The four automatically-allocated implicit descriptors owned by every
/// statement: application/implementation row/parameter descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DescKind {
    AppRow,
    AppParam,
    ImpRow,
    ImpParam,
}

impl DescKind {
    /// `true` for the two application descriptors (ARD/APD) an application
    /// binds directly; `false` for the driver-owned implementation
    /// descriptors (IRD/IPD). msodbcsql calls the ARD/APD shape `AD` since
    /// both share one record layout (`sqlsrv.h:1546-1557`).
    pub(crate) fn is_application(self) -> bool {
        matches!(self, DescKind::AppRow | DescKind::AppParam)
    }
}

/// Descriptor handle.
#[derive(Debug)]
pub(crate) struct DescHandle {
    pub(crate) object_type: HandleType,
    pub(crate) kind: DescKind,
    pub(crate) inner: Mutex<DescState>,
}

/// Header fields common to every descriptor (`RecNumber == 0` in
/// `SQLGetDescFieldW`/`SQLSetDescFieldW`). Validity per kind is gated by
/// [`classify_field`], not by field presence here — mirrors msodbcsql's
/// shared `GENDESCTAG` (`sqlsrv.h:1167-1178`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DescHeader {
    /// `SQL_DESC_ALLOC_TYPE`. Always `SQL_DESC_ALLOC_AUTO`: every descriptor
    /// is implicit until explicit allocation (`SQLAllocHandle(SQL_HANDLE_DESC,
    /// ...)`, AB#47436) exists.
    pub(crate) alloc_type: SqlSmallInt,
    /// `SQL_DESC_ARRAY_SIZE`. ARD/APD only.
    pub(crate) array_size: SqlULen,
    /// `SQL_DESC_ARRAY_STATUS_PTR`. All kinds.
    pub(crate) array_status_ptr: SqlPointer,
    /// `SQL_DESC_BIND_OFFSET_PTR`. ARD/APD only.
    pub(crate) bind_offset_ptr: SqlPointer,
    /// `SQL_DESC_BIND_TYPE`. ARD/APD only. `SQLINTEGER`-width per the ODBC
    /// descriptor field table (confirmed against msodbcsql's
    /// `GetADHeaderField`, `sqlcdesc.cpp:4060-4063`) — unlike its
    /// statement-attribute twin `SQL_ATTR_ROW_BIND_TYPE`/`SQL_ATTR_PARAM_BIND_TYPE`,
    /// which are `SQLULEN`.
    pub(crate) bind_type: SqlInteger,
    /// `SQL_DESC_ROWS_PROCESSED_PTR`. IRD/IPD only.
    pub(crate) rows_processed_ptr: SqlPointer,
}

impl Default for DescHeader {
    fn default() -> Self {
        Self {
            alloc_type: SQL_DESC_ALLOC_AUTO,
            array_size: SQL_ROWSET_SIZE_DEFAULT,
            array_status_ptr: std::ptr::null_mut(),
            bind_offset_ptr: std::ptr::null_mut(),
            // `SQL_BIND_BY_COLUMN` (0) — the constant is `SqlULen`-typed since
            // it doubles as `SQL_ATTR_ROW_BIND_TYPE`'s value, but this field
            // is `SQLINTEGER`-width (see field doc comment).
            bind_type: 0,
            rows_processed_ptr: std::ptr::null_mut(),
        }
    }
}

/// One descriptor record (`RecNumber >= 1`), 1-based within a descriptor.
/// Shared shape across all four kinds; a field's validity for a given kind
/// is gated by [`classify_field`], mirroring msodbcsql's shared `cpbaseTag`
/// record base (`sqlsrv.h:1146-1165`) plus kind-specific validation rather
/// than four separate record types.
#[derive(Debug, Clone)]
pub(crate) struct DescRecord {
    /// `SQL_DESC_CONCISE_TYPE`. `SQL_DESC_TYPE` (verbose) is derived from
    /// this plus `datetime_interval_code` for the datetime/interval families;
    /// every other type reports the same value for both fields.
    pub(crate) concise_type: SqlSmallInt,
    /// `SQL_DESC_DATETIME_INTERVAL_CODE`. Zero when not a datetime/interval type.
    pub(crate) datetime_interval_code: SqlSmallInt,
    /// `SQL_DESC_LENGTH`.
    pub(crate) length: SqlULen,
    /// `SQL_DESC_OCTET_LENGTH`.
    pub(crate) octet_length: SqlLen,
    /// `SQL_DESC_PRECISION`.
    pub(crate) precision: SqlSmallInt,
    /// `SQL_DESC_SCALE`.
    pub(crate) scale: SqlSmallInt,
    /// `SQL_DESC_NULLABLE`. IRD/IPD only; always get-only.
    pub(crate) nullable: SqlSmallInt,
    /// `SQL_DESC_NAME`. IRD/IPD only; writable on IPD only.
    pub(crate) name: String,
    /// `SQL_DESC_PARAMETER_TYPE`. IPD only. (`SQL_DESC_UNNAMED` is derived
    /// from `name` on read — `SQL_UNNAMED` iff `name` is empty — rather than
    /// stored redundantly.)
    pub(crate) parameter_type: SqlSmallInt,
    /// `SQL_DESC_DATA_PTR`. ARD/APD only: the application buffer address.
    /// Opaque to this module — never dereferenced here, only by the eventual
    /// bind/execute consumer (AB#47437).
    pub(crate) data_ptr: SqlPointer,
    /// `SQL_DESC_INDICATOR_PTR`. ARD/APD only. Opaque, see `data_ptr`.
    pub(crate) indicator_ptr: SqlPointer,
    /// `SQL_DESC_OCTET_LENGTH_PTR`. ARD/APD only. Opaque, see `data_ptr`.
    pub(crate) octet_length_ptr: SqlPointer,
}

impl DescRecord {
    /// A freshly grown record's defaults, keyed by descriptor kind. Mirrors
    /// msodbcsql's `FastSetADRecDefaults` (`fCType = SQL_C_DEFAULT`,
    /// `sqlcdesc.cpp:136-148`) and `FastSetIPDRecDefaults`
    /// (`fParamType = SQL_PARAM_INPUT`, `fParamNullable = SQL_NULLABLE`,
    /// `sqlcdesc.cpp:155-168`). IRD has no analogous default-fill helper in
    /// msodbcsql since it is always populated from result metadata rather
    /// than grown by an application `SQL_DESC_COUNT` write; a freshly grown
    /// IRD record here is simply zeroed, matching an unpopulated column.
    fn default_for(kind: DescKind) -> Self {
        let (concise_type, parameter_type, nullable) = match kind {
            DescKind::AppRow | DescKind::AppParam => (SQL_C_DEFAULT, 0, 0),
            DescKind::ImpParam => (0, SQL_PARAM_INPUT, SQL_NULLABLE),
            DescKind::ImpRow => (0, 0, SQL_NULLABLE),
        };
        Self {
            concise_type,
            datetime_interval_code: 0,
            length: 0,
            octet_length: 0,
            precision: 0,
            scale: 0,
            nullable,
            name: String::new(),
            parameter_type,
            data_ptr: std::ptr::null_mut(),
            indicator_ptr: std::ptr::null_mut(),
            octet_length_ptr: std::ptr::null_mut(),
        }
    }

    /// `SQL_DESC_TYPE`, the verbose form of `concise_type`: the datetime
    /// family (`SQL_TYPE_DATE..=SQL_TYPE_TIMESTAMP`) collapses to
    /// `SQL_DATETIME`, with the member identified by
    /// `datetime_interval_code`; every other type reports its concise value
    /// unchanged.
    ///
    /// Deliberately simpler than msodbcsql's equivalent
    /// (`sqlcdesc.cpp:2226-2243`): msodbcsql stores descriptor types in a
    /// 2.x-era internal representation and remaps on both read and write.
    /// This driver targets ODBC 3.x only
    /// (`.github/instructions/mssql-odbc.instructions.md`) and stores the
    /// 3.x concise value directly, so verbose synthesis is a direct range
    /// check rather than a remap. The ODBC `SQL_INTERVAL_*` family is
    /// likewise not folded to a verbose `SQL_INTERVAL`: SQL Server has no
    /// interval SQL type, so no concise interval value can ever reach a
    /// descriptor record through this driver's execution path.
    pub(crate) fn verbose_type(&self) -> SqlSmallInt {
        use crate::api::odbc_types::{SQL_DATETIME, SQL_TYPE_DATE, SQL_TYPE_TIMESTAMP};
        if (SQL_TYPE_DATE..=SQL_TYPE_TIMESTAMP).contains(&self.concise_type) {
            SQL_DATETIME
        } else {
            self.concise_type
        }
    }
}

#[derive(Debug)]
pub(crate) struct DescState {
    pub(crate) diag_records: Vec<DiagRecord>,
    pub(crate) header: DescHeader,
    /// 1-based descriptor records: `records[0]` is record number 1.
    pub(crate) records: Vec<DescRecord>,
}

impl DescState {
    /// Returns the record at 1-based `record_number`, or `None` if it does
    /// not exist (`record_number < 1` or `> SQL_DESC_COUNT`).
    pub(crate) fn record(&self, record_number: SqlSmallInt) -> Option<&DescRecord> {
        let index = usize::try_from(record_number).ok()?.checked_sub(1)?;
        self.records.get(index)
    }

    /// Mutable counterpart of [`Self::record`].
    pub(crate) fn record_mut(&mut self, record_number: SqlSmallInt) -> Option<&mut DescRecord> {
        let index = usize::try_from(record_number).ok()?.checked_sub(1)?;
        self.records.get_mut(index)
    }

    /// Grows or shrinks the record list to `count`, per `SQL_DESC_COUNT`
    /// write semantics: shrinking discards trailing records; growing
    /// default-initializes only the newly exposed ones. Mirrors msodbcsql's
    /// `AllocPlex`/`FreePlex` (`sqlcdesc.cpp:3752-3901` for AD,
    /// `4318-4463` for IPD): existing records are preserved, not
    /// reinitialized, on either grow or shrink.
    pub(crate) fn set_record_count(&mut self, count: usize, kind: DescKind) {
        if count < self.records.len() {
            self.records.truncate(count);
        } else {
            self.records
                .resize_with(count, || DescRecord::default_for(kind));
        }
    }
}

impl DescHandle {
    pub(crate) fn new(kind: DescKind) -> Self {
        Self {
            object_type: HandleType::Desc,
            kind,
            inner: Mutex::new(DescState {
                diag_records: Vec::new(),
                header: DescHeader::default(),
                records: Vec::new(),
            }),
        }
    }
}

impl HasObjectType for DescHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}

impl HasDiagnostics for DescState {
    fn diag_records(&self) -> &[DiagRecord] {
        &self.diag_records
    }
    fn diag_records_mut(&mut self) -> &mut Vec<DiagRecord> {
        &mut self.diag_records
    }
}

// SAFETY: `DescHeader`/`DescRecord` hold raw pointers
// (`array_status_ptr`, `bind_offset_ptr`, `rows_processed_ptr`, `data_ptr`,
// `indicator_ptr`, `octet_length_ptr`), which prevents auto-derivation of
// `Send`/`Sync` for `DescState` and therefore `DescHandle` (same pattern as
// `StmtHandle`/`BoundParam`, which store the analogous application buffer
// addresses). Every one of these pointers is an opaque application-owned
// address: copied in by `SQLSetDescFieldW`/`SQLSetStmtAttrW`, copied out by
// `SQLGetDescFieldW`/`SQLGetStmtAttrW`, and never dereferenced by this
// module. The Driver Manager may legitimately call ODBC entry points for the
// same handle from different threads (serialized by `inner`'s mutex), so the
// handle itself must be `Send + Sync`.
unsafe impl Send for DescHandle {}
unsafe impl Sync for DescHandle {}

/// Where a `SQL_DESC_*` field's value lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldScope {
    /// `RecordNumber` must be `0`; the value applies to the whole descriptor.
    Header,
    /// `RecordNumber` must be `>= 1`; the value applies to one record.
    Record,
}

/// A field's supported operations for one descriptor kind, as returned by
/// [`classify_field`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FieldAccess {
    pub(crate) scope: FieldScope,
    pub(crate) writable: bool,
}

/// Classifies a `SQL_DESC_*` field identifier for a specific descriptor kind.
///
/// Returns `None` when the field is either not a real descriptor field this
/// driver recognizes, or not valid for `kind` at all — both cases are
/// `HY091` ("Invalid descriptor field identifier") at the call site, per
/// ODBC: that SQLSTATE covers "not one of the defined values... or not
/// defined for the descriptor type". Mirrors msodbcsql's
/// `IsDescriptorHeaderField`/`IsDescriptorRecordField`
/// (`sqlcdesc.cpp:3395-3462`) plus its per-kind `GetXField`/`SetXField`
/// accept-lists, collapsed into one table so `SQLGetDescFieldW` and
/// `SQLSetDescFieldW` share one source of truth instead of duplicating the
/// field enumeration.
///
/// IRD reports every otherwise-readable field as not writable:
/// `SQLSetDescFieldW` rejects every IRD field except
/// `SQL_DESC_ROWS_PROCESSED_PTR`/`SQL_DESC_ARRAY_STATUS_PTR`
/// (`sqlcdesc.cpp:1399-1405`), which this function marks writable for every
/// kind including IRD, matching msodbcsql special-casing those two ahead of
/// the general IRD-is-read-only gate (`sqlcdesc.cpp:1537-1541`).
///
/// Deliberately out of scope (see module docs): catalog-style descriptive
/// fields (`SQL_DESC_LABEL`, `TABLE_NAME`, `TYPE_NAME`, `SEARCHABLE`, etc.)
/// always return `None` here and are reported `HY091`, not because they are
/// invalid ODBC fields, but because this driver answers them through
/// `SQLColAttributeW` today and folding them into descriptor storage is
/// deferred to AB#47437's IRD-population design.
pub(crate) fn classify_field(kind: DescKind, field: SqlUSmallInt) -> Option<FieldAccess> {
    use FieldScope::{Header, Record};

    let is_ad = kind.is_application();
    let is_ird = matches!(kind, DescKind::ImpRow);
    let is_ipd = matches!(kind, DescKind::ImpParam);

    let (scope, writable) = match field {
        // ---- Header fields ----------------------------------------------
        SQL_DESC_ALLOC_TYPE => (Header, false),
        SQL_DESC_COUNT => (Header, !is_ird),
        SQL_DESC_ARRAY_SIZE if is_ad => (Header, true),
        SQL_DESC_ARRAY_STATUS_PTR => (Header, true),
        SQL_DESC_BIND_OFFSET_PTR if is_ad => (Header, true),
        SQL_DESC_BIND_TYPE if is_ad => (Header, true),
        SQL_DESC_ROWS_PROCESSED_PTR if is_ird || is_ipd => (Header, true),

        // ---- Record fields common to every kind -------------------------
        SQL_DESC_TYPE
        | SQL_DESC_CONCISE_TYPE
        | SQL_DESC_DATETIME_INTERVAL_CODE
        | SQL_DESC_LENGTH
        | SQL_DESC_OCTET_LENGTH
        | SQL_DESC_PRECISION
        | SQL_DESC_SCALE => (Record, !is_ird),

        // ---- Record fields specific to application descriptors ----------
        SQL_DESC_DATA_PTR | SQL_DESC_INDICATOR_PTR | SQL_DESC_OCTET_LENGTH_PTR if is_ad => {
            (Record, true)
        }

        // ---- Record fields specific to implementation descriptors -------
        SQL_DESC_NULLABLE if is_ird || is_ipd => (Record, false),
        SQL_DESC_NAME if is_ird || is_ipd => (Record, is_ipd),
        // SQL_DESC_UNNAMED is derived from `name` on read (see
        // `DescRecord::name`'s doc comment) rather than stored separately,
        // but it is writable on IPD: the ODBC reference and msodbcsql's
        // `SetIPDField` (`sqlcdesc.cpp:4873-4884`) both make `SQL_UNNAMED`
        // (and only that value) a valid write that clears the parameter
        // name — see `set_unnamed` in set_desc_field.rs. Read-only on IRD,
        // matching SQL_DESC_NAME's own IRD/IPD split above.
        SQL_DESC_UNNAMED if is_ird || is_ipd => (Record, is_ipd),
        SQL_DESC_PARAMETER_TYPE if is_ipd => (Record, true),

        _ => return None,
    };
    Some(FieldAccess { scope, writable })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_DATETIME, SQL_TYPE_DATE, SQL_TYPE_TIMESTAMP};

    const ALL_KINDS: [DescKind; 4] = [
        DescKind::AppRow,
        DescKind::AppParam,
        DescKind::ImpRow,
        DescKind::ImpParam,
    ];

    #[test]
    fn alloc_type_is_read_only_header_field_everywhere() {
        for kind in ALL_KINDS {
            let access = classify_field(kind, SQL_DESC_ALLOC_TYPE).unwrap();
            assert_eq!(access.scope, FieldScope::Header);
            assert!(!access.writable, "{kind:?}");
        }
    }

    #[test]
    fn count_is_writable_everywhere_except_ird() {
        for kind in ALL_KINDS {
            let access = classify_field(kind, SQL_DESC_COUNT).unwrap();
            assert_eq!(access.scope, FieldScope::Header);
            assert_eq!(access.writable, kind != DescKind::ImpRow, "{kind:?}");
        }
    }

    #[test]
    fn array_size_and_bind_fields_are_ad_only() {
        for field in [
            SQL_DESC_ARRAY_SIZE,
            SQL_DESC_BIND_OFFSET_PTR,
            SQL_DESC_BIND_TYPE,
        ] {
            assert!(classify_field(DescKind::AppRow, field).is_some());
            assert!(classify_field(DescKind::AppParam, field).is_some());
            assert!(classify_field(DescKind::ImpRow, field).is_none());
            assert!(classify_field(DescKind::ImpParam, field).is_none());
        }
    }

    #[test]
    fn rows_processed_ptr_is_implementation_only() {
        assert!(classify_field(DescKind::AppRow, SQL_DESC_ROWS_PROCESSED_PTR).is_none());
        assert!(classify_field(DescKind::AppParam, SQL_DESC_ROWS_PROCESSED_PTR).is_none());
        assert!(classify_field(DescKind::ImpRow, SQL_DESC_ROWS_PROCESSED_PTR).is_some());
        assert!(classify_field(DescKind::ImpParam, SQL_DESC_ROWS_PROCESSED_PTR).is_some());
    }

    #[test]
    fn array_status_ptr_is_writable_on_every_kind_including_ird() {
        for kind in ALL_KINDS {
            let access = classify_field(kind, SQL_DESC_ARRAY_STATUS_PTR).unwrap();
            assert!(access.writable, "{kind:?}");
        }
    }

    #[test]
    fn common_record_fields_are_read_only_on_ird_only() {
        for field in [
            SQL_DESC_TYPE,
            SQL_DESC_CONCISE_TYPE,
            SQL_DESC_DATETIME_INTERVAL_CODE,
            SQL_DESC_LENGTH,
            SQL_DESC_OCTET_LENGTH,
            SQL_DESC_PRECISION,
            SQL_DESC_SCALE,
        ] {
            for kind in ALL_KINDS {
                let access = classify_field(kind, field).unwrap();
                assert_eq!(access.scope, FieldScope::Record);
                assert_eq!(
                    access.writable,
                    kind != DescKind::ImpRow,
                    "{kind:?} {field}"
                );
            }
        }
    }

    #[test]
    fn data_and_indicator_pointer_fields_are_ad_only() {
        for field in [
            SQL_DESC_DATA_PTR,
            SQL_DESC_INDICATOR_PTR,
            SQL_DESC_OCTET_LENGTH_PTR,
        ] {
            assert!(classify_field(DescKind::AppRow, field).unwrap().writable);
            assert!(classify_field(DescKind::AppParam, field).unwrap().writable);
            assert!(classify_field(DescKind::ImpRow, field).is_none());
            assert!(classify_field(DescKind::ImpParam, field).is_none());
        }
    }

    #[test]
    fn nullable_is_read_only_on_ird_and_ipd_only() {
        assert!(classify_field(DescKind::AppRow, SQL_DESC_NULLABLE).is_none());
        assert!(classify_field(DescKind::AppParam, SQL_DESC_NULLABLE).is_none());
        for kind in [DescKind::ImpRow, DescKind::ImpParam] {
            let access = classify_field(kind, SQL_DESC_NULLABLE).unwrap();
            assert!(!access.writable, "{kind:?}");
        }
    }

    #[test]
    fn name_is_writable_on_ipd_but_not_ird() {
        assert!(classify_field(DescKind::AppRow, SQL_DESC_NAME).is_none());
        assert!(classify_field(DescKind::AppParam, SQL_DESC_NAME).is_none());
        assert!(
            !classify_field(DescKind::ImpRow, SQL_DESC_NAME)
                .unwrap()
                .writable
        );
        assert!(
            classify_field(DescKind::ImpParam, SQL_DESC_NAME)
                .unwrap()
                .writable
        );
    }

    /// Regression: `SQL_DESC_UNNAMED` is derived from `name` on read
    /// (`DescRecord`'s doc comment) but is writable on IPD to `SQL_UNNAMED`
    /// — the ODBC reference and msodbcsql's `SetIPDField` both make this the
    /// one legal write for the field (see `set_unnamed` in
    /// set_desc_field.rs). Read-only on IRD and on the application
    /// descriptors, where the field isn't valid at all.
    #[test]
    fn unnamed_is_writable_only_on_ipd() {
        assert!(classify_field(DescKind::AppRow, SQL_DESC_UNNAMED).is_none());
        assert!(classify_field(DescKind::AppParam, SQL_DESC_UNNAMED).is_none());
        assert!(
            !classify_field(DescKind::ImpRow, SQL_DESC_UNNAMED)
                .unwrap()
                .writable
        );
        assert!(
            classify_field(DescKind::ImpParam, SQL_DESC_UNNAMED)
                .unwrap()
                .writable
        );
    }

    #[test]
    fn parameter_type_is_ipd_only() {
        assert!(classify_field(DescKind::AppRow, SQL_DESC_PARAMETER_TYPE).is_none());
        assert!(classify_field(DescKind::AppParam, SQL_DESC_PARAMETER_TYPE).is_none());
        assert!(classify_field(DescKind::ImpRow, SQL_DESC_PARAMETER_TYPE).is_none());
        assert!(
            classify_field(DescKind::ImpParam, SQL_DESC_PARAMETER_TYPE)
                .unwrap()
                .writable
        );
    }

    #[test]
    fn unknown_field_id_is_none_for_every_kind() {
        for kind in ALL_KINDS {
            assert!(classify_field(kind, 0xFFFF).is_none());
        }
    }

    #[test]
    fn new_descriptor_starts_with_no_records_and_default_header() {
        let handle = DescHandle::new(DescKind::AppParam);
        let state = handle.inner.lock().unwrap();
        assert!(state.records.is_empty());
        assert_eq!(state.header.alloc_type, SQL_DESC_ALLOC_AUTO);
        assert_eq!(state.header.array_size, SQL_ROWSET_SIZE_DEFAULT);
    }

    #[test]
    fn set_record_count_grows_with_kind_defaults() {
        let mut state = DescState {
            diag_records: Vec::new(),
            header: DescHeader::default(),
            records: Vec::new(),
        };
        state.set_record_count(3, DescKind::AppParam);
        assert_eq!(state.records.len(), 3);
        for record in &state.records {
            assert_eq!(record.concise_type, SQL_C_DEFAULT);
        }

        // Fresh state: growing an IPD default-fills the IPD-specific fields
        // (mixing kinds on one growing state isn't a real scenario — a
        // descriptor's kind never changes after creation).
        let mut ipd_state = DescState {
            diag_records: Vec::new(),
            header: DescHeader::default(),
            records: Vec::new(),
        };
        ipd_state.set_record_count(1, DescKind::ImpParam);
        assert_eq!(ipd_state.records.len(), 1);
        assert_eq!(ipd_state.records[0].parameter_type, SQL_PARAM_INPUT);
        assert_eq!(ipd_state.records[0].nullable, SQL_NULLABLE);
    }

    #[test]
    fn set_record_count_shrink_discards_trailing_records_and_preserves_the_rest() {
        let mut state = DescState {
            diag_records: Vec::new(),
            header: DescHeader::default(),
            records: Vec::new(),
        };
        state.set_record_count(3, DescKind::AppRow);
        if let Some(r) = state.record_mut(1) {
            r.concise_type = 42;
        }
        state.set_record_count(1, DescKind::AppRow);
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.record(1).unwrap().concise_type, 42);
        assert!(state.record(2).is_none());
    }

    #[test]
    fn record_and_record_mut_reject_zero_and_negative() {
        let mut state = DescState {
            diag_records: Vec::new(),
            header: DescHeader::default(),
            records: Vec::new(),
        };
        state.set_record_count(1, DescKind::AppRow);
        assert!(state.record(0).is_none());
        assert!(state.record(-1).is_none());
        assert!(state.record_mut(0).is_none());
    }

    #[test]
    fn verbose_type_collapses_datetime_family_and_passes_through_others() {
        let mut record = DescRecord::default_for(DescKind::ImpRow);
        record.concise_type = SQL_TYPE_DATE;
        assert_eq!(record.verbose_type(), SQL_DATETIME);
        record.concise_type = SQL_TYPE_TIMESTAMP;
        assert_eq!(record.verbose_type(), SQL_DATETIME);
        record.concise_type = crate::api::odbc_types::SQL_INTEGER;
        assert_eq!(record.verbose_type(), crate::api::odbc_types::SQL_INTEGER);
    }
}
