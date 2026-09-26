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
//! Scope note: IRD/IPD records are populated from the driver's own live
//! metadata sources, not read independently of them:
//! `api::ird::populate_ird` rewrites the IRD from `column_metadata` after
//! every execute and `SQLMoreResults` advance (AB#47437), using the exact
//! field-mapping functions `SQLDescribeColW`/`SQLColAttributeW` already use,
//! so the three cannot disagree for the same column; `describe_param.rs`'s
//! `refine_ipd` does the equivalent for IPD from `SQLDescribeParam`'s
//! server-described `parameter_metadata`, overriding whatever
//! `SQLBindParameter` guessed at bind time. One deliberate, narrow gap
//! remains: the IRD is not reset to empty when a cursor closes
//! (`SQLCloseCursor`/`SQLFreeStmt(SQL_CLOSE)`/`SQLMoreResults`'s
//! batch-end/error arms) — see `api::ird::populate_ird`'s doc comment for
//! why this is safe in practice. Catalog-style descriptive fields
//! (`SQL_DESC_LABEL`, `TABLE_NAME`/`CATALOG_NAME`/`SCHEMA_NAME`,
//! `LITERAL_PREFIX`/`SUFFIX`, `LOCAL_TYPE_NAME`, `TYPE_NAME`, `SEARCHABLE`,
//! `UPDATABLE`, `CASE_SENSITIVE`, `AUTO_UNIQUE_VALUE`, `FIXED_PREC_SCALE`,
//! `UNSIGNED`, `NUM_PREC_RADIX`, `DISPLAY_SIZE`) remain out of scope here:
//! they stay answerable via the already-implemented `SQLColAttributeW`
//! (`api::col_attribute`) alone, deliberately not duplicated into descriptor
//! storage, since nothing above needs them to close the ARD/APD/IRD/IPD
//! binding-and-metadata gap this module used to describe.
//!
//! Same scope note applies to four `DescHeader` fields the ODBC spec defines
//! as *aliases* of statement attributes rather than as independent storage:
//! `SQL_DESC_ARRAY_SIZE` (`SQL_ATTR_ROW_ARRAY_SIZE` / `PARAMSET_SIZE`),
//! `SQL_DESC_BIND_TYPE` (`SQL_ATTR_ROW_BIND_TYPE`), `SQL_DESC_ARRAY_STATUS_PTR`
//! (`SQL_ATTR_ROW_STATUS_PTR`), and `SQL_DESC_ROWS_PROCESSED_PTR`
//! (`SQL_ATTR_ROWS_FETCHED_PTR`). `DescHeader` stores these independently of
//! `StmtState`'s equivalent fields (`set_stmt_attr.rs`), so a
//! `SQLSetStmtAttrW`/`SQLGetDescFieldW` pair (or the reverse) on the same
//! logical value currently sees two unaliased copies. This is the one
//! header-field gap AB#47437 did not close: it scoped record-level binding
//! and metadata, not header-level attribute aliasing.
//!
//! The equivalent *record*-side gap — `SQLBindCol` storing bindings
//! somewhere other than the ARD, invisible to a column bound purely through
//! `SQLSetDescFieldW` — is resolved: the ARD's own records (`ColumnBinding`'s
//! `write_to_record`/`from_record`, `bind_col.rs`) are now the single
//! storage `SQLBindCol` and `SQLFetchScroll` share, and the equivalent
//! APD/IPD pairing for `SQLBindParameter`/execute (`BoundParam`'s
//! `write_to_records`/`from_records`, `bind_param.rs`/`exec_common.rs`) closes
//! the same gap for parameters. `SQLGetDescRecW`/`SQLSetDescRec`
//! (`api::get_desc_rec.rs`/`api::set_desc_rec.rs`) round out the descriptor
//! API surface over this same storage, reusing `set_desc_field.rs`'s own
//! field setters so the bulk and single-field APIs cannot diverge.
//! `SQLSetDescRec` has no `W`/`A` split (its arguments carry no character
//! data), unlike `SQLGetDescRecW`'s `Name` output.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::Mutex;

use super::{DbcHandle, HandleType, HasObjectType, StmtHandle, handle_from_raw};
use crate::api::odbc_types::{
    SQL_C_DEFAULT, SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME, SQL_CA_SS_UDT_CATALOG_NAME,
    SQL_CA_SS_UDT_SCHEMA_NAME, SQL_CA_SS_UDT_TYPE_NAME, SQL_DESC_ALLOC_AUTO, SQL_DESC_ALLOC_TYPE,
    SQL_DESC_ALLOC_USER, SQL_DESC_ARRAY_SIZE, SQL_DESC_ARRAY_STATUS_PTR, SQL_DESC_BIND_OFFSET_PTR,
    SQL_DESC_BIND_TYPE, SQL_DESC_CONCISE_TYPE, SQL_DESC_COUNT, SQL_DESC_DATA_PTR,
    SQL_DESC_DATETIME_INTERVAL_CODE, SQL_DESC_INDICATOR_PTR, SQL_DESC_LENGTH, SQL_DESC_NAME,
    SQL_DESC_NULLABLE, SQL_DESC_OCTET_LENGTH, SQL_DESC_OCTET_LENGTH_PTR, SQL_DESC_PARAMETER_TYPE,
    SQL_DESC_PRECISION, SQL_DESC_ROWS_PROCESSED_PTR, SQL_DESC_SCALE, SQL_DESC_TYPE,
    SQL_DESC_UNNAMED, SQL_ERROR, SQL_NULLABLE, SQL_PARAM_INPUT, SQL_ROWSET_SIZE_DEFAULT,
    SQL_SUCCESS, SqlInteger, SqlLen, SqlPointer, SqlReturn, SqlSmallInt, SqlULen, SqlUSmallInt,
};
use crate::api::sqlstate::SQLSTATE_HY000;
use crate::error::{DiagRecord, HasDiagnostics, free_errors, post_sql_error};
use tracing::error;

