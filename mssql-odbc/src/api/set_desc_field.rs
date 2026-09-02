// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLSetDescFieldW`.
//!
//! Field validity per descriptor kind is decided once, in
//! [`crate::handles::desc::classify_field`], shared with `SQLGetDescFieldW`.
//! This module owns only the set-direction concerns: the IRD-is-read-only
//! gate, growing the record plex on demand, type/concise-type/interval-code
//! coupling, and `SQL_C_NUMERIC` precision/scale validation.
//!
//! Scalar values arrive pointer-encoded (`Value` holds the number itself,
//! not its address) — the same convention this crate already uses for
//! `SQLSetStmtAttrW` (`set_stmt_attr.rs`) and confirmed against msodbcsql's
//! `SetADHeaderField` (`(SIZE_T)Value`, `sqlcdesc.cpp:4129-4149`).
//!
//! Unlike `SQLBindCol`/`SQLFreeStmt(SQL_UNBIND)`/`SQLSetStmtAttr`, this
//! entry point does not check `STMT_STATE_FETCH_IN_PROGRESS` before writing
//! `SQL_DESC_DATA_PTR`/`SQL_DESC_OCTET_LENGTH`/`SQL_DESC_CONCISE_TYPE` on an
//! ARD/APD record a fetch may still be reading through — a known, deferred
//! gap tracked in [#472](https://github.com/microsoft/mssql-rs/issues/472).

use std::mem::size_of;

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_C_NUMERIC, SQL_CODE_DATE, SQL_CODE_TIME, SQL_CODE_TIMESTAMP, SQL_DATETIME,
    SQL_DESC_ARRAY_SIZE, SQL_DESC_ARRAY_STATUS_PTR, SQL_DESC_BIND_OFFSET_PTR, SQL_DESC_BIND_TYPE,
    SQL_DESC_CONCISE_TYPE, SQL_DESC_COUNT, SQL_DESC_DATA_PTR, SQL_DESC_DATETIME_INTERVAL_CODE,
    SQL_DESC_INDICATOR_PTR, SQL_DESC_LENGTH, SQL_DESC_NAME, SQL_DESC_OCTET_LENGTH,
    SQL_DESC_OCTET_LENGTH_PTR, SQL_DESC_PARAMETER_TYPE, SQL_DESC_PRECISION,
    SQL_DESC_ROWS_PROCESSED_PTR, SQL_DESC_SCALE, SQL_DESC_TYPE, SQL_DESC_UNNAMED, SQL_ERROR,
    SQL_INVALID_HANDLE, SQL_NTS, SQL_PARAM_INPUT, SQL_PARAM_INPUT_OUTPUT, SQL_PARAM_OUTPUT,
    SQL_PREC_NUMERIC, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SQL_TYPE_DATE, SQL_TYPE_TIME,
    SQL_TYPE_TIMESTAMP, SQL_UNNAMED, SqlHandle, SqlInteger, SqlLen, SqlPointer, SqlReturn,
    SqlSmallInt, SqlULen, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    ERR_CANNOT_MODIFY_IRD, ERR_INCONSISTENT_DESCRIPTOR_INFO, ERR_INVALID_ATTRIBUTE_VALUE,
    ERR_INVALID_C_DATA_TYPE, ERR_INVALID_DESCRIPTOR_FIELD, ERR_INVALID_DESCRIPTOR_INDEX,
    ERR_INVALID_NULL_POINTER, ERR_INVALID_PARAMETER_TYPE, ERR_INVALID_PRECISION_OR_SCALE,
    ERR_INVALID_SQL_DATA_TYPE, ERR_INVALID_STRING_OR_BUFFER_LENGTH,
    ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED, WARN_ARRAY_SIZE_CHANGED, post_diag,
};
use crate::api::type_rules::{
    SqlTypeSupport, canonical_c_type, classify_parameter_sql_type, is_valid_c_type,
};
use crate::api::util::read_utf16;
use crate::error::free_errors;
use crate::handles::desc::{DescKind, DescRecord, DescState, FieldScope, classify_field};
use crate::handles::{DescHandle, HandleType, handle_from_raw};

/// Implementation of [`SQLSetDescFieldW`](super::exports::SQLSetDescFieldW).
///
/// # Safety
/// See the exported function's doc for caller requirements.
pub(crate) unsafe fn sql_set_desc_field_w(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
) -> SqlReturn {
    debug!(
        ?descriptor_handle,
        record_number,
        field_identifier,
        ?value_ptr,
        buffer_length,
        "SQLSetDescFieldW called",
    );
    crate::ffi_entry!("SQLSetDescFieldW", unsafe {
        sql_set_desc_field_w_impl(
            descriptor_handle,
            record_number,
            field_identifier,
            value_ptr,
            buffer_length,
        )
    })
}