/// The five descriptor shapes this driver constructs: the four
/// automatically-allocated implicit descriptors owned by every statement
/// (application/implementation row/parameter), plus the generic explicit
/// application descriptor allocated by `SQLAllocHandle(SQL_HANDLE_DESC, ...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DescKind {
    AppRow,
    AppParam,
    ImpRow,
    ImpParam,
    /// An explicitly-allocated application descriptor. Unlike `AppRow`/
    /// `AppParam`, it has no fixed row-or-parameter role: a single explicit
    /// descriptor can be associated as one statement's ARD and another's APD
    /// at the same time (`SQLSetStmtAttrW`), so it cannot be tagged with
    /// either role at allocation time. Behaves identically to `AppRow`/
    /// `AppParam` for field classification — msodbcsql tags all three the
    /// same generic `SQL_HANDLE_AD` (`sqlcdesc.cpp:5891`), since ARD and APD
    /// were never a behaviorally distinct pair to begin with.
    Ad,
}

impl DescKind {
    /// `true` for descriptors an application binds directly (the two
    /// implicit application descriptors, ARD/APD, plus any explicit
    /// descriptor); `false` for the driver-owned implementation descriptors
    /// (IRD/IPD). msodbcsql calls this shape `AD` since all three share one
    /// record layout (`sqlsrv.h:1546-1557`).
    pub(crate) fn is_application(self) -> bool {
        matches!(self, DescKind::AppRow | DescKind::AppParam | DescKind::Ad)
    }
}

/// Descriptor handle.
#[derive(Debug)]
pub(crate) struct DescHandle {
    pub(crate) object_type: HandleType,
    pub(crate) kind: DescKind,
    /// `SQL_DESC_ALLOC_TYPE`: `SQL_DESC_ALLOC_AUTO` for the four implicit
    /// descriptors, `SQL_DESC_ALLOC_USER` for one allocated by
    /// `SQLAllocHandle(SQL_HANDLE_DESC, ...)`. Mirrored into
    /// `DescState::header::alloc_type` for the `SQLGetDescFieldW`-visible
    /// copy; kept here too, outside `inner`, so lifecycle code (association,
    /// free) can tell implicit and explicit descriptors apart without a lock.
    /// Sound as a plain field because the value is fixed at construction and
    /// `SQL_DESC_ALLOC_TYPE` is permanently read-only (`classify_field`), so
    /// the two copies can never diverge.
    pub(crate) alloc_type: SqlSmallInt,
    /// Back-pointer to the owning DBC. Every descriptor has one — including
    /// the four implicit descriptors, owned by their statement's parent DBC.
    /// It is what
    /// `SQLSetStmtAttrW(SQL_ATTR_APP_ROW_DESC/APP_PARAM_DESC)` compares
    /// against the target statement's own `parent_dbc` (HY024 on mismatch),
    /// and what `SQLFreeHandle(SQL_HANDLE_DESC)` uses to find every statement
    /// that might currently have this descriptor as its active ARD/APD.
    /// IPD definition changes also use it to find the owning statement. Set
    /// once at construction, never mutated — same soundness rationale as
    /// `StmtHandle::parent_dbc`.
    pub(crate) parent_dbc: *mut c_void,
    pub(crate) inner: Mutex<DescState>,
}

/// Header fields common to every descriptor (`RecNumber == 0` in
/// `SQLGetDescFieldW`/`SQLSetDescFieldW`). Validity per kind is gated by
/// [`classify_field`], not by field presence here — mirrors msodbcsql's
/// shared `GENDESCTAG` (`sqlsrv.h:1167-1178`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DescHeader {
    /// `SQL_DESC_ALLOC_TYPE`, mirrored from [`DescHandle::alloc_type`] at
    /// construction (see that field's doc comment for why the value lives in
    /// both places). Always read-only through `SQLSetDescFieldW`
    /// (`classify_field`).
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
    /// Whether an application value binding has been established for this
    /// record. Unlike `data_ptr`, this remains true for a null DAE token.
    pub(crate) data_bound: bool,
    /// APD only: whether the application itself wrote `SQL_DESC_PRECISION`
    /// and/or `SQL_DESC_SCALE` via `SQLSetDescField`/`SQLSetDescRec`, as
    /// opposed to this driver's own default-fill (`SQLBindParameter`'s
    /// `SQL_C_NUMERIC` reset, or a `SQL_DESC_TYPE` write's matching reset —
    /// see `set_type`). `decimal_from_numeric`'s fast path
    /// (`sqlcfunc.cpp:3163-3172`) only forwards a `SQL_C_NUMERIC` struct's own
    /// embedded precision/scale when the APD's precision/scale numerically
    /// match the IPD's *and* the application chose them explicitly; an APD
    /// that merely landed on the same values through this driver's own
    /// `SetTypeDefaults`-equivalent default-fill must not trigger it, or a
    /// bare `SQLBindParameter` into a same-shaped column would wrongly skip
    /// the rescale every other source takes. It also gates which *source*
    /// scale the non-fast-path rescale trusts (`decimal_from_numeric`'s
    /// slow path): explicit means the APD's own `scale` is authoritative,
    /// non-explicit falls back to the struct's own embedded scale, since
    /// that is the only self-description available for a value the app
    /// never described through the descriptor.
    ///
    /// Set to `true` only by `set_precision`/`set_scale` themselves. Reset
    /// to `false` by a `SQL_DESC_TYPE` write that changes the concise type
    /// (`set_type`) and by `SQLBindParameter`'s own APD write
    /// (`write_to_records`) *only when its `ValueType` is changing* —
    /// mirroring the ODBC spec's `SQLBindParameter` rebind rule that
    /// rebinding the same `ValueType` retains other APD fields set by a
    /// prior bind or `SQLSetDescField` call. A same-`SQL_C_NUMERIC` rebind
    /// therefore keeps this flag from a prior explicit call even though the
    /// precision/scale *values* still reset to `(SQL_PREC_NUMERIC, 0)`.
    pub(crate) precision_scale_explicit: bool,
    /// IPD only: set when the application has itself written this record's
    /// type/size (`SQL_DESC_CONCISE_TYPE`/`TYPE`, `DATETIME_INTERVAL_CODE`,
    /// `LENGTH`, `OCTET_LENGTH`, `PRECISION` or `SCALE`) via
    /// `SQLBindParameter` or `SQLSetDescField`/`SQLSetDescRec` — never by
    /// `describe_param.rs`'s own `refine_ipd`, which writes these same
    /// fields directly and bypasses this flag entirely. Distinguishes "the
    /// application chose this" from "a previous `SQLDescribeParam` filled
    /// this in" so `refine_ipd` can refresh what it previously auto-filled
    /// (e.g. across a re-`SQLPrepare`) while never overriding an explicit
    /// bind — `concise_type != 0` alone can't tell the two apart, since
    /// `refine_ipd`'s own write leaves it non-zero too.
    pub(crate) explicitly_bound: bool,
    /// `SQL_CA_SS_UDT_CATALOG_NAME` / `SQL_CA_SS_UDT_SCHEMA_NAME` /
    /// `SQL_CA_SS_UDT_TYPE_NAME`. IPD only, and only for a `SQL_SS_UDT`
    /// parameter, so it is boxed rather than costing three strings on every
    /// record of every descriptor.
    ///
    /// `Arc`, not `Box`: `parameter_definition` snapshots this for the
    /// before/after comparison that decides whether a prepared handle is
    /// stale, and `refine_ipd` takes that snapshot twice per record on every
    /// cache-served describe. Deep-copying the three names there cost O(N^2)
    /// string allocations across a describe-all pass; sharing makes each
    /// snapshot a refcount bump. Writers use `Arc::make_mut`, so a record
    /// whose identity is also held by a live snapshot is copied once, on
    /// write, rather than on every read.
    pub(crate) udt_names: Option<Arc<UdtNames>>,
    /// IPD only: which of the three *wire* name parts the application set
    /// through `SQLSetDescField`, as opposed to `refine_ipd` auto-filling them
    /// from the server's `suggested_user_type_*` columns. Plays the same role
    /// for the UDT identity that `explicitly_bound` plays for the type/size.
    ///
    /// Per field, not one flag for the record: the three parts are
    /// independently writable, so a single bit cannot express "the
    /// application chose the schema, the server still owns the type name".
    /// Collapsing them let a re-`SQLPrepare` preserve a stale server-filled
    /// type name because the application had written only the schema, and let
    /// a describe overwrite a catalog the application had set.
    ///
    /// `assembly_type_name` has no entry: it never reaches the wire, no
    /// describe supplies it, and writing it must not claim the identity.
    pub(crate) udt_name_claimed: UdtNameClaims,
}

/// Which wire parts of a UDT identity the application claimed.
///
/// One bool per part rather than one for the record: `SQLSetDescField` writes
/// them independently, so provenance is per field. An unclaimed part stays the
/// server's to supply and to refresh; a claimed one survives both a describe
/// and the prepare-time clear.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UdtNameClaims {
    pub(crate) catalog: bool,
    pub(crate) schema: bool,
    pub(crate) type_name: bool,
}

impl UdtNameClaims {
    /// True when the application has claimed no wire part, so the whole
    /// identity is still the server's to supply.
    pub(crate) fn none(&self) -> bool {
        !self.catalog && !self.schema && !self.type_name
    }
}

/// The server-side identity an application supplies for a UDT parameter.
///
/// Only the type name is required; SQL Server resolves an unqualified name
/// against the current database and default schema. `assembly_type_name` is
/// stored and echoed back but never sent: the parameter `TYPE_INFO` has no
/// field for it (`CRPCPolicy::WriteUDTHeader` writes three name parts), unlike
/// the `UDT_INFO` in `COLMETADATA`. msodbcsql keeps it the same way
/// (`sqlcdesc.cpp:4954` stores it; no RPC writer reads it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UdtNames {
    pub(crate) catalog: String,
    pub(crate) schema: String,
    pub(crate) type_name: String,
    pub(crate) assembly_type_name: String,
}