unsafe fn sql_set_desc_field_w_impl(
    descriptor_handle: SqlHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
) -> SqlReturn {
    if descriptor_handle.is_null() {
        error!("SQLSetDescFieldW: descriptor_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let desc = unsafe { handle_from_raw::<DescHandle>(descriptor_handle) };
    debug_assert_eq!(
        desc.object_type,
        HandleType::Desc,
        "SQLSetDescFieldW: handle is not a DESC"
    );

    sql_set_desc_field_w_safe(
        desc,
        record_number,
        field_identifier,
        value_ptr,
        buffer_length,
    )
}

fn sql_set_desc_field_w_safe(
    desc: &DescHandle,
    record_number: SqlSmallInt,
    field_identifier: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
) -> SqlReturn {
    let Ok(mut state) = desc.inner.lock() else {
        error!("SQLSetDescFieldW: desc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    let Ok(field) = SqlUSmallInt::try_from(field_identifier) else {
        error!(
            field_identifier,
            "SQLSetDescFieldW: negative field identifier"
        );
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    };

    // Blanket IRD gate, checked before general field validity — mirrors
    // msodbcsql's literal order (`sqlcdesc.cpp:1399-1405`): every IRD field
    // write is rejected except these two header pointer fields.
    if desc.kind == DescKind::ImpRow
        && field != SQL_DESC_ROWS_PROCESSED_PTR
        && field != SQL_DESC_ARRAY_STATUS_PTR
    {
        error!("SQLSetDescFieldW: cannot modify an implementation row descriptor");
        post_diag(&mut state, ERR_CANNOT_MODIFY_IRD);
        return SQL_ERROR;
    }

    let Some(access) = classify_field(desc.kind, field) else {
        error!(
            field,
            kind = ?desc.kind,
            "SQLSetDescFieldW: field not valid for this descriptor kind"
        );
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    };

    if !access.writable {
        error!(field, "SQLSetDescFieldW: field is not writable");
        post_diag(&mut state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    }

    match access.scope {
        FieldScope::Header if field == SQL_DESC_COUNT => {
            set_record_count_field(&mut state, desc.kind, value_ptr)
        }
        FieldScope::Header => set_header_field(&mut state, field, value_ptr),
        FieldScope::Record => {
            if record_number < 1 {
                error!(record_number, "SQLSetDescFieldW: invalid record number");
                post_diag(&mut state, ERR_INVALID_DESCRIPTOR_INDEX);
                return SQL_ERROR;
            }
            let Ok(count) = usize::try_from(record_number) else {
                post_diag(&mut state, ERR_INVALID_DESCRIPTOR_INDEX);
                return SQL_ERROR;
            };
            // Growing on demand: msodbcsql calls AllocPlex before the
            // field-specific setter for any record write, which can grow
            // SQL_DESC_COUNT even if that setter later fails
            // (sqlcdesc.cpp:1587-1614). Mirrored here rather than validating
            // first, so behavior matches on the failure path too.
            if count > state.records.len() {
                state.set_record_count(count, desc.kind);
            }
            set_record_field(
                &mut state,
                desc.kind,
                record_number,
                field,
                value_ptr,
                buffer_length,
            )
        }
    }
}

/// `SQL_DESC_COUNT` write: grows or shrinks the record plex.
fn set_record_count_field(
    state: &mut DescState,
    kind: DescKind,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let requested = value_ptr as SqlULen;
    let Ok(max) = usize::try_from(SqlSmallInt::MAX) else {
        return SQL_ERROR; // unreachable: SqlSmallInt::MAX always fits usize
    };
    if requested > max {
        error!(requested, "SQLSetDescFieldW: SQL_DESC_COUNT out of range");
        post_diag(state, ERR_INVALID_ATTRIBUTE_VALUE);
        return SQL_ERROR;
    }
    state.set_record_count(requested, kind);
    SQL_SUCCESS
}

/// Header fields other than `SQL_DESC_COUNT` (handled by the caller since it
/// needs the record list, not just the header).
fn set_header_field(
    state: &mut DescState,
    field: SqlUSmallInt,
    value_ptr: SqlPointer,
) -> SqlReturn {
    match field {
        SQL_DESC_ARRAY_SIZE => {
            let requested = value_ptr as SqlULen;
            // Zero is not a valid rowset size — matches this crate's own
            // SQL_ATTR_ROW_ARRAY_SIZE validation (set_stmt_attr.rs) and the
            // ODBC-mandated minimum of 1 row per rowset. Deliberate
            // divergence from msodbcsql, which stores 0 unvalidated and only
            // clamps the upper bound (`sqlcdesc.cpp:4161-4167`) — do not
            // "fix" this back toward msodbcsql's behavior.
            if requested == 0 {
                error!("SQLSetDescFieldW: SQL_DESC_ARRAY_SIZE of 0 is invalid");
                post_diag(state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            }
            let Ok(max) = SqlULen::try_from(i32::MAX) else {
                return SQL_ERROR; // unreachable
            };
            if requested > max {
                // Clamped with 01S02, matching msodbcsql (sqlcdesc.cpp:4161-4167).
                state.header.array_size = max;
                post_diag(state, WARN_ARRAY_SIZE_CHANGED);
                return SQL_SUCCESS_WITH_INFO;
            }
            state.header.array_size = requested;
            SQL_SUCCESS
        }
        SQL_DESC_ARRAY_STATUS_PTR => {
            state.header.array_status_ptr = value_ptr;
            SQL_SUCCESS
        }
        SQL_DESC_BIND_OFFSET_PTR => {
            state.header.bind_offset_ptr = value_ptr;
            SQL_SUCCESS
        }
        SQL_DESC_BIND_TYPE => {
            let requested = value_ptr as SqlULen;
            let Ok(v) = SqlInteger::try_from(requested) else {
                post_diag(state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            };
            state.header.bind_type = v;
            SQL_SUCCESS
        }
        SQL_DESC_ROWS_PROCESSED_PTR => {
            state.header.rows_processed_ptr = value_ptr;
            SQL_SUCCESS
        }
        _ => {
            debug_assert!(
                false,
                "classify_field allowed an unmapped writable header field"
            );
            post_diag(state, ERR_INVALID_DESCRIPTOR_FIELD);
            SQL_ERROR
        }
    }
}

/// Record fields. The record at `record_number` already exists — the caller
/// grows the plex before dispatching here.
fn set_record_field(
    state: &mut DescState,
    kind: DescKind,
    record_number: SqlSmallInt,
    field: SqlUSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
) -> SqlReturn {
    match field {
        SQL_DESC_CONCISE_TYPE | SQL_DESC_TYPE => {
            set_type(state, kind, record_number, field, value_ptr)
        }
        SQL_DESC_DATETIME_INTERVAL_CODE => {
            let Ok(code) = SqlSmallInt::try_from(value_ptr as SqlLen) else {
                post_diag(state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            };
            write_record_field(state, record_number, |r| {
                r.datetime_interval_code = code;
                r.explicitly_bound = true;
            })
        }
        SQL_DESC_LENGTH => {
            let len = value_ptr as SqlULen;
            write_record_field(state, record_number, |r| {
                r.length = len;
                r.explicitly_bound = true;
            })
        }
        SQL_DESC_OCTET_LENGTH => {
            let len = value_ptr as SqlLen;
            write_record_field(state, record_number, |r| {
                r.octet_length = len;
                r.explicitly_bound = true;
            })
        }
        SQL_DESC_PRECISION => set_precision(state, record_number, value_ptr),
        SQL_DESC_SCALE => set_scale(state, record_number, value_ptr),
        SQL_DESC_NAME => set_name(state, record_number, value_ptr, buffer_length),
        SQL_DESC_UNNAMED => set_unnamed(state, record_number, value_ptr),
        SQL_DESC_PARAMETER_TYPE => set_parameter_type(state, record_number, value_ptr),
        SQL_DESC_DATA_PTR => set_data_ptr(state, record_number, value_ptr),
        SQL_DESC_INDICATOR_PTR => {
            write_record_field(state, record_number, |r| r.indicator_ptr = value_ptr)
        }
        SQL_DESC_OCTET_LENGTH_PTR => {
            write_record_field(state, record_number, |r| r.octet_length_ptr = value_ptr)
        }
        _ => {
            debug_assert!(
                false,
                "classify_field allowed an unmapped writable record field"
            );
            post_diag(state, ERR_INVALID_DESCRIPTOR_FIELD);
            SQL_ERROR
        }
    }
}

/// Writes a validated value into an existing record via a closure, so
/// validation (which needs `&mut DescState` to post diagnostics) and the
/// mutation (which needs `&mut DescRecord`) never borrow `state` at once.
pub(super) fn write_record_field(
    state: &mut DescState,
    record_number: SqlSmallInt,
    f: impl FnOnce(&mut DescRecord),
) -> SqlReturn {
    let Some(record) = state.record_mut(record_number) else {
        debug_assert!(false, "record should already exist — grown before dispatch");
        return SQL_ERROR;
    };
    f(record);
    SQL_SUCCESS
}

/// `SQL_DESC_TYPE` / `SQL_DESC_CONCISE_TYPE` write. Also derives
/// `SQL_DESC_DATETIME_INTERVAL_CODE` from the resolved concise type so the
/// two fields cannot go stale relative to each other
/// (`datetime_interval_code_for`).
///
/// AD (ARD/APD) types are C types, validated exactly like `SQLBindParameter`
/// validates `ValueType` (`canonical_c_type` + `is_valid_c_type`), so a
/// descriptor-set type can never diverge from what direct binding accepts.
/// C types have no verbose form, so `SQL_DESC_TYPE` and
/// `SQL_DESC_CONCISE_TYPE` are handled identically for AD.
///
/// IPD types are SQL types. `SQL_DESC_TYPE = SQL_DATETIME` (the verbose
/// family marker, `9`) is the standard way to bind a DATE/TIME/TIMESTAMP
/// subtype through a descriptor: `SQL_DESC_DATETIME_INTERVAL_CODE` picks the
/// member, in either write order, and the concise type is derived from
/// whichever is currently stored (matches msodbcsql's own fallthrough,
/// `sqlcdesc.cpp:1635-1641`). This is legal precisely *because*
/// `SQL_DESC_DATETIME_INTERVAL_CODE` is a distinct field that disambiguates
/// it — unlike `SQLBindParameter`'s `ParameterType` (a single scalar
/// argument with no paired subtype field), where `9` is genuinely ambiguous
/// between the 2.x concise `SQL_DATE` spelling and the 3.x verbose
/// `SQL_DATETIME` marker (`.github/instructions/mssql-odbc.instructions.md`).
/// Every other IPD type — including a concise value written through
/// `SQL_DESC_CONCISE_TYPE`, or through `SQL_DESC_TYPE` itself — is validated
/// the same way `SQLBindParameter` validates `ParameterType`
/// (`classify_parameter_sql_type`).
pub(super) fn set_type(
    state: &mut DescState,
    kind: DescKind,
    record_number: SqlSmallInt,
    field: SqlUSmallInt,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let Ok(requested) = SqlSmallInt::try_from(value_ptr as SqlLen) else {
        post_diag(state, ERR_INVALID_ATTRIBUTE_VALUE);
        return SQL_ERROR;
    };

    let resolved = if kind.is_application() {
        let canonical = canonical_c_type(requested);
        if !is_valid_c_type(canonical) {
            error!(requested, "SQLSetDescFieldW: invalid application C type");
            post_diag(state, ERR_INVALID_C_DATA_TYPE);
            return SQL_ERROR;
        }
        canonical
    } else if field == SQL_DESC_TYPE && requested == SQL_DATETIME {
        let current_code = state
            .record(record_number)
            .map(|r| r.datetime_interval_code)
            .unwrap_or(0);
        let Some(concise) = concise_type_for_datetime_code(current_code) else {
            error!(
                "SQLSetDescFieldW: SQL_DESC_TYPE=SQL_DATETIME set before a valid \
                 SQL_DESC_DATETIME_INTERVAL_CODE"
            );
            post_diag(state, ERR_INCONSISTENT_DESCRIPTOR_INFO);
            return SQL_ERROR;
        };
        concise
    } else {
        match classify_parameter_sql_type(requested) {
            SqlTypeSupport::Supported => requested,
            SqlTypeSupport::NotImplemented => {
                error!(requested, "SQLSetDescFieldW: SQL type not implemented");
                post_diag(state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
                return SQL_ERROR;
            }
            SqlTypeSupport::Invalid => {
                error!(requested, "SQLSetDescFieldW: invalid SQL data type");
                post_diag(state, ERR_INVALID_SQL_DATA_TYPE);
                return SQL_ERROR;
            }
        }
    };

    write_record_field(state, record_number, |r| {
        r.concise_type = resolved;
        r.datetime_interval_code = datetime_interval_code_for(resolved);
        r.explicitly_bound = true;
    })
}

/// Inverse of [`datetime_interval_code_for`]: the concise date/time type a
/// verbose `SQL_DESC_TYPE = SQL_DATETIME` write resolves to, given the
/// record's currently-stored `SQL_DESC_DATETIME_INTERVAL_CODE`. `None` if no
/// recognized code is stored yet.
fn concise_type_for_datetime_code(code: SqlSmallInt) -> Option<SqlSmallInt> {
    let date = SqlSmallInt::try_from(SQL_CODE_DATE).ok()?;
    let time = SqlSmallInt::try_from(SQL_CODE_TIME).ok()?;
    let timestamp = SqlSmallInt::try_from(SQL_CODE_TIMESTAMP).ok()?;
    if code == date {
        Some(SQL_TYPE_DATE)
    } else if code == time {
        Some(SQL_TYPE_TIME)
    } else if code == timestamp {
        Some(SQL_TYPE_TIMESTAMP)
    } else {
        None
    }
}

/// The `SQL_DESC_DATETIME_INTERVAL_CODE` a concise type couples to: the
/// three ODBC 3.x date/time concise types carry their matching code, every
/// other type carries none. The ODBC interval family
/// (`SQL_INTERVAL_YEAR..SQL_INTERVAL_MINUTE_TO_SECOND`) is not modeled here:
/// SQL Server has no interval SQL type, so no concise interval value can
/// reach a descriptor record through this driver's execution path.
pub(crate) fn datetime_interval_code_for(concise_type: SqlSmallInt) -> SqlSmallInt {
    let code = match concise_type {
        SQL_TYPE_DATE => SQL_CODE_DATE,
        SQL_TYPE_TIME => SQL_CODE_TIME,
        SQL_TYPE_TIMESTAMP => SQL_CODE_TIMESTAMP,
        _ => return 0,
    };
    SqlSmallInt::try_from(code).unwrap_or(0)
}

/// `SQL_DESC_PRECISION` write. When the record's concise type is
/// `SQL_C_NUMERIC`, enforces msodbcsql's `CheckADDescRecConsistency` bound
/// (`1..=SQL_PREC_NUMERIC`, `sqlcdesc.cpp:11384-11389`) immediately rather
/// than deferring to an execute-time consistency pass this driver does not
/// have yet — task AB#47297 calls out "numeric value representation"
/// validation as this PR's job.
pub(super) fn set_precision(
    state: &mut DescState,
    record_number: SqlSmallInt,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let Ok(precision) = SqlSmallInt::try_from(value_ptr as SqlLen) else {
        post_diag(state, ERR_INVALID_ATTRIBUTE_VALUE);
        return SQL_ERROR;
    };

    let is_numeric = state.record(record_number).map(|r| r.concise_type) == Some(SQL_C_NUMERIC);
    if is_numeric && !(1..=SQL_PREC_NUMERIC).contains(&precision) {
        error!(
            precision,
            "SQLSetDescFieldW: invalid SQL_C_NUMERIC precision"
        );
        post_diag(state, ERR_INVALID_PRECISION_OR_SCALE);
        return SQL_ERROR;
    }

    write_record_field(state, record_number, |r| {
        r.precision = precision;
        r.explicitly_bound = true;
    })
}

/// `SQL_DESC_SCALE` write. Same `SQL_C_NUMERIC` consistency bound as
/// [`set_precision`]: scale must be non-negative and `<= precision`
/// (`sqlcdesc.cpp:11391-11394`).
pub(super) fn set_scale(
    state: &mut DescState,
    record_number: SqlSmallInt,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let Ok(scale) = SqlSmallInt::try_from(value_ptr as SqlLen) else {
        post_diag(state, ERR_INVALID_ATTRIBUTE_VALUE);
        return SQL_ERROR;
    };

    let record_info = state
        .record(record_number)
        .map(|r| (r.concise_type, r.precision));
    if let Some((SQL_C_NUMERIC, precision)) = record_info
        && (scale < 0 || scale > precision)
    {
        error!(
            scale,
            precision, "SQLSetDescFieldW: invalid SQL_C_NUMERIC scale"
        );
        post_diag(state, ERR_INVALID_PRECISION_OR_SCALE);
        return SQL_ERROR;
    }

    write_record_field(state, record_number, |r| {
        r.scale = scale;
        r.explicitly_bound = true;
    })
}

/// `SQL_DESC_DATA_PTR` write (AD only). Re-validates the record's
/// `SQL_C_NUMERIC` precision/scale against whatever is currently stored
/// before accepting the pointer, matching msodbcsql's final
/// `CheckADDescRecConsistency` pass at bind time (`sqlcdesc.cpp:11380-11394`).
/// `set_precision`/`set_scale` only validate against the type stored *at the
/// time each is written*, so an out-of-range precision set before the type
/// changes to `SQL_C_NUMERIC` would otherwise go unnoticed; `DATA_PTR` is
/// the last field mssql-python's binding sequence sets, so this is the
/// natural point to catch it without deferring all the way to execute time.
///
/// Deliberately unconditional on whether `value_ptr` is null (an unbind).
/// `CheckADDescRecConsistency` has an early `rgbValue == NOT_BOUND` (i.e.
/// null) return, but msodbcsql's actual `SetADField` caller for
/// `SQL_DESC_DATA_PTR` (`sqlcdesc.cpp:4219-4235`) defeats that shortcut on
/// purpose: it forces `rgbValue` to a non-null sentinel before calling the
/// check, and only assigns the real (possibly null) requested value if the
/// check passes — so msodbcsql validates precision/scale on every
/// `SQL_DESC_DATA_PTR` write, bind *or* unbind, matching this
/// implementation. `NOT_BOUND`'s early return is real, but it is only ever
/// reachable from the whole-descriptor sweep at execute time
/// (`CheckADDescConsistency`, over each record's live `rgbValue`), a
/// different call site skipping records nothing is currently bound to —
/// not from a single `SQLSetDescField(SQL_DESC_DATA_PTR, ...)` call.
pub(super) fn set_data_ptr(
    state: &mut DescState,
    record_number: SqlSmallInt,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let record_info = state
        .record(record_number)
        .map(|r| (r.concise_type, r.precision, r.scale));
    if let Some((SQL_C_NUMERIC, precision, scale)) = record_info
        && (!(1..=SQL_PREC_NUMERIC).contains(&precision) || scale < 0 || scale > precision)
    {
        error!(
            precision,
            scale,
            "SQLSetDescFieldW: SQL_DESC_DATA_PTR set with an inconsistent SQL_C_NUMERIC precision/scale"
        );
        post_diag(state, ERR_INVALID_PRECISION_OR_SCALE);
        return SQL_ERROR;
    }
    write_record_field(state, record_number, |r| r.data_ptr = value_ptr)
}

/// `SQL_DESC_NAME` write (IPD only — the only kind `classify_field` marks
/// this field writable for). `buffer_length` is in bytes, or `SQL_NTS` for
/// NUL-terminated input, matching ODBC's general character-input rule.
fn set_name(
    state: &mut DescState,
    record_number: SqlSmallInt,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
) -> SqlReturn {
    // Unlike a connection string or catalog wildcard, a null buffer has no
    // valid meaning for SQL_DESC_NAME — reject it before it ever reaches
    // `read_utf16` (an `unsafe fn` that dereferences the pointer
    // unconditionally). A null `Value` here is genuine application input,
    // not something the Driver Manager filters out: verified against this
    // exact call sequence, an unchecked null crashes the process with a
    // non-unwinding access-violation abort that `ffi_entry!`'s
    // `catch_unwind` cannot intercept.
    if value_ptr.is_null() {
        error!("SQLSetDescFieldW: SQL_DESC_NAME value_ptr is null");
        post_diag(state, ERR_INVALID_NULL_POINTER);
        return SQL_ERROR;
    }

    let ptr = value_ptr as *const SqlWChar;
    let name = if buffer_length == SqlInteger::from(SQL_NTS) {
        unsafe { read_utf16(ptr, SQL_NTS) }
    } else {
        let Ok(byte_len) = usize::try_from(buffer_length) else {
            post_diag(state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
            return SQL_ERROR;
        };
        let Ok(char_len) = SqlSmallInt::try_from(byte_len / size_of::<SqlWChar>()) else {
            post_diag(state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
            return SQL_ERROR;
        };
        unsafe { read_utf16(ptr, char_len) }
    };
    write_record_field(state, record_number, |r| r.name = name)
}

/// `SQL_DESC_UNNAMED` write (IPD only — `classify_field` marks it read-only
/// everywhere else). The field is derived from `name` on read rather than
/// stored separately, but `SQL_UNNAMED` is a valid write that clears the
/// parameter name; any other value is rejected. Matches the ODBC reference
/// (*"A driver returns SQLSTATE HY091 ... if an application attempts to set
/// the SQL_DESC_UNNAMED field of an IPD to SQL_NAMED"*) and msodbcsql's
/// `SetIPDField` (`sqlcdesc.cpp:4873-4884`).
fn set_unnamed(
    state: &mut DescState,
    record_number: SqlSmallInt,
    value_ptr: SqlPointer,
) -> SqlReturn {
    if value_ptr as SqlLen != SQL_UNNAMED {
        error!("SQLSetDescFieldW: SQL_DESC_UNNAMED only accepts SQL_UNNAMED");
        post_diag(state, ERR_INVALID_DESCRIPTOR_FIELD);
        return SQL_ERROR;
    }
    write_record_field(state, record_number, |r| r.name.clear())
}

/// `SQL_DESC_PARAMETER_TYPE` write (IPD only). Validated against the three
/// ODBC 3.x `SQL_PARAM_*` values this driver can act on — an arbitrary
/// `SqlSmallInt` that merely fits the wire width (e.g. `9999`) is not a
/// real parameter type and must not be stored as though it were.
///
/// Two deliberate divergences from msodbcsql's `SetIPDField`
/// (`sqlcdesc.cpp:4855-4870`), neither exercised by an e2e parity test:
/// msodbcsql returns `HY092` here, not `HY105` (this crate's choice is the
/// more specific SQLSTATE the ODBC reference itself documents for this
/// field); and msodbcsql also accepts `SQL_PARAM_INPUT_OUTPUT_STREAM` /
/// `SQL_PARAM_OUTPUT_STREAM`, both valid per the reference — rejected here
/// since this driver has no streamed-parameter support to back them.
fn set_parameter_type(
    state: &mut DescState,
    record_number: SqlSmallInt,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let Ok(requested) = SqlSmallInt::try_from(value_ptr as SqlLen) else {
        post_diag(state, ERR_INVALID_ATTRIBUTE_VALUE);
        return SQL_ERROR;
    };
    if !matches!(
        requested,
        SQL_PARAM_INPUT | SQL_PARAM_INPUT_OUTPUT | SQL_PARAM_OUTPUT
    ) {
        error!(
            requested,
            "SQLSetDescFieldW: invalid SQL_DESC_PARAMETER_TYPE"
        );
        post_diag(state, ERR_INVALID_PARAMETER_TYPE);
        return SQL_ERROR;
    }
    write_record_field(state, record_number, |r| r.parameter_type = requested)
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::get_desc_field::sql_get_desc_field_w;
    use crate::api::odbc_types::{
        SQL_ATTR_APP_PARAM_DESC, SQL_C_LONG, SQL_C_WCHAR, SQL_INTEGER, SQL_INTERVAL_YEAR,
        SQL_INVALID_HANDLE, SQL_NAMED, SQL_NULL_HANDLE, SQL_TYPE_DATE, SqlNumericStruct,
    };
    use crate::api::set_stmt_attr::sql_get_stmt_attr_w;
    use crate::error::diag::DiagRecord;
    use crate::handles::{DescHandle, handle_from_raw};
    use crate::test_support::TestHandles;

    // `SQL_DESC_*` field-identifier constants are `SqlUSmallInt`-typed (shared
    // with `SQLColAttributeW`'s unsigned `FieldIdentifier`), but
    // `SQLGetDescFieldW`/`SQLSetDescFieldW`'s `FieldIdentifier` is signed per
    // the ODBC prototype. Every real field id is small and non-negative, so
    // this is a lossless, compile-time-constant retyping done once per name
    // here rather than a cast at every call site below.
    const SQL_DESC_TYPE: SqlSmallInt = crate::api::odbc_types::SQL_DESC_TYPE as SqlSmallInt;
    const SQL_DESC_CONCISE_TYPE: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_CONCISE_TYPE as SqlSmallInt;
    const SQL_DESC_COUNT: SqlSmallInt = crate::api::odbc_types::SQL_DESC_COUNT as SqlSmallInt;
    const SQL_DESC_ALLOC_TYPE: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_ALLOC_TYPE as SqlSmallInt;
    const SQL_DESC_PRECISION: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_PRECISION as SqlSmallInt;
    const SQL_DESC_SCALE: SqlSmallInt = crate::api::odbc_types::SQL_DESC_SCALE as SqlSmallInt;
    const SQL_DESC_DATA_PTR: SqlSmallInt = crate::api::odbc_types::SQL_DESC_DATA_PTR as SqlSmallInt;
    const SQL_DESC_ARRAY_SIZE: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_ARRAY_SIZE as SqlSmallInt;
    const SQL_DESC_ARRAY_STATUS_PTR: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_ARRAY_STATUS_PTR as SqlSmallInt;
    const SQL_DESC_ROWS_PROCESSED_PTR: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_ROWS_PROCESSED_PTR as SqlSmallInt;
    const SQL_DESC_DATETIME_INTERVAL_CODE: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_DATETIME_INTERVAL_CODE as SqlSmallInt;
    const SQL_DESC_NAME: SqlSmallInt = crate::api::odbc_types::SQL_DESC_NAME as SqlSmallInt;
    const SQL_DESC_UNNAMED: SqlSmallInt = crate::api::odbc_types::SQL_DESC_UNNAMED as SqlSmallInt;
    const SQL_DESC_PARAMETER_TYPE: SqlSmallInt =
        crate::api::odbc_types::SQL_DESC_PARAMETER_TYPE as SqlSmallInt;

    fn assert_last_diag(records: &[DiagRecord], expected: crate::api::sqlstate::DiagMsg) {
        let d = records.last().expect("expected a diagnostic record");
        assert_eq!(d.sql_state, expected.state, "SQLSTATE mismatch");
        assert!(
            d.message.contains(expected.text),
            "message {:?} did not contain {:?}",
            d.message,
            expected.text
        );
    }

    fn desc_diags(handle: SqlHandle) -> Vec<DiagRecord> {
        let desc = unsafe { handle_from_raw::<DescHandle>(handle) };
        desc.inner.lock().unwrap().diag_records.clone()
    }

    fn get_small_int(handle: SqlHandle, record: SqlSmallInt, field: SqlSmallInt) -> SqlSmallInt {
        let mut value: SqlSmallInt = -1;
        let ret = unsafe {
            sql_get_desc_field_w(
                handle,
                record,
                field,
                &mut value as *mut SqlSmallInt as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS, "GET failed reading back field {field}");
        value
    }

    #[test]
    fn set_desc_field_null_handle_returns_invalid_handle() {
        let ret =
            unsafe { sql_set_desc_field_w(SQL_NULL_HANDLE, 1, SQL_DESC_TYPE, ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    /// The exact sequence `mssql-python`'s `ddbc_bindings.cpp` runs for a
    /// `SQL_C_NUMERIC` input parameter (`BindParameters`, lines 1003-1048):
    /// `SQLGetStmtAttr(APP_PARAM_DESC)` then four `SQLSetDescField` calls on
    /// record 1, in this order. This is the regression anchor for AB#47297.
    #[test]
    fn mssql_python_numeric_parameter_sequence_succeeds() {
        let h = TestHandles::with_env_dbc_stmt();

        let mut hdesc: SqlHandle = SQL_NULL_HANDLE;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_APP_PARAM_DESC,
                &mut hdesc as *mut SqlHandle as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(
            hdesc,
            h.apd(),
            "SQLGetStmtAttr must return the implicit APD"
        );

        let ret = unsafe {
            sql_set_desc_field_w(
                hdesc,
                1,
                SQL_DESC_TYPE,
                SQL_C_NUMERIC as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS, "SQL_DESC_TYPE");

        let ret =
            unsafe { sql_set_desc_field_w(hdesc, 1, SQL_DESC_PRECISION, 10isize as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS, "SQL_DESC_PRECISION");

        let ret =
            unsafe { sql_set_desc_field_w(hdesc, 1, SQL_DESC_SCALE, 2isize as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS, "SQL_DESC_SCALE");

        let mut numeric_buf: SqlNumericStruct = SqlNumericStruct::default();
        let ret = unsafe {
            sql_set_desc_field_w(
                hdesc,
                1,
                SQL_DESC_DATA_PTR,
                &mut numeric_buf as *mut SqlNumericStruct as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS, "SQL_DESC_DATA_PTR");

        // Every field is readable back through SQLGetDescFieldW afterward.
        assert_eq!(
            get_small_int(hdesc, 1, SQL_DESC_CONCISE_TYPE),
            SQL_C_NUMERIC
        );
        assert_eq!(get_small_int(hdesc, 1, SQL_DESC_PRECISION), 10);
        assert_eq!(get_small_int(hdesc, 1, SQL_DESC_SCALE), 2);
        let mut data_ptr: SqlPointer = ptr::null_mut();
        let ret = unsafe {
            sql_get_desc_field_w(
                hdesc,
                1,
                SQL_DESC_DATA_PTR,
                &mut data_ptr as *mut SqlPointer as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(
            data_ptr,
            &mut numeric_buf as *mut SqlNumericStruct as SqlPointer
        );
    }

    #[test]
    fn set_desc_field_count_grows_and_shrinks() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_desc_field_w(h.apd(), 0, SQL_DESC_COUNT, 3isize as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(get_small_int(h.apd(), 0, SQL_DESC_COUNT), 3);

        let ret =
            unsafe { sql_set_desc_field_w(h.apd(), 0, SQL_DESC_COUNT, 1isize as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(get_small_int(h.apd(), 0, SQL_DESC_COUNT), 1);

        let ret = unsafe {
            sql_get_desc_field_w(
                h.apd(),
                2,
                SQL_DESC_TYPE,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, crate::api::odbc_types::SQL_NO_DATA);
    }

    #[test]
    fn set_desc_field_grows_count_implicitly_on_record_write() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.apd(),
                3,
                SQL_DESC_TYPE,
                SQL_C_LONG as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(get_small_int(h.apd(), 0, SQL_DESC_COUNT), 3);
        // Records 1-2 exist with default values, not left as gaps.
        assert_eq!(
            get_small_int(h.apd(), 1, SQL_DESC_CONCISE_TYPE),
            crate::api::odbc_types::SQL_C_DEFAULT
        );
    }

    #[test]
    fn set_desc_field_ird_rejects_type_write_with_hy016() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ird(),
                1,
                SQL_DESC_TYPE,
                SQL_INTEGER as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ird()), ERR_CANNOT_MODIFY_IRD);
    }

    #[test]
    fn set_desc_field_ird_allows_rows_processed_and_array_status_ptr() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut rows: SqlULen = 0;
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ird(),
                0,
                SQL_DESC_ROWS_PROCESSED_PTR,
                &mut rows as *mut SqlULen as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        let mut status: SqlUSmallInt = 0;
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ird(),
                0,
                SQL_DESC_ARRAY_STATUS_PTR,
                &mut status as *mut SqlUSmallInt as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_desc_field_unknown_field_returns_hy091() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_desc_field_w(h.ard(), 1, 0x7FFF, ptr::null_mut(), 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ard()), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    #[test]
    fn set_desc_field_read_only_field_returns_hy091() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(h.ard(), 0, SQL_DESC_ALLOC_TYPE, 2isize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ard()), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    #[test]
    fn set_desc_field_invalid_record_number_returns_invalid_descriptor_index() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.apd(),
                0,
                SQL_DESC_TYPE,
                SQL_C_LONG as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_DESCRIPTOR_INDEX);
    }

    #[test]
    fn set_desc_field_invalid_c_type_on_ad_returns_hy003() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_desc_field_w(h.apd(), 1, SQL_DESC_TYPE, 9999isize as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_C_DATA_TYPE);
    }

    #[test]
    fn set_desc_field_invalid_sql_type_on_ipd_returns_hy004() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_desc_field_w(h.ipd(), 1, SQL_DESC_TYPE, 9999isize as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ipd()), ERR_INVALID_SQL_DATA_TYPE);
    }

    #[test]
    fn set_desc_field_concise_type_rejects_legacy_datetime_values() {
        // SQL_DESC_CONCISE_TYPE never gets verbose treatment: 9/10/11 remain
        // ambiguous/unsupported there (unlike SQL_DESC_TYPE=9, see below).
        let h = TestHandles::with_env_dbc_stmt();
        for legacy in [9isize, 10, 11] {
            let ret = unsafe {
                sql_set_desc_field_w(h.ipd(), 1, SQL_DESC_CONCISE_TYPE, legacy as SqlPointer, 0)
            };
            assert_eq!(ret, SQL_ERROR, "legacy value {legacy}");
        }
    }

    #[test]
    fn set_desc_field_type_legacy_time_timestamp_values_still_rejected() {
        // Only the true verbose SQL_DATETIME (9) marker gets special
        // handling on SQL_DESC_TYPE; the deprecated 2.x SQL_TIME(10)/
        // SQL_TIMESTAMP(11) concise spellings are not real SQL types this
        // driver recognizes there.
        let h = TestHandles::with_env_dbc_stmt();
        for legacy in [10isize, 11] {
            let ret =
                unsafe { sql_set_desc_field_w(h.ipd(), 1, SQL_DESC_TYPE, legacy as SqlPointer, 0) };
            assert_eq!(ret, SQL_ERROR, "legacy value {legacy}");
            assert_last_diag(&desc_diags(h.ipd()), ERR_INVALID_SQL_DATA_TYPE);
        }
    }

    /// Regression: verbose `SQL_DESC_TYPE = SQL_DATETIME` on IPD must be
    /// *accepted*, not rejected as ambiguous — it is the standard ODBC way
    /// to bind a DATE/TIME/TIMESTAMP subtype through a descriptor, precisely
    /// because `SQL_DESC_DATETIME_INTERVAL_CODE` disambiguates it (see
    /// `set_type`'s doc comment). Without a prior interval code, though, the
    /// pair is genuinely inconsistent.
    #[test]
    fn set_desc_field_verbose_type_datetime_without_interval_code_is_inconsistent() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_TYPE,
                SQL_DATETIME as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ipd()), ERR_INCONSISTENT_DESCRIPTOR_INFO);
    }

    #[test]
    fn set_desc_field_verbose_type_datetime_derives_concise_from_interval_code() {
        let h = TestHandles::with_env_dbc_stmt();
        let code = SqlSmallInt::try_from(crate::api::odbc_types::SQL_CODE_TIMESTAMP).unwrap();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_DATETIME_INTERVAL_CODE,
                code as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_TYPE,
                SQL_DATETIME as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(
            get_small_int(h.ipd(), 1, SQL_DESC_CONCISE_TYPE),
            SQL_TYPE_TIMESTAMP
        );
    }

    #[test]
    fn set_desc_field_interval_type_on_ipd_returns_optional_feature_not_implemented() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_TYPE,
                SQL_INTERVAL_YEAR as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(
            &desc_diags(h.ipd()),
            crate::api::sqlstate::ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED,
        );
    }

    #[test]
    fn set_desc_field_concise_datetime_type_derives_interval_code() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_TYPE,
                SQL_TYPE_DATE as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(
            get_small_int(h.ipd(), 1, SQL_DESC_DATETIME_INTERVAL_CODE),
            SqlSmallInt::try_from(crate::api::odbc_types::SQL_CODE_DATE).unwrap()
        );

        // Changing to a non-datetime type clears the stale interval code.
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_TYPE,
                SQL_INTEGER as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(
            get_small_int(h.ipd(), 1, SQL_DESC_DATETIME_INTERVAL_CODE),
            0
        );
    }

    #[test]
    fn set_desc_field_numeric_precision_out_of_range_returns_hy094() {
        let h = TestHandles::with_env_dbc_stmt();
        unsafe {
            sql_set_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_TYPE,
                SQL_C_NUMERIC as isize as SqlPointer,
                0,
            )
        };

        for bad in [0isize, -1, 39] {
            let ret = unsafe {
                sql_set_desc_field_w(h.apd(), 1, SQL_DESC_PRECISION, bad as SqlPointer, 0)
            };
            assert_eq!(ret, SQL_ERROR, "precision {bad}");
            assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_PRECISION_OR_SCALE);
        }
    }

    #[test]
    fn set_desc_field_numeric_scale_exceeding_precision_returns_hy094() {
        let h = TestHandles::with_env_dbc_stmt();
        unsafe {
            sql_set_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_TYPE,
                SQL_C_NUMERIC as isize as SqlPointer,
                0,
            )
        };
        unsafe { sql_set_desc_field_w(h.apd(), 1, SQL_DESC_PRECISION, 5isize as SqlPointer, 0) };

        let ret =
            unsafe { sql_set_desc_field_w(h.apd(), 1, SQL_DESC_SCALE, 6isize as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_PRECISION_OR_SCALE);

        let ret =
            unsafe { sql_set_desc_field_w(h.apd(), 1, SQL_DESC_SCALE, (-1isize) as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_PRECISION_OR_SCALE);
    }

    #[test]
    fn set_desc_field_non_numeric_type_does_not_enforce_precision_bound() {
        let h = TestHandles::with_env_dbc_stmt();
        unsafe {
            sql_set_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_TYPE,
                SQL_C_WCHAR as isize as SqlPointer,
                0,
            )
        };

        // Precision has no numeric-specific meaning for a character type; any
        // in-range SqlSmallInt is accepted.
        let ret = unsafe {
            sql_set_desc_field_w(h.apd(), 1, SQL_DESC_PRECISION, 100isize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    /// Regression: `set_precision`/`set_scale` only validate against the
    /// type stored *at the time each is written*. An out-of-range precision
    /// set while the type is still something else (so the numeric bound
    /// didn't apply yet), followed by changing the type to `SQL_C_NUMERIC`,
    /// must still be caught — matching msodbcsql's final consistency check
    /// at bind time — rather than silently accepted.
    #[test]
    fn set_desc_field_data_ptr_catches_precision_set_before_type_became_numeric() {
        let h = TestHandles::with_env_dbc_stmt();
        // Precision 39 is out of range only once the type becomes SQL_C_NUMERIC.
        unsafe {
            sql_set_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_TYPE,
                SQL_C_WCHAR as isize as SqlPointer,
                0,
            )
        };
        let ret = unsafe {
            sql_set_desc_field_w(h.apd(), 1, SQL_DESC_PRECISION, 39isize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
        let ret = unsafe {
            sql_set_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_TYPE,
                SQL_C_NUMERIC as isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        let mut numeric_buf = SqlNumericStruct::default();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.apd(),
                1,
                SQL_DESC_DATA_PTR,
                &mut numeric_buf as *mut SqlNumericStruct as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_PRECISION_OR_SCALE);
    }

    #[test]
    fn set_desc_field_name_write_and_read_back_on_ipd() {
        let h = TestHandles::with_env_dbc_stmt();
        // NUL-terminated: SQL_NTS tells read_utf16 to scan for the
        // terminator, so the buffer must actually have one.
        let name: Vec<u16> = "p1".encode_utf16().chain(std::iter::once(0)).collect();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_NAME,
                name.as_ptr() as SqlPointer,
                SQL_NTS.into(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);

        let mut buf: [SqlWChar; 8] = [0; 8];
        let ret = unsafe {
            sql_get_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_NAME,
                buf.as_mut_ptr() as SqlPointer,
                SqlInteger::try_from(buf.len() * size_of::<SqlWChar>()).unwrap(),
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let len = buf.iter().position(|c| *c == 0).unwrap();
        assert_eq!(String::from_utf16(&buf[..len]).unwrap(), "p1");
    }

    #[test]
    fn set_desc_field_parameter_type_accepts_valid_values_and_rejects_others() {
        let h = TestHandles::with_env_dbc_stmt();
        for valid in [SQL_PARAM_INPUT, SQL_PARAM_INPUT_OUTPUT, SQL_PARAM_OUTPUT] {
            let ret = unsafe {
                sql_set_desc_field_w(
                    h.ipd(),
                    1,
                    SQL_DESC_PARAMETER_TYPE,
                    valid as isize as SqlPointer,
                    0,
                )
            };
            assert_eq!(ret, SQL_SUCCESS, "valid parameter type {valid}");
        }

        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_PARAMETER_TYPE,
                9999isize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ipd()), ERR_INVALID_PARAMETER_TYPE);
    }

    #[test]
    fn set_desc_field_name_write_rejected_on_ard() {
        let h = TestHandles::with_env_dbc_stmt();
        let name: Vec<u16> = "x".encode_utf16().chain(std::iter::once(0)).collect();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ard(),
                1,
                SQL_DESC_NAME,
                name.as_ptr() as SqlPointer,
                SQL_NTS.into(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ard()), ERR_INVALID_DESCRIPTOR_FIELD);
    }

    /// Regression: a null `value_ptr` with `SQL_DESC_NAME` used to reach
    /// `read_utf16` unchecked and dereference null — reproduced as a
    /// non-unwinding process abort before the fix (`ffi_entry!`'s
    /// `catch_unwind` cannot intercept an abort). Must now fail cleanly.
    #[test]
    fn set_desc_field_name_null_value_ptr_returns_error_not_crash() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(h.ipd(), 1, SQL_DESC_NAME, ptr::null_mut(), SQL_NTS.into())
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ipd()), ERR_INVALID_NULL_POINTER);
    }

    /// Regression: `SQL_DESC_UNNAMED` is derived from `name` on read, but is
    /// writable on IPD to `SQL_UNNAMED` — the ODBC reference and msodbcsql's
    /// `SetIPDField` both make this the one legal write for the field,
    /// clearing the parameter name. Any other value (e.g. `SQL_NAMED`) is
    /// rejected with HY091.
    #[test]
    fn set_desc_field_unnamed_accepts_only_sql_unnamed() {
        let h = TestHandles::with_env_dbc_stmt();
        // NUL-terminated: SQL_NTS tells read_utf16 to scan for the
        // terminator, so the buffer must actually have one.
        let name: Vec<u16> = "p1".encode_utf16().chain(std::iter::once(0)).collect();
        let ret = unsafe {
            sql_set_desc_field_w(
                h.ipd(),
                1,
                SQL_DESC_NAME,
                name.as_ptr() as SqlPointer,
                SQL_NTS.into(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(get_small_int(h.ipd(), 1, SQL_DESC_UNNAMED), 0);

        let ret = unsafe {
            sql_set_desc_field_w(h.ipd(), 1, SQL_DESC_UNNAMED, SQL_NAMED as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.ipd()), ERR_INVALID_DESCRIPTOR_FIELD);

        let ret = unsafe {
            sql_set_desc_field_w(h.ipd(), 1, SQL_DESC_UNNAMED, SQL_UNNAMED as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(get_small_int(h.ipd(), 1, SQL_DESC_UNNAMED), 1);
    }

    #[test]
    fn set_desc_field_array_size_zero_returns_error() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(h.apd(), 0, SQL_DESC_ARRAY_SIZE, 0isize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_ERROR);
        assert_last_diag(&desc_diags(h.apd()), ERR_INVALID_ATTRIBUTE_VALUE);
    }

    #[test]
    fn set_desc_field_array_size_clamped_with_warning() {
        let h = TestHandles::with_env_dbc_stmt();
        let huge = (i32::MAX as isize) + 1000;
        let ret =
            unsafe { sql_set_desc_field_w(h.apd(), 0, SQL_DESC_ARRAY_SIZE, huge as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        assert_last_diag(&desc_diags(h.apd()), WARN_ARRAY_SIZE_CHANGED);

        let mut array_size: SqlULen = 0;
        let ret = unsafe {
            sql_get_desc_field_w(
                h.apd(),
                0,
                SQL_DESC_ARRAY_SIZE,
                &mut array_size as *mut SqlULen as SqlPointer,
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(array_size, SqlULen::try_from(i32::MAX).unwrap());
    }

    #[test]
    fn set_desc_field_diagnostics_cleared_on_new_call() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_desc_field_w(h.ard(), 0, SQL_DESC_ALLOC_TYPE, 2isize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(desc_diags(h.ard()).len(), 1);

        // A subsequent successful call clears the stale diagnostic.
        let ret = unsafe {
            sql_set_desc_field_w(h.ard(), 0, SQL_DESC_ARRAY_SIZE, 5isize as SqlPointer, 0)
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert!(desc_diags(h.ard()).is_empty());
    }
}