/// SQL-side inputs to the prepared declaration, compared only during IPD writes.
#[derive(Debug, Clone)]
pub(crate) struct ParameterDefinition {
    direction: SqlSmallInt,
    sql_type: SqlSmallInt,
    length: SqlULen,
    precision: SqlSmallInt,
    scale: SqlSmallInt,
    /// Catalog/schema/type of a UDT, which `sp_executesql` spells out in the
    /// declaration and so must invalidate a materialized handle when it
    /// changes. The assembly-qualified name is excluded: it never reaches the
    /// wire, so rewriting it cannot change the prepared text.
    ///
    /// Shares the record's `Arc` rather than copying the names: this snapshot
    /// is taken twice per record on every cache-served describe, so cloning
    /// made the comparison itself the allocation cost it was meant to avoid.
    /// `PartialEq` still compares the names by value, so a record whose
    /// identity was replaced compares unequal even if the new `Arc` happens
    /// to hold the same strings.
    udt: Option<Arc<UdtNames>>,
}

impl PartialEq for ParameterDefinition {
    /// Compares the three *wire* name parts, never `assembly_type_name`: it
    /// does not reach the declaration, so rewriting it must not orphan a
    /// prepared handle. A derived impl would compare it, since it lives on the
    /// shared `UdtNames`.
    ///
    /// Pointer equality is checked first only as a fast path for the common
    /// case where both snapshots share one `Arc`; the value comparison behind
    /// it is what decides.
    fn eq(&self, other: &Self) -> bool {
        if self.direction != other.direction
            || self.sql_type != other.sql_type
            || self.length != other.length
            || self.precision != other.precision
            || self.scale != other.scale
        {
            return false;
        }
        match (&self.udt, &other.udt) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                Arc::ptr_eq(a, b)
                    || (a.catalog == b.catalog
                        && a.schema == b.schema
                        && a.type_name == b.type_name)
            }
            _ => false,
        }
    }
}

impl Eq for ParameterDefinition {}

impl DescRecord {
    pub(crate) fn parameter_definition(&self) -> ParameterDefinition {
        use crate::api::odbc_types::{
            SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_CHAR, SQL_DECIMAL, SQL_DOUBLE, SQL_FLOAT,
            SQL_GUID, SQL_INTEGER, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NUMERIC, SQL_REAL,
            SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_TINYINT, SQL_TYPE_DATE,
            SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR, SQL_WCHAR,
            SQL_WLONGVARCHAR, SQL_WVARCHAR,
        };
        let (length, precision, scale) = match self.concise_type {
            SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR | SQL_WCHAR | SQL_WVARCHAR
            | SQL_WLONGVARCHAR | SQL_BINARY | SQL_VARBINARY | SQL_LONGVARBINARY => {
                (self.length, 0, 0)
            }
            SQL_NUMERIC | SQL_DECIMAL => (0, self.precision, self.scale),
            SQL_BIGINT | SQL_BIT | SQL_TINYINT | SQL_SMALLINT | SQL_INTEGER | SQL_REAL
            | SQL_FLOAT | SQL_DOUBLE | SQL_GUID | SQL_TYPE_DATE => (0, 0, 0),
            // datetime_metadata always declares scale 7; the application scale
            // controls conversion validation, not time/datetime2/offset SQL.
            SQL_TYPE_TIME | SQL_TYPE_TIMESTAMP | SQL_SS_TIME2 | SQL_SS_TIMESTAMPOFFSET => (0, 0, 0),
            // Special types can encode dimensions or other SQL shape here.
            _ => (self.length, self.precision, self.scale),
        };
        ParameterDefinition {
            direction: self.parameter_type,
            sql_type: self.concise_type,
            length,
            precision,
            scale,
            // Only the three wire parts, and only on a record that actually
            // declares a UDT: the concise type says so, and a type name is
            // present. The `SQL_CA_SS_UDT_*` fields are writable on any IPD
            // record (`classify_field` gates on kind, not type), so without
            // the first test a UDT name on an `int` marker would orphan a
            // materialized prepared handle for text the declaration never
            // contains. Without the second, a record holding only a catalog,
            // schema or the echo-only assembly name would do the same - none
            // of those declare a type, and `udt_type_name` refuses to execute
            // such a record at all (`ERR_MISSING_UDT_TYPE_NAME`).
            //
            // "Has a wire identity" is a non-empty type name, the same test
            // `udt_type_name` applies before an execute: a record holding only
            // a catalog or schema declares nothing. Once a type name is
            // present the catalog and schema travel with it, so changing
            // either still invalidates, and a later switch away from
            // `SQL_SS_UDT` invalidates through `sql_type`.
            udt: self
                .udt_names
                .as_ref()
                .filter(|names| {
                    self.concise_type == crate::api::odbc_types::SQL_SS_UDT
                        && !names.type_name.is_empty()
                })
                .map(Arc::clone),
        }
    }

    /// A freshly grown record's defaults, keyed by descriptor kind. Mirrors
    /// msodbcsql's `FastSetADRecDefaults` (`fCType = SQL_C_DEFAULT`,
    /// `sqlcdesc.cpp:136-148`) and `FastSetIPDRecDefaults`
    /// (`fParamType = SQL_PARAM_INPUT`, `fParamNullable = SQL_NULLABLE`,
    /// `sqlcdesc.cpp:155-168`). IRD has no analogous default-fill helper in
    /// msodbcsql since it is always populated from result metadata rather
    /// than grown by an application `SQL_DESC_COUNT` write; a freshly grown
    /// IRD record here is simply zeroed, matching an unpopulated column.
    pub(crate) fn default_for(kind: DescKind) -> Self {
        let (concise_type, parameter_type, nullable) = match kind {
            DescKind::AppRow | DescKind::AppParam | DescKind::Ad => (SQL_C_DEFAULT, 0, 0),
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
            data_bound: false,
            precision_scale_explicit: false,
            explicitly_bound: false,
            udt_names: None,
            udt_name_claimed: UdtNameClaims::default(),
        }
    }

    /// `SQL_DESC_TYPE`, the verbose form of `concise_type`: `SQL_TYPE_TIME`/
    /// `SQL_TYPE_TIMESTAMP` collapse to `SQL_DATETIME`, with the member
    /// identified by `datetime_interval_code`; every other type — including
    /// `SQL_TYPE_DATE` — reports its concise value unchanged.
    ///
    /// `SQL_TYPE_DATE` is deliberately excluded from the fold, not an
    /// oversight: verified against msodbcsql's own `GetDescField`
    /// (`sqlcdesc.cpp:2226-2236`), which special-cases its stored `SQL_DATE`
    /// tag to answer `SQL_DESC_TYPE = SQL_TYPE_DATE` — *not* the verbose
    /// `SQL_DATETIME` its `SQL_TIME`/`SQL_TIMESTAMP` siblings fold to — and
    /// matches this crate's own `SQLColAttributeW` (`col_attribute.rs`'s
    /// `verbose_type`, `is_odbc_timestamp` excludes `TdsDataType::DateN` for
    /// the identical reason), which an IRD populated from live column
    /// metadata must agree with (AB#47437's consistency requirement) —
    /// folding `SQL_TYPE_DATE` here would silently disagree with that
    /// already-verified answer for every `date` column.
    ///
    /// Otherwise deliberately simpler than msodbcsql's equivalent:
    /// msodbcsql stores descriptor types in a 2.x-era internal
    /// representation and remaps on both read and write. This driver
    /// targets ODBC 3.x only
    /// (`.github/instructions/mssql-odbc.instructions.md`) and stores the
    /// 3.x concise value directly, so verbose synthesis is a direct type
    /// check rather than a remap. The ODBC `SQL_INTERVAL_*` family is
    /// likewise not folded to a verbose `SQL_INTERVAL`: SQL Server has no
    /// interval SQL type, so no concise interval value can ever reach a
    /// descriptor record through this driver's execution path.
    pub(crate) fn verbose_type(&self) -> SqlSmallInt {
        use crate::api::odbc_types::{SQL_DATETIME, SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP};
        match self.concise_type {
            SQL_TYPE_TIME | SQL_TYPE_TIMESTAMP => SQL_DATETIME,
            _ => self.concise_type,
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
    /// Drops the UDT identities `SQLDescribeParam` auto-filled, keeping any the
    /// application set through `SQLSetDescField`. Call when new SQL supersedes
    /// the text a name was described from - the identity belongs to that text,
    /// not to the binding, and a record the application later binds would
    /// otherwise carry it onto an unrelated statement.
    ///
    /// Only the three wire-relevant parts are dropped; an assembly-qualified
    /// name the application set is echo-only state that no describe *in this
    /// driver* supplies (`read_udt_names` deliberately skips the column), so
    /// it outlives the identity it was written beside.
    ///
    /// Never call with a STMT lock held (see the crate's locking rules).
    /// Returns `SQL_ERROR` on a poisoned mutex rather than reporting success
    /// with a stale identity still in place.
    pub(crate) fn clear_auto_filled_udt_names(&self) -> SqlReturn {
        let Ok(mut state) = self.inner.lock() else {
            error!("clearing auto-filled UDT names: desc mutex poisoned");
            return SQL_ERROR;
        };
        for record in &mut state.records {
            if record.udt_name_claimed == UdtNameClaims::default() && record.udt_names.is_none() {
                continue;
            }
            let Some(names) = record.udt_names.as_mut().map(Arc::make_mut) else {
                continue;
            };
            // Per part: an application's write survives, an auto-filled one
            // goes. Clearing a part rather than the record keeps the rest -
            // including the echo-only assembly name, which no describe can
            // restore - and leaves the cleared parts refreshable, since
            // `refine_ipd` treats an unclaimed part as the server's to supply.
            if !record.udt_name_claimed.catalog {
                names.catalog.clear();
            }
            if !record.udt_name_claimed.schema {
                names.schema.clear();
            }
            if !record.udt_name_claimed.type_name {
                names.type_name.clear();
            }
            // Nothing left worth keeping: no claimed part, no assembly name.
            if record.udt_name_claimed.none() && names.assembly_type_name.is_empty() {
                record.udt_names = None;
            }
        }
        SQL_SUCCESS
    }

    /// Captures partial failed writes too. Never acquires DBC/STMT while DESC
    /// is locked, and APD/ARD writes never inspect statement ownership.
    pub(crate) fn update_definition(
        &self,
        record_number: SqlSmallInt,
        op: &str,
        update: impl FnOnce(&mut DescState) -> SqlReturn,
    ) -> SqlReturn {
        let Ok(mut state) = self.inner.lock() else {
            error!("{op}: desc mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut state);
        let is_ipd = self.kind == DescKind::ImpParam;
        let previous_count = state.records.len();
        let previous = is_ipd
            .then(|| {
                state
                    .record(record_number)
                    .map(DescRecord::parameter_definition)
            })
            .flatten();
        let rc = update(&mut state);
        let first_changed = if !is_ipd {
            None
        } else if previous_count != state.records.len() {
            Some(previous_count.min(state.records.len()) + 1)
        } else if previous
            != state
                .record(record_number)
                .map(DescRecord::parameter_definition)
        {
            usize::try_from(record_number).ok()
        } else {
            None
        };
        drop(state);
        if let Some(first_changed) = first_changed
            && self.invalidate_prepared_owner(first_changed).is_err()
        {
            error!("{op}: failed invalidating prepared parameter definition");
            if let Ok(mut state) = self.inner.lock() {
                post_sql_error(
                    &mut state,
                    SQLSTATE_HY000,
                    0,
                    "Internal error invalidating prepared parameter definition",
                );
            }
            return SQL_ERROR;
        }
        rc
    }

    fn invalidate_prepared_owner(&self, first_changed: usize) -> Result<(), ()> {
        // The DM keeps the parent alive; its child list is protected for the
        // walk, matching SQLFreeHandle(DESC)'s existing DBC -> STMT traversal.
        let dbc = unsafe { handle_from_raw::<DbcHandle>(self.parent_dbc) };
        let Ok(state) = dbc.inner.lock() else {
            error!("invalidating IPD definition: dbc mutex poisoned");
            return Err(());
        };
        for &raw in &state.statements {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(raw) };
            if std::ptr::eq(stmt.ipd.cast::<DescHandle>(), self) {
                return stmt.invalidate_parameter_definition(first_changed);
            }
        }
        error!("invalidating IPD definition: owning statement not found");
        Err(())
    }

    pub(crate) fn new(kind: DescKind, alloc_type: SqlSmallInt, parent_dbc: *mut c_void) -> Self {
        Self {
            object_type: HandleType::Desc,
            kind,
            alloc_type,
            parent_dbc,
            inner: Mutex::new(DescState {
                diag_records: Vec::new(),
                header: DescHeader {
                    alloc_type,
                    ..DescHeader::default()
                },
                records: Vec::new(),
            }),
        }
    }

    /// `true` for an explicitly-allocated descriptor
    /// (`SQLAllocHandle(SQL_HANDLE_DESC, ...)`), `false` for one of the four
    /// implicit descriptors a statement owns from creation.
    pub(crate) fn is_explicit(&self) -> bool {
        self.alloc_type == SQL_DESC_ALLOC_USER
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
// `indicator_ptr`, `octet_length_ptr`), and `DescHandle` itself holds
// `parent_dbc`, which together prevent auto-derivation of
// `Send`/`Sync` for `DescState` and therefore `DescHandle` (same pattern as
// `StmtHandle`/`BoundParam`, which store the analogous application buffer
// addresses). Every one of the `DescHeader`/`DescRecord` pointers is an
// opaque application-owned address: copied in by
// `SQLSetDescFieldW`/`SQLSetStmtAttrW`, copied out by
// `SQLGetDescFieldW`/`SQLGetStmtAttrW`, and never dereferenced by this
// module. `parent_dbc` is set once at construction and never mutated, and the
// parent DBC is guaranteed alive because the DM ensures every descriptor —
// implicit (freed with its owning statement) or explicit (freed by
// `SQLFreeHandle(SQL_HANDLE_DESC)`) — is freed before its connection. The
// Driver Manager may legitimately call ODBC entry points for the same handle
// from different threads (serialized by `inner`'s mutex), so the handle
// itself must be `Send + Sync`.
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
        // The UDT identity for a `SQL_SS_UDT` parameter. IPD-only and
        // writable. An application writing here is one of two sources; the
        // other is `refine_ipd`, which copies the server's
        // `suggested_user_type_*` columns into any record the application has
        // not claimed. The assembly-qualified name is accepted and echoed but
        // never sent - the parameter header has no field for it, and msodbcsql
        // stores it without writing it either.
        SQL_CA_SS_UDT_CATALOG_NAME
        | SQL_CA_SS_UDT_SCHEMA_NAME
        | SQL_CA_SS_UDT_TYPE_NAME
        | SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME
            if is_ipd =>
        {
            (Record, true)
        }

        _ => return None,
    };
    Some(FieldAccess { scope, writable })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_DATETIME, SQL_TYPE_DATE, SQL_TYPE_TIMESTAMP};

    const ALL_KINDS: [DescKind; 5] = [
        DescKind::AppRow,
        DescKind::AppParam,
        DescKind::ImpRow,
        DescKind::ImpParam,
        DescKind::Ad,
    ];

    #[test]
    fn special_parameter_definitions_keep_size_precision_and_scale() {
        use crate::api::odbc_types::{SQL_SS_UDT, SQL_SS_VECTOR};

        for sql_type in [SQL_SS_VECTOR, SQL_SS_UDT] {
            let mut record = DescRecord::default_for(DescKind::ImpParam);
            record.concise_type = sql_type;
            for field in 0..3 {
                let previous = record.parameter_definition();
                match field {
                    0 => record.length += 4,
                    1 => record.precision += 1,
                    _ => record.scale += 1,
                }
                assert_ne!(record.parameter_definition(), previous);
            }
        }
    }

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

    /// The UDT identity is supplied by the application on the IPD; every other
    /// descriptor kind has no use for it. All four parts are writable, matching
    /// msodbcsql (`sqlcdesc.cpp:4891-4957`), even though only the first three
    /// reach the wire.
    #[test]
    fn udt_name_fields_are_writable_on_ipd_only() {
        for field in [
            SQL_CA_SS_UDT_CATALOG_NAME,
            SQL_CA_SS_UDT_SCHEMA_NAME,
            SQL_CA_SS_UDT_TYPE_NAME,
            SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME,
        ] {
            for kind in [DescKind::AppRow, DescKind::AppParam, DescKind::ImpRow] {
                assert!(classify_field(kind, field).is_none(), "{kind:?} {field}");
            }
            let access = classify_field(DescKind::ImpParam, field).unwrap();
            assert_eq!(access.scope, FieldScope::Record, "{field}");
            assert!(access.writable, "{field}");
        }
    }

    #[test]
    fn unknown_field_id_is_none_for_every_kind() {
        for kind in ALL_KINDS {
            assert!(classify_field(kind, 0xFFFF).is_none());
        }
    }

    /// New SQL supersedes the text a describe filled these names from, so the
    /// auto-filled ones go and the application's stay. Without this, a later
    /// `SQLBindParameter` marks the record explicitly bound and freezes a stale
    /// identity that `refine_ipd` then refuses to touch.
    #[test]
    fn clearing_auto_filled_udt_names_spares_application_supplied_ones() {
        // The statement's real IPD, per the fixture rule: in production this
        // method is only ever called on `stmt.ipd`.
        let h = crate::test_support::TestHandles::with_env_dbc_stmt();
        let handle = unsafe { crate::handles::handle_from_raw::<DescHandle>(h.ipd()) };
        {
            let mut state = handle.inner.lock().unwrap();
            state.set_record_count(2, DescKind::ImpParam);
            let described = state.record_mut(1).unwrap();
            described.udt_names = Some(Arc::new(UdtNames {
                type_name: "hierarchyid".to_string(),
                ..Default::default()
            }));
            described.udt_name_claimed = UdtNameClaims::default();
            let chosen = state.record_mut(2).unwrap();
            chosen.udt_names = Some(Arc::new(UdtNames {
                type_name: "Point".to_string(),
                ..Default::default()
            }));
            chosen.udt_name_claimed = UdtNameClaims {
                catalog: true,
                schema: true,
                type_name: true,
            };
        }

        handle.clear_auto_filled_udt_names();

        let state = handle.inner.lock().unwrap();
        assert!(state.records[0].udt_names.is_none());
        assert_eq!(state.records[0].udt_name_claimed, UdtNameClaims::default());
        assert_eq!(
            state.records[1].udt_names.as_ref().unwrap().type_name,
            "Point"
        );
    }

    /// The assembly-qualified name is echo-only and no describe in this driver
    /// supplies it (`read_udt_names` skips the column), so writing it must not claim the wire identity - doing so froze a stale
    /// auto-filled name against both the prepare-time clear and a later
    /// describe. The name itself still survives that clear.
    #[test]
    fn the_assembly_name_neither_claims_nor_loses_the_wire_identity() {
        let h = crate::test_support::TestHandles::with_env_dbc_stmt();
        let handle = unsafe { crate::handles::handle_from_raw::<DescHandle>(h.ipd()) };
        {
            let mut state = handle.inner.lock().unwrap();
            state.set_record_count(1, DescKind::ImpParam);
            let record = state.record_mut(1).unwrap();
            record.udt_names = Some(Arc::new(UdtNames {
                type_name: "hierarchyid".to_string(),
                assembly_type_name: "Asm.Point".to_string(),
                ..Default::default()
            }));
            record.udt_name_claimed = UdtNameClaims::default();
        }

        assert_eq!(handle.clear_auto_filled_udt_names(), SQL_SUCCESS);

        let state = handle.inner.lock().unwrap();
        let names = state.records[0].udt_names.as_ref().unwrap();
        assert_eq!(
            names.type_name, "",
            "the auto-filled wire identity must not survive"
        );
        assert_eq!(
            names.assembly_type_name, "Asm.Point",
            "the application's assembly name is not the server's to drop"
        );
    }

    /// The UDT's catalog/schema/type are spelled out in the `sp_executesql`
    /// declaration, so changing one has to orphan a materialized prepared
    /// handle exactly as changing the SQL type would. `update_definition`
    /// decides that by comparing `parameter_definition()`, so the names must
    /// be part of the projection. The assembly-qualified name is excluded: it
    /// never reaches the wire, so rewriting it must not force a re-prepare.
    #[test]
    fn the_udt_name_is_part_of_the_prepared_parameter_definition() {
        let mut record = DescRecord::default_for(DescKind::ImpParam);
        record.concise_type = crate::api::odbc_types::SQL_SS_UDT;
        let without_names = record.parameter_definition();

        record.udt_names = Some(Arc::new(UdtNames {
            catalog: String::new(),
            schema: "dbo".to_string(),
            type_name: "Point".to_string(),
            assembly_type_name: String::new(),
        }));
        let point = record.parameter_definition();
        assert_ne!(without_names, point);

        record
            .udt_names
            .as_mut()
            .map(Arc::make_mut)
            .unwrap()
            .type_name = "Shape".to_string();
        assert_ne!(point, record.parameter_definition());

        record
            .udt_names
            .as_mut()
            .map(Arc::make_mut)
            .unwrap()
            .type_name = "Point".to_string();
        record
            .udt_names
            .as_mut()
            .map(Arc::make_mut)
            .unwrap()
            .assembly_type_name = "Asm.Point".to_string();
        assert_eq!(
            point,
            record.parameter_definition(),
            "the assembly name never reaches the wire, so it cannot invalidate"
        );

        // The same rule from the other direction: a record that carries *only*
        // an assembly name has no wire identity at all, so its projection must
        // stay indistinguishable from a record with no identity. Projecting
        // `Some(("", "", ""))` here orphaned a materialized prepared handle on
        // an `SQL_CA_SS_UDT_ASSEMBLY_TYPE_NAME` write, forcing a re-prepare for
        // a field that never reaches the declaration.
        let mut assembly_only = DescRecord::default_for(DescKind::ImpParam);
        assembly_only.concise_type = crate::api::odbc_types::SQL_SS_UDT;
        assembly_only.udt_names = Some(Arc::new(UdtNames {
            assembly_type_name: "Asm.Point".to_string(),
            ..Default::default()
        }));
        assert_eq!(
            without_names,
            assembly_only.parameter_definition(),
            "an assembly-only record declares nothing, so it must not re-prepare"
        );

        // The same for a record holding only a catalog or only a schema: no
        // type name means no declaration, and `udt_type_name` refuses to
        // execute it. This is the test `refine_ipd` applies as `claimed`, so
        // the two definitions of "has a wire identity" stay identical.
        for partial in [
            UdtNames {
                catalog: "mydb".to_string(),
                ..Default::default()
            },
            UdtNames {
                schema: "dbo".to_string(),
                ..Default::default()
            },
        ] {
            let mut record = DescRecord::default_for(DescKind::ImpParam);
            record.concise_type = crate::api::odbc_types::SQL_SS_UDT;
            record.udt_names = Some(Arc::new(partial));
            assert_eq!(
                without_names,
                record.parameter_definition(),
                "a qualification without a type name declares nothing"
            );
        }

        // A UDT name on a record that does not declare a UDT. The
        // `SQL_CA_SS_UDT_*` fields are writable on any IPD record, but an
        // `int` declaration never mentions them, so the projection must ignore
        // them until the type actually becomes `SQL_SS_UDT` - at which point
        // `sql_type` invalidates and carries the name along.
        let mut scalar = DescRecord::default_for(DescKind::ImpParam);
        scalar.concise_type = crate::api::odbc_types::SQL_INTEGER;
        let scalar_plain = scalar.parameter_definition();
        scalar.udt_names = Some(Arc::new(UdtNames {
            type_name: "Point".to_string(),
            ..Default::default()
        }));
        assert_eq!(
            scalar_plain,
            scalar.parameter_definition(),
            "a UDT name on a non-UDT record changes no declaration"
        );

        // ... and switching that record to a UDT does invalidate, name included.
        scalar.concise_type = crate::api::odbc_types::SQL_SS_UDT;
        assert_ne!(scalar_plain, scalar.parameter_definition());
    }

    #[test]
    fn new_descriptor_starts_with_no_records_and_default_header() {
        let handle = DescHandle::new(
            DescKind::AppParam,
            SQL_DESC_ALLOC_AUTO,
            std::ptr::null_mut(),
        );
        let state = handle.inner.lock().unwrap();
        assert!(state.records.is_empty());
        assert_eq!(state.header.alloc_type, SQL_DESC_ALLOC_AUTO);
        assert_eq!(state.header.array_size, SQL_ROWSET_SIZE_DEFAULT);
    }

    #[test]
    fn new_explicit_descriptor_reports_alloc_user_on_both_copies() {
        let dbc = 0x1234_usize as *mut c_void;
        let handle = DescHandle::new(DescKind::Ad, SQL_DESC_ALLOC_USER, dbc);
        assert!(handle.is_explicit());
        assert_eq!(handle.parent_dbc, dbc);
        assert_eq!(
            handle.inner.lock().unwrap().header.alloc_type,
            SQL_DESC_ALLOC_USER
        );
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

    /// `SQL_TYPE_DATE` is the one member of the "datetime family" that does
    /// *not* fold to the verbose `SQL_DATETIME` — verified against
    /// msodbcsql's `GetDescField` (`sqlcdesc.cpp:2226-2236`), which
    /// special-cases it to answer its own concise value, unlike
    /// `SQL_TYPE_TIME`/`SQL_TYPE_TIMESTAMP` which do fold. Getting this wrong
    /// would disagree with `SQLColAttributeW`'s already-verified answer for
    /// a `date` column once an IRD is populated from live metadata
    /// (AB#47437) — see [`super::verbose_type`]'s doc comment.
    #[test]
    fn verbose_type_passes_through_date_but_collapses_time_and_timestamp() {
        let mut record = DescRecord::default_for(DescKind::ImpRow);
        record.concise_type = SQL_TYPE_DATE;
        assert_eq!(record.verbose_type(), SQL_TYPE_DATE);
        record.concise_type = crate::api::odbc_types::SQL_TYPE_TIME;
        assert_eq!(record.verbose_type(), SQL_DATETIME);
        record.concise_type = SQL_TYPE_TIMESTAMP;
        assert_eq!(record.verbose_type(), SQL_DATETIME);
        record.concise_type = crate::api::odbc_types::SQL_INTEGER;
        assert_eq!(record.verbose_type(), crate::api::odbc_types::SQL_INTEGER);
    }
}
