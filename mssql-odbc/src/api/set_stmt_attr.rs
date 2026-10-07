// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLSetStmtAttrW` / `SQLGetStmtAttrW`.
//!
//! The block-fetch rowset controls (`SQL_ATTR_ROW_ARRAY_SIZE`,
//! `SQL_ATTR_ROWS_FETCHED_PTR`, `SQL_ATTR_ROW_STATUS_PTR`,
//! `SQL_ATTR_ROW_BIND_TYPE`) are stored and later consumed by the columnar
//! fetch path. `SQL_ATTR_CURSOR_TYPE` and `SQL_ATTR_CONCURRENCY` accept only the
//! supported forward-only / read-only values; any other request is substituted
//! and reported with `01S02` (option value changed) rather than silently
//! succeeding; `SQL_ATTR_CURSOR_SCROLLABLE` is the same setting seen from the
//! other side and behaves identically. `SQL_ATTR_QUERY_TIMEOUT` is stored and
//! clamped to [`MAX_QUERY_TIMEOUT`], matching msodbcsql, which reports `01S02`
//! rather than rejecting an over-large request; the stored timeout is enforced
//! against the running statement by `SQLExecute`/`SQLExecDirectW` (AB#46385).
//! `SQL_ATTR_MAX_ROWS` is stored and genuinely enforced by the fetch path.
//! Other recognized parameter and cursor controls are stored and
//! round-tripped without effect, because msodbcsql reports back whatever was
//! written; the handful whose reported value msodbcsql pins regardless of the
//! request (`SQL_ATTR_MAX_LENGTH`, `SQL_ATTR_KEYSET_SIZE`,
//! `SQL_ATTR_SIMULATE_CURSOR`) substitute and warn with `01S02`.
//! Parameter-array attributes are stored and consumed by `SQLExecute`; data-at-
//! execution and output arrays remain explicitly unsupported.
//! `SQL_ATTR_METADATA_ID` accepts its
//! pattern-mode default (`SQL_FALSE`) but returns `HYC00` for `SQL_TRUE` until
//! catalog calls implement identifier matching. Unrecognized attribute
//! identifiers fail with `HY092`.
//!
//! The SQL Server vendor attributes (`SQL_SOPT_SS_*`, ids 1225-1238) differ from
//! the rest in that the driver, not the Driver Manager, validates their values:
//! the DM knows nothing about these identifiers and forwards whatever it is
//! given, so each one carries the accept rule measured from msodbcsql and
//! answers `HY024` outside it, leaving the previous value in place. Two are
//! get-only and answer `HY092` on set (`SQL_SOPT_SS_CURRENT_COMMAND`, which
//! reports the batch command ordinal, and `SQL_SOPT_SS_NOCOUNT_STATUS`), and two
//! are string-valued (the query-notification message text and options) — the
//! only statement attributes that use `buffer_length` / `string_length_ptr`
//! rather than passing an integer in the pointer slot. A null pointer clears
//! either string; msodbcsql faults on that input, so this is a deliberate safe
//! result rather than parity.
//!
//! `SQL_ATTR_APP_ROW_DESC`/`SQL_ATTR_APP_PARAM_DESC` associate an explicitly
//! allocated descriptor as the statement's active ARD/APD after validating
//! that it belongs to the same connection.
//!
//! Each entry point follows the crate's mandatory layering: FFI panic boundary
//! → `unsafe` raw-handle shim → safe core (`README.md`; `num_result_cols.rs`).

use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, error, warn};

use crate::api::attributes::{AttrOp, AttrScope, unimplemented_attr_diag};
use crate::api::odbc_types::{
    MAX_QUERY_TIMEOUT, MSODBCSQL_MAX_LENGTH, SQL_ATTR_APP_PARAM_DESC, SQL_ATTR_APP_ROW_DESC,
    SQL_ATTR_CONCURRENCY, SQL_ATTR_CURSOR_SCROLLABLE, SQL_ATTR_CURSOR_SENSITIVITY,
    SQL_ATTR_CURSOR_TYPE, SQL_ATTR_IMP_PARAM_DESC, SQL_ATTR_IMP_ROW_DESC, SQL_ATTR_KEYSET_SIZE,
    SQL_ATTR_MAX_LENGTH, SQL_ATTR_MAX_ROWS, SQL_ATTR_METADATA_ID, SQL_ATTR_PARAM_BIND_OFFSET_PTR,
    SQL_ATTR_PARAM_BIND_TYPE, SQL_ATTR_PARAM_OPERATION_PTR, SQL_ATTR_PARAM_STATUS_PTR,
    SQL_ATTR_PARAMS_PROCESSED_PTR, SQL_ATTR_PARAMSET_SIZE, SQL_ATTR_QUERY_TIMEOUT,
    SQL_ATTR_ROW_ARRAY_SIZE, SQL_ATTR_ROW_BIND_OFFSET_PTR, SQL_ATTR_ROW_BIND_TYPE,
    SQL_ATTR_ROW_NUMBER, SQL_ATTR_ROW_OPERATION_PTR, SQL_ATTR_ROW_STATUS_PTR,
    SQL_ATTR_ROWS_FETCHED_PTR, SQL_ATTR_SIMULATE_CURSOR, SQL_CONCUR_READ_ONLY,
    SQL_CURSOR_FORWARD_ONLY, SQL_ERROR, SQL_FALSE, SQL_INSENSITIVE, SQL_INVALID_HANDLE,
    SQL_NONSCROLLABLE, SQL_NTS, SQL_ROWSET_SIZE, SQL_SC_UNIQUE, SQL_SOPT_SS_CURRENT_COMMAND,
    SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT, SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS, SQL_SUCCESS,
    SQL_SUCCESS_WITH_INFO, SQL_TRUE, SqlHandle, SqlInteger, SqlPointer, SqlReturn, SqlULen,
    SqlWChar,
};
use crate::api::set_desc_field::clamp_array_size;
use crate::api::sqlstate::{
    DiagMsg, ERR_FUNCTION_SEQUENCE, ERR_INVALID_ATTRIBUTE_VALUE, ERR_INVALID_CURSOR_STATE,
    ERR_INVALID_USE_OF_AUTO_DESC, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED, SQLSTATE_01S02,
    SQLSTATE_HY000, WARN_ARRAY_SIZE_CHANGED, WARN_OPTION_VALUE_CHANGED, post_diag,
};
use crate::api::util::{read_utf16_attr, write_if_some, write_wide_attr};
use crate::error::{free_errors, post_sql_error};
use crate::handles::desc::{DescHandle, DescHeader, DescKind};
use crate::handles::stmt::{
    InertStmtAttrs, STMT_STATE_EXEC_STARTED, STMT_STATE_FETCH_IN_PROGRESS, StmtState,
    VendorStmtAttrs,
};
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// First-occurrence latches for the two cursor substitution reports below.
///
/// Each of those substitutions is an unconditional property of this driver
/// rather than a per-call event: a scrollable cursor *always* becomes
/// forward-only here, so once an operator has been told, the thousandth
/// identical line carries no further information. `warn` is the default trace
/// level and the file sink writes unbuffered while holding the serialization
/// lock, so an application that sets one of these per statement would
/// otherwise pay a formatted, locked write per statement to restate a fact it
/// already has. The first occurrence keeps `warn` so the condition stays
/// visible without raising the level; the rest drop to `debug`.
///
/// Per process rather than per handle: statements are commonly allocated one
/// per query, so a per-handle latch would still emit one line per statement
/// and would not bound anything. Suppression is only about the trace stream —
/// the caller still receives its `01S02` diagnostic on every call, which is
/// also why one latch per arm is enough here. The inert "stored without
/// effect" report posts no diagnostic at all, so it latches per attribute
/// instead; see [`InertStmtAttrs::claim_ignored_report`].
static CURSOR_TYPE_SUBSTITUTED: AtomicBool = AtomicBool::new(false);
static CONCURRENCY_SUBSTITUTED: AtomicBool = AtomicBool::new(false);

/// Returns whether this is the first time `latch` has been reached, taking it
/// if so.
fn first_occurrence(latch: &AtomicBool) -> bool {
    !latch.swap(true, Ordering::Relaxed)
}

/// Clamps a requested `SQL_ATTR_QUERY_TIMEOUT` to the largest value the driver
/// accepts, reporting whether the request had to be reduced.
///
/// msodbcsql caps at `MAX_QUERY_TIMEOUT` and posts `01S02` instead of rejecting
/// (`sqlcmisc.cpp:3988-3994`), so an over-large request still yields a usable
/// statement. Shared with the `SQLSetConnectAttrW` route, which applies the same
/// cap before fanning the value out.
pub(super) fn clamp_query_timeout(requested: SqlULen) -> (u32, bool) {
    let seconds = u32::try_from(requested).unwrap_or(u32::MAX);
    if seconds > MAX_QUERY_TIMEOUT {
        (MAX_QUERY_TIMEOUT, true)
    } else {
        (seconds, false)
    }
}

/// Sets a statement attribute.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. For the pointer
/// attributes the caller-supplied `value_ptr` must remain valid for the
/// lifetime it is used by later fetches or parameter-array executions.
pub(crate) unsafe fn sql_set_stmt_attr_w(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        attribute,
        ?value_ptr,
        string_length,
        "SQLSetStmtAttrW called",
    );
    crate::ffi_entry!("SQLSetStmtAttrW", unsafe {
        sql_set_stmt_attr_w_impl(statement_handle, attribute, value_ptr, string_length)
    })
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`. For the two
/// query-notification string attributes, `value_ptr` must be readable for
/// `string_length` bytes of UTF-16 or through a NUL terminator when the length is
/// `SQL_NTS`. Pointer-valued attributes must remain valid while associated with
/// the statement.
unsafe fn sql_set_stmt_attr_w_impl(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLSetStmtAttrW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLSetStmtAttrW: handle is not a STMT"
    );
    unsafe { sql_set_stmt_attr_w_safe(stmt, attribute, value_ptr, string_length) }
}

/// # Safety
/// For the two string-valued attributes (`SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT`
/// and `..._OPTIONS`) `value_ptr` must point to `string_length` readable bytes of
/// UTF-16, or to a NUL-terminated string when `string_length` is `SQL_NTS`. For
/// every other attribute `value_ptr` is an integer passed in the pointer slot and
/// is never dereferenced.
unsafe fn sql_set_stmt_attr_w_safe(
    stmt: &StmtHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length: SqlInteger,
) -> SqlReturn {
    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLSetStmtAttrW: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    match attribute {
        SQL_ATTR_PARAMSET_SIZE
        | SQL_ATTR_PARAM_BIND_OFFSET_PTR
        | SQL_ATTR_PARAM_BIND_TYPE
        | SQL_ATTR_PARAM_OPERATION_PTR
        | SQL_ATTR_PARAM_STATUS_PTR
        | SQL_ATTR_PARAMS_PROCESSED_PTR
            if state.has_state(STMT_STATE_EXEC_STARTED) =>
        {
            error!(
                attribute,
                "SQLSetStmtAttrW: parameter execution is in progress on this statement"
            );
            post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
            SQL_ERROR
        }
        // The rowset controls are read into a fetch's snapshot, so moving them
        // mid-fetch would point it at buffers of the wrong size or shape.
        // `SQL_ATTR_ROW_OPERATION_PTR` is unconsumed today but gated so both
        // spellings of the ARD's `SQL_DESC_ARRAY_STATUS_PTR` agree.
        // `SQL_ROWSET_SIZE` has no descriptor spelling; it is gated
        // pre-emptively, for consistency with the other rowset controls.
        SQL_ATTR_ROW_ARRAY_SIZE
        | SQL_ROWSET_SIZE
        | SQL_ATTR_ROWS_FETCHED_PTR
        | SQL_ATTR_ROW_STATUS_PTR
        | SQL_ATTR_ROW_BIND_OFFSET_PTR
        | SQL_ATTR_ROW_BIND_TYPE
        | SQL_ATTR_ROW_OPERATION_PTR
            if state.has_state(STMT_STATE_FETCH_IN_PROGRESS) =>
        {
            error!(
                attribute,
                "SQLSetStmtAttrW: a fetch is in progress on this statement"
            );
            post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
            SQL_ERROR
        }
        SQL_ATTR_ROW_ARRAY_SIZE => {
            // The value is a `SQLULEN` passed by value in the pointer slot. Zero
            // is an invalid rowset size (HY024) — reject rather than paper over.
            let n = value_ptr as SqlULen;
            if n == 0 {
                // Registry entry 25 (docs/parity-deviations.md): msodbcsql
                // accepts and stores 0 here; this driver rejects it.
                error!("SQLSetStmtAttrW: SQL_ATTR_ROW_ARRAY_SIZE of 0 is invalid");
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            }
            let ard = state.effective_ard(stmt);
            drop(state);
            let (rc, stored) = set_desc_header(
                stmt,
                ard,
                HeaderField::ArraySize,
                value_ptr,
                ArraySizeClamp::ToI32Max,
            );
            if rc != SQL_ERROR {
                debug!(
                    row_array_size = stored,
                    "SQLSetStmtAttrW: SQL_ATTR_ROW_ARRAY_SIZE set"
                );
            }
            rc
        }
        SQL_ATTR_PARAMSET_SIZE => {
            if value_ptr as SqlULen == 0 {
                error!("SQLSetStmtAttrW: SQL_ATTR_PARAMSET_SIZE of 0 is invalid");
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            }
            let apd = state.effective_apd(stmt);
            drop(state);
            set_desc_header(
                stmt,
                apd,
                HeaderField::ArraySize,
                value_ptr,
                ArraySizeClamp::None,
            )
            .0
        }
        SQL_ROWSET_SIZE => {
            // Not an alias of `SQL_DESC_ARRAY_SIZE`, but ARD-resident like
            // msodbcsql's `pARD->dwRowSetSize`. It shares the array-size
            // *validation* case, clamping to
            // `INT32_MAX` with `01S02` (`IsSetStmtOptionValid`). Deliberately
            // no zero check, unlike the two spellings above: msodbcsql accepts
            // 0 on all of them, and a conforming Driver Manager rejects a zero
            // `SQL_ROWSET_SIZE` before either driver is called. See registry
            // entry 25.
            let ard = state.effective_ard(stmt);
            drop(state);
            set_desc_header(
                stmt,
                ard,
                HeaderField::RowsetSize,
                value_ptr,
                ArraySizeClamp::ToI32Max,
            )
            .0
        }
        // The remaining header aliases: unvalidated in msodbcsql
        // (`IsSetStmtOptionValid`), stored verbatim on the owning descriptor.
        SQL_ATTR_ROW_BIND_TYPE
        | SQL_ATTR_ROW_BIND_OFFSET_PTR
        | SQL_ATTR_ROW_OPERATION_PTR
        | SQL_ATTR_ROW_STATUS_PTR
        | SQL_ATTR_ROWS_FETCHED_PTR
        | SQL_ATTR_PARAM_BIND_TYPE
        | SQL_ATTR_PARAM_BIND_OFFSET_PTR
        | SQL_ATTR_PARAM_OPERATION_PTR
        | SQL_ATTR_PARAM_STATUS_PTR
        | SQL_ATTR_PARAMS_PROCESSED_PTR => {
            let Some((owner, field)) = header_alias(attribute) else {
                // Unreachable: every identifier in this arm is in the table.
                post_diag(
                    &mut state,
                    unimplemented_attr_diag(AttrScope::Stmt, AttrOp::Set, attribute),
                );
                return SQL_ERROR;
            };
            let raw = owner.resolve(&state, stmt);
            drop(state);
            set_desc_header(stmt, raw, field, value_ptr, ArraySizeClamp::None).0
        }
        SQL_ATTR_CURSOR_TYPE => {
            // The driver is forward-only. Accept SQL_CURSOR_FORWARD_ONLY as-is;
            // for any other cursor type substitute forward-only and warn with
            // 01S02, per the ODBC contract for unsupported cursor types (a
            // silent success would tell the caller a scrollable cursor took
            // effect when it did not). The substituted value is what
            // SQLGetStmtAttrW reports back.
            if value_ptr as SqlULen == SQL_CURSOR_FORWARD_ONLY {
                SQL_SUCCESS
            } else {
                let requested = value_ptr as SqlULen;
                if first_occurrence(&CURSOR_TYPE_SUBSTITUTED) {
                    warn!(
                        requested,
                        "SQLSetStmtAttrW: cursor type substituted with SQL_CURSOR_FORWARD_ONLY"
                    );
                } else {
                    debug!(
                        requested,
                        "SQLSetStmtAttrW: cursor type substituted with SQL_CURSOR_FORWARD_ONLY"
                    );
                }
                post_sql_error(
                    &mut state,
                    SQLSTATE_01S02,
                    0,
                    "Cursor type not supported; substituted SQL_CURSOR_FORWARD_ONLY",
                );
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_CONCURRENCY => {
            // The driver is read-only. Accept SQL_CONCUR_READ_ONLY as-is;
            // substitute read-only and warn with 01S02 for any writable
            // concurrency request.
            if value_ptr as SqlULen == SQL_CONCUR_READ_ONLY {
                SQL_SUCCESS
            } else {
                let requested = value_ptr as SqlULen;
                if first_occurrence(&CONCURRENCY_SUBSTITUTED) {
                    warn!(
                        requested,
                        "SQLSetStmtAttrW: concurrency substituted with SQL_CONCUR_READ_ONLY"
                    );
                } else {
                    debug!(
                        requested,
                        "SQLSetStmtAttrW: concurrency substituted with SQL_CONCUR_READ_ONLY"
                    );
                }
                post_sql_error(
                    &mut state,
                    SQLSTATE_01S02,
                    0,
                    "Concurrency not supported; substituted SQL_CONCUR_READ_ONLY",
                );
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_QUERY_TIMEOUT => {
            let (seconds, clamped) = clamp_query_timeout(value_ptr as SqlULen);
            state.query_timeout = seconds;
            debug!(seconds, "SQLSetStmtAttrW: SQL_ATTR_QUERY_TIMEOUT set");
            if clamped {
                post_diag(&mut state, WARN_OPTION_VALUE_CHANGED);
                return SQL_SUCCESS_WITH_INFO;
            }
            SQL_SUCCESS
        }
        SQL_ATTR_MAX_ROWS => {
            // Enforced, not merely stored: `fetch_rows_next` stops the cursor
            // once this many rows have been returned from the current result
            // set, matching msodbcsql.
            state.max_rows = value_ptr as SqlULen;
            debug!(
                max_rows = state.max_rows,
                "SQLSetStmtAttrW: SQL_ATTR_MAX_ROWS set"
            );
            SQL_SUCCESS
        }
        SQL_ATTR_MAX_LENGTH => {
            // msodbcsql substitutes MSODBCSQL_MAX_LENGTH for any non-zero
            // request and reports 01S02; the cap is then never applied, and a
            // longer value still comes back whole. Mirror the reported value
            // and the warning so an application that inspects either sees the
            // same thing from both drivers.
            let requested = value_ptr as SqlULen;
            let effective = if requested == 0 {
                0
            } else {
                MSODBCSQL_MAX_LENGTH
            };
            state.inert_attrs.set(SQL_ATTR_MAX_LENGTH, effective);
            if requested == effective {
                SQL_SUCCESS
            } else {
                post_diag(&mut state, WARN_OPTION_VALUE_CHANGED);
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_KEYSET_SIZE => {
            // Keyset-driven cursors are not implemented, so any non-zero keyset
            // is refused the same way msodbcsql refuses it: the stored value
            // stays 0 and the caller is told with 01S02.
            if value_ptr as SqlULen == 0 {
                SQL_SUCCESS
            } else {
                post_diag(&mut state, WARN_OPTION_VALUE_CHANGED);
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_SIMULATE_CURSOR => {
            // msodbcsql reports SQL_SC_UNIQUE and accepts only that value.
            if value_ptr as SqlULen == SQL_SC_UNIQUE {
                SQL_SUCCESS
            } else {
                post_diag(&mut state, WARN_OPTION_VALUE_CHANGED);
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_CURSOR_SCROLLABLE => {
            // Scrollability is the cursor type seen from the other side:
            // msodbcsql moves SQL_ATTR_CURSOR_TYPE off forward-only when a
            // caller asks for a scrollable cursor. This driver has no scrollable
            // cursor, so a request is substituted and reported exactly as
            // SQL_ATTR_CURSOR_TYPE already does.
            if value_ptr as SqlULen == SQL_NONSCROLLABLE {
                SQL_SUCCESS
            } else {
                post_sql_error(
                    &mut state,
                    SQLSTATE_01S02,
                    0,
                    "Scrollable cursors not supported; substituted SQL_NONSCROLLABLE",
                );
                SQL_SUCCESS_WITH_INFO
            }
        }
        SQL_ATTR_CURSOR_SENSITIVITY => {
            // SQL_UNSPECIFIED (0) means "whatever the driver does", so the get
            // path must answer with the driver's actual sensitivity rather than
            // the request. msodbcsql resolves it to SQL_INSENSITIVE silently —
            // no 01S02 — and stores any other value verbatim.
            let requested = value_ptr as SqlULen;
            let effective = if requested == 0 {
                SQL_INSENSITIVE
            } else {
                requested
            };
            state
                .inert_attrs
                .set(SQL_ATTR_CURSOR_SENSITIVITY, effective);
            SQL_SUCCESS
        }
        SQL_ATTR_METADATA_ID => {
            let requested = value_ptr as SqlULen;
            if requested == SqlULen::from(SQL_FALSE) {
                state
                    .inert_attrs
                    .set(SQL_ATTR_METADATA_ID, SqlULen::from(SQL_FALSE));
                SQL_SUCCESS
            } else if requested == SqlULen::from(SQL_TRUE) {
                error!("SQLSetStmtAttrW: metadata identifier mode is not implemented");
                post_diag(&mut state, ERR_OPTIONAL_FEATURE_NOT_IMPLEMENTED);
                SQL_ERROR
            } else {
                error!(
                    value = requested,
                    "SQLSetStmtAttrW: invalid SQL_ATTR_METADATA_ID value"
                );
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                SQL_ERROR
            }
        }
        SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT | SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS => {
            // The only two string-valued statement attributes. `string_length`
            // is a byte count (or SQL_NTS), which is why the set entry point
            // has to forward it rather than treating the pointer slot as an
            // integer like every other attribute here.
            //
            // `SQL_NTS` is the only negative length ODBC defines for a
            // character attribute. msodbcsql answers HY024 for any other
            // negative value and leaves the stored string alone; reading it as
            // empty instead would silently clear the attribute.
            if string_length < 0 && string_length != SqlInteger::from(SQL_NTS) {
                error!(
                    attribute,
                    string_length, "SQLSetStmtAttrW: invalid string length"
                );
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                return SQL_ERROR;
            }
            // msodbcsql faults on null for these ids, so there is no result to
            // match. Clear the value rather than faulting the host process.
            let value = unsafe { read_utf16_attr(value_ptr as *const SqlWChar, string_length) };
            if attribute == SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT {
                state.qn_msgtext = value;
            } else {
                state.qn_options = value;
            }
            SQL_SUCCESS
        }
        // SQL Server vendor attributes. Unlike the inert set these validate
        // before storing: the Driver Manager has no knowledge of ids 1225-1238
        // and passes any value straight through, so rejecting an out-of-range
        // value is the driver's job. A rejected set leaves the previous value
        // in place.
        //
        // The get-only ids are deliberately excluded by `is_settable` so they
        // fall through to the identifier-rejection path below, which answers
        // the `HY092` msodbcsql gives them rather than `HY024`.
        attribute if VendorStmtAttrs::is_settable(attribute) => {
            if state.vendor_attrs.set(attribute, value_ptr as SqlULen) {
                debug!(attribute, "SQLSetStmtAttrW: vendor attribute stored");
                SQL_SUCCESS
            } else {
                error!(
                    attribute,
                    value = value_ptr as SqlULen,
                    "SQLSetStmtAttrW: value rejected for vendor attribute"
                );
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                SQL_ERROR
            }
        }
        SQL_ATTR_APP_ROW_DESC => match validate_descriptor_association(stmt, stmt.ard, value_ptr) {
            Ok(new_active) => {
                state.active_ard = new_active;
                debug!(?new_active, "SQLSetStmtAttrW: SQL_ATTR_APP_ROW_DESC set");
                SQL_SUCCESS
            }
            Err(diag) => {
                error!(attribute, "SQLSetStmtAttrW: SQL_ATTR_APP_ROW_DESC rejected");
                post_diag(&mut state, diag);
                SQL_ERROR
            }
        },
        SQL_ATTR_APP_PARAM_DESC => match validate_descriptor_association(stmt, stmt.apd, value_ptr)
        {
            Ok(new_active) => {
                state.active_apd = new_active;
                debug!(?new_active, "SQLSetStmtAttrW: SQL_ATTR_APP_PARAM_DESC set");
                SQL_SUCCESS
            }
            Err(diag) => {
                error!(
                    attribute,
                    "SQLSetStmtAttrW: SQL_ATTR_APP_PARAM_DESC rejected"
                );
                post_diag(&mut state, diag);
                SQL_ERROR
            }
        },
        _ => {
            // Recognized attributes stored and round-tripped without effect:
            // these parameter and cursor controls do not change the implemented
            // forward-only, read-only behavior, but msodbcsql reports back
            // whatever was written.
            if state.inert_attrs.contains(attribute) {
                state.inert_attrs.set(attribute, value_ptr as SqlULen);
                // Only the attributes nothing reads back are "without effect".
                // NOSCAN and the parameter-array attributes are stored here but
                // consumed at execute, so claiming they are ignored would be
                // false — and `warn` is visible at the default trace level.
                if InertStmtAttrs::is_honoured(attribute) {
                    debug!(attribute, "SQLSetStmtAttrW: attribute stored");
                } else if InertStmtAttrs::claim_ignored_report(attribute) {
                    warn!(
                        attribute,
                        "SQLSetStmtAttrW: attribute stored without effect"
                    );
                } else {
                    debug!(
                        attribute,
                        "SQLSetStmtAttrW: attribute stored without effect"
                    );
                }
                SQL_SUCCESS
            } else {
                post_diag(
                    &mut state,
                    unimplemented_attr_diag(AttrScope::Stmt, AttrOp::Set, attribute),
                );
                SQL_ERROR
            }
        }
    }
}

/// Whether an array-size spelling clamps its request, mirroring msodbcsql's
/// `IsSetStmtOptionValid` validator table (`odbc/sqlcmisc.cpp`).
///
/// That table is keyed on the *attribute*, not on the field it lands in, so two
/// spellings of one storage location can validate differently — and in
/// msodbcsql they do. `SQL_ATTR_ROW_ARRAY_SIZE` shares a `case` with
/// `SQL_ROWSET_SIZE` that clamps to `INT32_MAX` and reports `IDS_01_S02`, while
/// `SQL_ATTR_PARAMSET_SIZE` sits under `// Attributes with no validation` and
/// is stored verbatim by `SQLSetStmtAttr` (`pAPDActual->dwArraySize = vParam`).
/// Making the descriptor canonical (AB#48943) changed where the value is kept,
/// not which rule guards each entry point, so this reproduces the asymmetry
/// instead of imposing a uniform bound the reference driver does not have.
#[derive(Clone, Copy)]
enum ArraySizeClamp {
    /// Clamp to `i32::MAX` and report `01S02` — the `SQL_ATTR_ROW_ARRAY_SIZE`
    /// and `SQL_ROWSET_SIZE` rule.
    ToI32Max,
    /// Store the request verbatim — the `SQL_ATTR_PARAMSET_SIZE` rule.
    None,
}

/// The statement's descriptor a header alias lives on: the effective ARD/APD,
/// or the statement's own IRD/IPD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderOwner {
    Ard,
    Apd,
    Ird,
    Ipd,
}

impl HeaderOwner {
    /// Resolved under the caller's STMT lock, which must be dropped before the
    /// handle is dereferenced (§7.1).
    fn resolve(self, state: &StmtState, stmt: &StmtHandle) -> SqlHandle {
        match self {
            HeaderOwner::Ard => state.effective_ard(stmt),
            HeaderOwner::Apd => state.effective_apd(stmt),
            HeaderOwner::Ird => stmt.ird,
            HeaderOwner::Ipd => stmt.ipd,
        }
    }
}

/// A `DescHeader` field reachable through a statement attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderField {
    ArraySize,
    BindType,
    BindOffsetPtr,
    ArrayStatusPtr,
    RowsProcessedPtr,
    /// Not an ODBC header field — see [`DescHeader::rowset_size`].
    RowsetSize,
}

impl HeaderField {
    /// The value as `SQLGetStmtAttrW` reports it (all pointer-width).
    fn read(self, header: &DescHeader) -> SqlULen {
        match self {
            HeaderField::ArraySize => header.array_size,
            HeaderField::BindType => header.bind_type,
            HeaderField::BindOffsetPtr => header.bind_offset_ptr.addr(),
            HeaderField::ArrayStatusPtr => header.array_status_ptr.addr(),
            HeaderField::RowsProcessedPtr => header.rows_processed_ptr.addr(),
            HeaderField::RowsetSize => header.rowset_size,
        }
    }

    /// Pointer fields take `value_ptr` as is; size fields take `size`.
    fn write(self, header: &mut DescHeader, value_ptr: SqlPointer, size: SqlULen) {
        match self {
            HeaderField::ArraySize => header.array_size = size,
            HeaderField::BindType => header.bind_type = size,
            HeaderField::BindOffsetPtr => header.bind_offset_ptr = value_ptr,
            HeaderField::ArrayStatusPtr => header.array_status_ptr = value_ptr,
            HeaderField::RowsProcessedPtr => header.rows_processed_ptr = value_ptr,
            HeaderField::RowsetSize => header.rowset_size = size,
        }
    }
}

/// Statement attributes stored on a descriptor header: the alias table in
/// `handles::desc`, plus `SQL_ROWSET_SIZE`. Mirrors where msodbcsql's
/// `SQLSetStmtAttr`/`SQLGetStmtAttr` keep each one. msodbcsql routes
/// `PARAMSET_SIZE`/`PARAM_BIND_TYPE`/`PARAM_BIND_OFFSET_PTR` to the TVP child
/// APD under `SQL_SOPT_SS_PARAM_FOCUS` (`pAPDActual`) but
/// `PARAM_OPERATION_PTR` to the statement's APD; the same here today only
/// because param focus is rejected.
fn header_alias(attribute: SqlInteger) -> Option<(HeaderOwner, HeaderField)> {
    let alias = match attribute {
        SQL_ATTR_ROW_ARRAY_SIZE => (HeaderOwner::Ard, HeaderField::ArraySize),
        SQL_ATTR_PARAMSET_SIZE => (HeaderOwner::Apd, HeaderField::ArraySize),
        SQL_ATTR_ROW_BIND_TYPE => (HeaderOwner::Ard, HeaderField::BindType),
        SQL_ATTR_PARAM_BIND_TYPE => (HeaderOwner::Apd, HeaderField::BindType),
        SQL_ATTR_ROW_BIND_OFFSET_PTR => (HeaderOwner::Ard, HeaderField::BindOffsetPtr),
        SQL_ATTR_PARAM_BIND_OFFSET_PTR => (HeaderOwner::Apd, HeaderField::BindOffsetPtr),
        SQL_ATTR_ROW_OPERATION_PTR => (HeaderOwner::Ard, HeaderField::ArrayStatusPtr),
        SQL_ATTR_PARAM_OPERATION_PTR => (HeaderOwner::Apd, HeaderField::ArrayStatusPtr),
        SQL_ATTR_ROW_STATUS_PTR => (HeaderOwner::Ird, HeaderField::ArrayStatusPtr),
        SQL_ATTR_PARAM_STATUS_PTR => (HeaderOwner::Ipd, HeaderField::ArrayStatusPtr),
        SQL_ATTR_ROWS_FETCHED_PTR => (HeaderOwner::Ird, HeaderField::RowsProcessedPtr),
        SQL_ATTR_PARAMS_PROCESSED_PTR => (HeaderOwner::Ipd, HeaderField::RowsProcessedPtr),
        SQL_ROWSET_SIZE => (HeaderOwner::Ard, HeaderField::RowsetSize),
        _ => return None,
    };
    Some(alias)
}

/// Writes one descriptor-header alias on the descriptor that owns it, applying
/// the clamp rule that belongs to the attribute spelling the caller used.
///
/// `raw` is resolved under the STMT lock by the caller and that lock is dropped
/// before this runs, so the handle is re-checked for liveness here.
/// Returns the status and the size actually stored (size fields only), so the
/// caller can trace it without recomputing the clamp.
fn set_desc_header(
    stmt: &StmtHandle,
    raw: SqlHandle,
    field: HeaderField,
    value_ptr: SqlPointer,
    clamp: ArraySizeClamp,
) -> (SqlReturn, SqlULen) {
    let (stored, clamped) = match clamp {
        ArraySizeClamp::ToI32Max => clamp_array_size(value_ptr as SqlULen),
        ArraySizeClamp::None => (value_ptr as SqlULen, false),
    };
    if crate::handles::live_type(raw) != Some(HandleType::Desc) {
        return (
            desc_header_error(stmt, "descriptor was freed concurrently"),
            stored,
        );
    }
    let desc = unsafe { handle_from_raw::<DescHandle>(raw) };
    // `SQLSetDescFieldW` refuses this exact write while any statement on the
    // connection is fetching through this descriptor as its effective ARD or
    // executing through it as its effective APD
    // (`DescHandle::update_definition`). The two spellings are one value, so
    // refusing on only one would let a sibling statement's in-flight fetch have
    // its rowset extent, bind offset or binding shape changed underneath the
    // pointers it already snapshotted, or its execute have its parameter-set
    // layout changed underneath the status array it already sized. Checked
    // before the DESC lock, matching `update_definition`'s DBC -> STMT -> DESC
    // order; consulted rather than reused because its diagnostic lands on the
    // descriptor and the application called `SQLSetStmtAttrW`.
    //
    // Only `Ad` is passed: the calling statement is already refused by the
    // in-progress arms at the top of `sql_set_stmt_attr_w_safe`,
    // `validate_descriptor_association` rejects another statement's implicit
    // descriptor with `HY017`, and an IRD/IPD is never shared, so for every
    // other kind both answers are structurally `false`. Only an explicit
    // descriptor can have a sibling reader. The residual same-statement window
    // is the advisory one recorded in §7.2 of the ODBC instructions; closing it
    // needs the descriptor-side interlock, not a second check with the same
    // gap.
    let readers = desc.readers_through(
        matches!(desc.kind, DescKind::Ad),
        matches!(desc.kind, DescKind::Ad),
    );
    // One walk can only report one failure, so the two previously-separate
    // "fetch state" / "execute state" traces fold into one. The posted `HY000`
    // text was identical either way; only the `error!` loses that resolution.
    let (fetching, executing) = match readers {
        Ok(pair) => pair,
        Err(()) => {
            return (
                desc_header_error(stmt, "failed to check descriptor reader state"),
                stored,
            );
        }
    };
    if fetching || executing {
        if fetching {
            error!("SQLSetStmtAttrW: a fetch is in progress through this descriptor");
        } else {
            error!("SQLSetStmtAttrW: an execute is in progress through this descriptor");
        }
        let Ok(mut state) = stmt.inner.lock() else {
            error!("SQLSetStmtAttrW: stmt mutex poisoned refusing an in-flight descriptor write");
            return (SQL_ERROR, stored);
        };
        post_diag(&mut state, ERR_FUNCTION_SEQUENCE);
        return (SQL_ERROR, stored);
    }
    let Ok(mut desc_state) = desc.inner.lock() else {
        return (desc_header_error(stmt, "descriptor mutex poisoned"), stored);
    };
    field.write(&mut desc_state.header, value_ptr, stored);
    // Released before the STMT lock below: the crate's locking rule forbids
    // nesting these in either order (`free_desc` already walks DBC->STMT).
    drop(desc_state);
    if clamped {
        // Posted on the statement, not the descriptor: the application called
        // `SQLSetStmtAttrW`, so that is the handle it reads diagnostics from.
        // A poisoned lock here cannot be absorbed — returning
        // `SQL_SUCCESS_WITH_INFO` with no record to read would hand the caller
        // a warning it can never retrieve.
        let Ok(mut state) = stmt.inner.lock() else {
            error!("SQLSetStmtAttrW: stmt mutex poisoned posting the array-size clamp warning");
            return (SQL_ERROR, stored);
        };
        post_diag(&mut state, WARN_ARRAY_SIZE_CHANGED);
        return (SQL_SUCCESS_WITH_INFO, stored);
    }
    (SQL_SUCCESS, stored)
}

fn desc_header_error(stmt: &StmtHandle, message: &'static str) -> SqlReturn {
    error!("SQLSet/GetStmtAttrW: {message}");
    // Already returning `SQL_ERROR`, so a poisoned statement lock costs only
    // the diagnostic record, not the status the caller sees.
    if let Ok(mut state) = stmt.inner.lock() {
        post_sql_error(
            &mut state,
            SQLSTATE_HY000,
            0,
            "Internal error accessing statement descriptor",
        );
    }
    SQL_ERROR
}

/// Validates a new `SQL_ATTR_APP_ROW_DESC`/`SQL_ATTR_APP_PARAM_DESC` value and
/// returns the slot to store in `StmtState::active_ard`/`active_apd`:
/// `own_implicit` is the statement's own permanent implicit descriptor for
/// this role (`stmt.ard` or `stmt.apd`).
///
/// Mirrors msodbcsql's `SQLSetStmtAttr` ARD/APD handling
/// (`sqlcmisc.cpp:3599-3639`) and the ODBC reference's `SQL_ATTR_APP_ROW_DESC`/
/// `SQL_ATTR_APP_PARAM_DESC` entries:
/// - `value_ptr` null or equal to `own_implicit` (the handle originally
///   returned for this statement's ARD/APD) resets to implicit (`Ok(None)`).
/// - Otherwise `value_ptr` must be an explicitly-allocated descriptor
///   (`SQL_DESC_ALLOC_USER`) on the *same connection* as `stmt`, or the call
///   fails: `HY017` if it is some other implicit descriptor (another
///   statement's ARD/APD, or this statement's own IRD/IPD — implicitly
///   allocated descriptors can never be associated except as their own
///   statement's ARD/APD, which is the reset case above), `HY024` if it is
///   explicit but on a different connection.
fn validate_descriptor_association(
    stmt: &StmtHandle,
    own_implicit: SqlHandle,
    value_ptr: SqlPointer,
) -> Result<Option<SqlHandle>, DiagMsg> {
    let value = value_ptr as SqlHandle;
    if value.is_null() || value == own_implicit {
        return Ok(None);
    }

    // SAFETY: trusts the Driver Manager to pass a live descriptor handle, per
    // this crate's FFI-boundary convention (see module docs / README.md).
    let target = unsafe { handle_from_raw::<DescHandle>(value) };
    debug_assert_eq!(
        target.object_type,
        HandleType::Desc,
        "SQLSetStmtAttrW: SQL_ATTR_APP_ROW_DESC/APP_PARAM_DESC value is not a DESC handle"
    );

    if !target.is_explicit() {
        return Err(ERR_INVALID_USE_OF_AUTO_DESC);
    }
    if target.parent_dbc != stmt.parent_dbc {
        return Err(ERR_INVALID_ATTRIBUTE_VALUE);
    }
    Ok(Some(value))
}

/// Retrieves a statement attribute.
///
/// # Safety
/// `statement_handle` must be a valid `StmtHandle` or null. `value_ptr`, when
/// non-null, must be writable for the size of the attribute (pointer-sized for
/// every attribute handled here).
pub(crate) unsafe fn sql_get_stmt_attr_w(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    debug!(
        ?statement_handle,
        attribute,
        ?value_ptr,
        buffer_length,
        ?string_length_ptr,
        "SQLGetStmtAttrW called",
    );
    crate::ffi_entry!("SQLGetStmtAttrW", unsafe {
        sql_get_stmt_attr_w_impl(
            statement_handle,
            attribute,
            value_ptr,
            buffer_length,
            string_length_ptr,
        )
    })
}

/// # Safety
/// `statement_handle` must be null or point to a live `StmtHandle`. `value_ptr`,
/// when non-null, must be writable for `buffer_length` bytes for string-valued
/// attributes or for one pointer-sized value otherwise. `string_length_ptr`,
/// when non-null, must be writable for one `SqlInteger`.
unsafe fn sql_get_stmt_attr_w_impl(
    statement_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLGetStmtAttrW: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLGetStmtAttrW: handle is not a STMT"
    );
    unsafe {
        sql_get_stmt_attr_w_safe(stmt, attribute, value_ptr, buffer_length, string_length_ptr)
    }
}

/// # Safety
/// `value_ptr`, when non-null, must be writable for `buffer_length` bytes for
/// the two string-valued attributes, and for one pointer-sized value otherwise.
/// `string_length_ptr`, when non-null, must be writable for one `SQLINTEGER`.
unsafe fn sql_get_stmt_attr_w_safe(
    stmt: &StmtHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    let Ok(mut state) = stmt.inner.lock() else {
        error!("SQLGetStmtAttrW: stmt mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    // Header aliases are read from the owning descriptor, after dropping this
    // lock.
    if let Some((owner, field)) = header_alias(attribute) {
        let raw = owner.resolve(&state, stmt);
        drop(state);
        return unsafe { get_desc_header(stmt, raw, field, value_ptr, string_length_ptr) };
    }

    // Every attribute reported here is a pointer-sized integer or pointer,
    // except the two query-notification strings, which return directly because
    // they own `buffer_length` / `string_length_ptr` themselves.
    // `write_if_some` is a no-op when `value_ptr` is null.
    match attribute {
        SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT | SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS => {
            let value = if attribute == SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT {
                state.qn_msgtext.clone()
            } else {
                state.qn_options.clone()
            };
            return unsafe {
                write_wide_attr(
                    &mut *state,
                    value_ptr as *mut SqlWChar,
                    buffer_length,
                    string_length_ptr,
                    &value,
                )
            };
        }
        SQL_ATTR_QUERY_TIMEOUT => unsafe {
            write_if_some(value_ptr as *mut SqlULen, state.query_timeout as SqlULen);
        },
        SQL_ATTR_MAX_ROWS => unsafe {
            write_if_some(value_ptr as *mut SqlULen, state.max_rows);
        },
        SQL_ATTR_ROW_NUMBER => {
            // Get-only, and only answerable while the cursor sits on a row:
            // msodbcsql returns 24000 on a statement with no cursor, on an
            // executed-but-not-yet-fetched cursor, and again once the rowset is
            // exhausted or closed. Positioned, it answers 0 rather than an
            // ordinal, because a forward-only cursor keeps no rowset origin to
            // number rows against.
            if !state.row_positioned {
                post_diag(&mut state, ERR_INVALID_CURSOR_STATE);
                return SQL_ERROR;
            }
            unsafe {
                write_if_some(value_ptr as *mut SqlULen, 0);
            }
        }
        SQL_ATTR_CURSOR_SCROLLABLE => unsafe {
            // Derived from the cursor type rather than stored, so the pair can
            // never disagree.
            write_if_some(value_ptr as *mut SqlULen, SQL_NONSCROLLABLE);
        },
        // Recognized attributes we don't store: report their effective ODBC
        // defaults for this forward-only, read-only, single-paramset driver.
        SQL_ATTR_CURSOR_TYPE => unsafe {
            write_if_some(value_ptr as *mut SqlULen, SQL_CURSOR_FORWARD_ONLY);
        },
        SQL_ATTR_CONCURRENCY => unsafe {
            write_if_some(value_ptr as *mut SqlULen, SQL_CONCUR_READ_ONLY);
        },
        // ARD/APD report the active association (an explicit descriptor if
        // one was set via SQLSetStmtAttrW, else the implicit default), so
        // they need `state` — unlike IRD/IPD below, which are never
        // swappable and live only on `StmtHandle` itself (set once in
        // `new()`, never reassigned — see that field's doc comment).
        SQL_ATTR_APP_ROW_DESC => unsafe {
            write_if_some(
                value_ptr as *mut SqlHandle,
                state.active_ard.unwrap_or(stmt.ard),
            );
        },
        SQL_ATTR_APP_PARAM_DESC => unsafe {
            write_if_some(
                value_ptr as *mut SqlHandle,
                state.active_apd.unwrap_or(stmt.apd),
            );
        },
        SQL_ATTR_IMP_ROW_DESC => unsafe {
            write_if_some(value_ptr as *mut SqlHandle, stmt.ird);
        },
        SQL_ATTR_IMP_PARAM_DESC => unsafe {
            write_if_some(value_ptr as *mut SqlHandle, stmt.ipd);
        },
        SQL_SOPT_SS_CURRENT_COMMAND => unsafe {
            // Get-only: the ordinal of the command being processed within the
            // current batch, maintained by the result-set advance hooks.
            write_if_some(value_ptr as *mut SqlULen, state.current_command);
        },
        // Attributes the set path stores without effect share the table that
        // holds their measured defaults, so a get before any set answers what
        // msodbcsql answers. Anything not in it is genuinely unhandled.
        _ => match state
            .inert_attrs
            .get(attribute)
            .or_else(|| state.vendor_attrs.get(attribute))
        {
            Some(value) => unsafe {
                write_if_some(value_ptr as *mut SqlULen, value);
            },
            None => {
                post_diag(
                    &mut state,
                    unimplemented_attr_diag(AttrScope::Stmt, AttrOp::Get, attribute),
                );
                return SQL_ERROR;
            }
        },
    }

    // Integer gets report the value's width. msodbcsql writes this on every
    // successful integer `SQLGetStmtAttrW`, so leaving the pointer untouched
    // hands the caller whatever was already in that memory. Every failure path
    // above returns early, and msodbcsql likewise leaves the pointer alone on
    // error, so writing once here is correct for both outcomes.
    unsafe {
        write_if_some(
            string_length_ptr,
            SqlInteger::try_from(size_of::<SqlULen>()).unwrap_or(SqlInteger::MAX),
        );
    }

    SQL_SUCCESS
}

/// Reports a descriptor-header alias as the value of its statement attribute.
///
/// Returns early from the common attribute path, so it owns the value-width
/// write that path performs at its tail.
///
/// # Safety
/// `value_ptr`, when non-null, must be writable for one `SqlULen`, and
/// `string_length_ptr`, when non-null, for one `SqlInteger` — the same
/// contract `SQLGetStmtAttrW` places on every integer-valued attribute.
/// `raw` must be null or a descriptor handle resolved from `stmt`; it is
/// re-checked for liveness before being dereferenced.
unsafe fn get_desc_header(
    stmt: &StmtHandle,
    raw: SqlHandle,
    field: HeaderField,
    value_ptr: SqlPointer,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    if crate::handles::live_type(raw) != Some(HandleType::Desc) {
        return desc_header_error(stmt, "descriptor was freed concurrently");
    }
    let desc = unsafe { handle_from_raw::<DescHandle>(raw) };
    // Copied out before the writes below: `write_if_some` dereferences
    // application-supplied pointers, which can fault a page in, and holding
    // the DESC guard across that would block every sibling statement sharing
    // an explicit descriptor for no benefit. Matches `set_desc_header`, which
    // drops its guard before touching anything else.
    let value = {
        let Ok(desc_state) = desc.inner.lock() else {
            return desc_header_error(stmt, "descriptor mutex poisoned");
        };
        field.read(&desc_state.header)
    };
    unsafe {
        write_if_some(value_ptr as *mut SqlULen, value);
        write_if_some(
            string_length_ptr,
            SqlInteger::try_from(size_of::<SqlULen>()).unwrap_or(SqlInteger::MAX),
        );
    }
    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::get_desc_field::sql_get_desc_field_w;
    use crate::api::odbc_types::{
        SQL_ATTR_ASYNC_ENABLE, SQL_ATTR_ENABLE_AUTO_IPD, SQL_ATTR_FETCH_BOOKMARK_PTR,
        SQL_ATTR_METADATA_ID, SQL_ATTR_NOSCAN, SQL_ATTR_RETRIEVE_DATA, SQL_ATTR_USE_BOOKMARKS,
        SQL_BIND_BY_COLUMN, SQL_DESC_ARRAY_SIZE, SQL_DESC_ARRAY_STATUS_PTR,
        SQL_DESC_BIND_OFFSET_PTR, SQL_DESC_BIND_TYPE, SQL_DESC_ROWS_PROCESSED_PTR, SQL_NTS,
        SQL_NULL_HANDLE, SQL_RD_ON, SQL_SOPT_SS_COLUMN_ENCRYPTION, SQL_SOPT_SS_CURSOR_OPTIONS,
        SQL_SOPT_SS_DEFER_PREPARE, SQL_SOPT_SS_HIDDEN_COLUMNS, SQL_SOPT_SS_NAME_SCOPE,
        SQL_SOPT_SS_NOBROWSETABLE, SQL_SOPT_SS_NOCOUNT_STATUS, SQL_SOPT_SS_PARAM_FOCUS,
        SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT, SQL_SOPT_SS_REGIONALIZE,
        SQL_SOPT_SS_TEXTPTR_LOGGING, SqlSmallInt, SqlUSmallInt,
    };
    use crate::api::set_desc_field::sql_set_desc_field_w;
    use crate::api::sqlstate::{
        SQLSTATE_01004, SQLSTATE_24000, SQLSTATE_HY024, SQLSTATE_HY092, SQLSTATE_HYC00,
    };
    use crate::handles::handle_from_raw;
    use crate::handles::stmt::InertStmtAttrs;
    use crate::test_support::TestHandles;

    fn header_of(desc: SqlHandle) -> DescHeader {
        let desc = unsafe { handle_from_raw::<DescHandle>(desc) };
        desc.inner.lock().unwrap().header
    }

    #[test]
    fn set_stmt_attr_null_handle() {
        let ret = unsafe {
            sql_set_stmt_attr_w(
                SQL_NULL_HANDLE,
                SQL_ATTR_ROW_ARRAY_SIZE,
                10 as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    /// Reads the SQLSTATE of the newest diagnostic on a statement. Must be
    /// called before any `sql_get_stmt_attr_w` helper, which frees diagnostics
    /// on entry.
    fn stmt_sql_state(stmt: SqlHandle) -> [u8; 5] {
        let stmt = unsafe { handle_from_raw::<StmtHandle>(stmt) };
        let state = stmt.inner.lock().unwrap();
        state.diag_records[0].sql_state
    }

    /// Reads `SQL_ATTR_QUERY_TIMEOUT` back off a statement.
    fn stmt_query_timeout(stmt: SqlHandle) -> SqlULen {
        let mut out: SqlULen = 0;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                stmt,
                SQL_ATTR_QUERY_TIMEOUT,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        out
    }

    #[test]
    fn query_timeout_defaults_to_no_limit() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(stmt_query_timeout(h.stmt), 0);
    }

    #[test]
    fn query_timeout_round_trips() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_QUERY_TIMEOUT, 30 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(stmt_query_timeout(h.stmt), 30);
    }

    #[test]
    fn query_timeout_at_the_cap_is_accepted_silently() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_ATTR_QUERY_TIMEOUT,
                MAX_QUERY_TIMEOUT as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(stmt_query_timeout(h.stmt), MAX_QUERY_TIMEOUT as SqlULen);
    }

    #[test]
    fn query_timeout_past_the_cap_is_clamped_with_a_warning() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(h.stmt, SQL_ATTR_QUERY_TIMEOUT, 0x10000 as SqlPointer, 0)
        };
        // Clamped rather than rejected, so the statement stays usable.
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            let state = stmt.inner.lock().unwrap();
            assert_eq!(state.diag_records[0].sql_state, SQLSTATE_01S02);
        }
        assert_eq!(stmt_query_timeout(h.stmt), MAX_QUERY_TIMEOUT as SqlULen);
    }

    #[test]
    fn clamp_reports_whether_it_reduced_the_request() {
        assert_eq!(clamp_query_timeout(0), (0, false));
        assert_eq!(clamp_query_timeout(30), (30, false));
        assert_eq!(
            clamp_query_timeout(MAX_QUERY_TIMEOUT as SqlULen),
            (MAX_QUERY_TIMEOUT, false)
        );
        assert_eq!(
            clamp_query_timeout(MAX_QUERY_TIMEOUT as SqlULen + 1),
            (MAX_QUERY_TIMEOUT, true)
        );
        // A value past `u32` must clamp, not wrap to a small number (or to 0,
        // which would silently mean "no timeout").
        assert_eq!(clamp_query_timeout(SqlULen::MAX), (MAX_QUERY_TIMEOUT, true));
    }

    #[test]
    fn set_row_array_size_stored_and_readback() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 128 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);

        let mut out: SqlULen = 0;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_ARRAY_SIZE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 128);
    }

    #[test]
    fn set_row_array_size_zero_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        // The previous (default) value must be left untouched.
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), 1);
    }

    #[test]
    fn set_rows_fetched_ptr_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut rows_fetched: SqlULen = 0;
        let ptr = &mut rows_fetched as *mut SqlULen;
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROWS_FETCHED_PTR, ptr.cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(header_of(h.ird()).rows_processed_ptr, ptr.cast());
    }

    /// Previously accepted as a no-op, which silently misplaced every bound
    /// column once a nonzero offset was in play.
    #[test]
    fn set_row_bind_offset_ptr_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut offset: SqlULen = 64;
        let ptr: *mut SqlULen = &mut offset;
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_BIND_OFFSET_PTR, ptr.cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(header_of(h.ard()).bind_offset_ptr, ptr.cast());

        // An attribute that can be set has to be readable back: reading the
        // stored field alone would not have caught a missing getter arm.
        let mut read_back: *mut SqlULen = std::ptr::null_mut();
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_BIND_OFFSET_PTR,
                (&mut read_back as *mut *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(read_back, ptr);
    }

    #[test]
    fn set_row_status_ptr_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut status: SqlUSmallInt = 0;
        let ptr = &mut status as *mut SqlUSmallInt;
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_STATUS_PTR, ptr.cast(), 0) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(header_of(h.ird()).array_status_ptr, ptr.cast());
    }

    /// The rowset pointers are the only statement attributes whose value *is* a
    /// pointer, so the get path has to write a pointer-width slot rather than
    /// the `SqlULen` every other attribute uses. Round-tripped through the
    /// public entry points because an application that sets a rowset pointer
    /// and reads it back must get the same address, not a truncated one.
    #[test]
    fn rowset_pointers_round_trip_through_the_get_path() {
        let h = TestHandles::with_env_dbc_stmt();

        let mut rows_fetched: SqlULen = 0;
        let fetched_ptr = &mut rows_fetched as *mut SqlULen;
        let mut status: SqlUSmallInt = 0;
        let status_ptr = &mut status as *mut SqlUSmallInt;

        unsafe {
            assert_eq!(
                sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROWS_FETCHED_PTR, fetched_ptr.cast(), 0),
                SQL_SUCCESS
            );
            assert_eq!(
                sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_STATUS_PTR, status_ptr.cast(), 0),
                SQL_SUCCESS
            );

            let mut out_fetched: *mut SqlULen = std::ptr::null_mut();
            assert_eq!(
                sql_get_stmt_attr_w(
                    h.stmt,
                    SQL_ATTR_ROWS_FETCHED_PTR,
                    (&raw mut out_fetched).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SQL_SUCCESS
            );
            assert_eq!(out_fetched, fetched_ptr);

            let mut out_status: *mut SqlUSmallInt = std::ptr::null_mut();
            assert_eq!(
                sql_get_stmt_attr_w(
                    h.stmt,
                    SQL_ATTR_ROW_STATUS_PTR,
                    (&raw mut out_status).cast(),
                    0,
                    std::ptr::null_mut(),
                ),
                SQL_SUCCESS
            );
            assert_eq!(out_status, status_ptr);
        }
    }

    #[test]
    fn set_row_bind_type_stored_and_readback() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_ROW_BIND_TYPE, 40 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        let mut out: SqlULen = 0;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_BIND_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 40);
    }

    #[test]
    fn default_row_bind_type_is_column_wise() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_BIND_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_BIND_BY_COLUMN);
    }

    #[test]
    fn set_unknown_attribute_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, 9999, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY092);
    }

    /// An identifier msodbcsql implements but this driver does not must report
    /// `HYC00`, not `HY092`: a caller probing for the feature has to be able to
    /// tell "unavailable" from "not an attribute".
    ///
    /// `SQL_ATTR_ASYNC_STMT_EVENT` (29) is the last statement attribute
    /// msodbcsql accepts that this driver does not implement, so it is the only
    /// remaining witness for this class. If asynchronous execution is ever
    /// implemented, this test needs a new subject — or deleting, if by then
    /// nothing is left in that class.
    #[test]
    fn set_attribute_known_to_msodbcsql_reports_not_implemented() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, 29, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HYC00);
    }

    /// Every statement attribute msodbcsql answers on a get, this driver also
    /// answers. Asserted over the recognition table rather than a hand-picked
    /// identifier so that adding a row without implementing it fails here
    /// instead of reaching an application as `HYC00`.
    ///
    /// This replaced a test that used `SQL_SOPT_SS_DEFER_PREPARE` as its
    /// unimplemented example; no gettable statement attribute is left in that
    /// class, which is the point.
    #[test]
    fn every_gettable_statement_attribute_is_implemented() {
        for id in crate::api::attributes::stmt_attr_ids(AttrOp::Get) {
            let h = TestHandles::with_env_dbc_stmt();
            // SQL_ATTR_ROW_NUMBER is the one legitimate error: it is answerable
            // only while the cursor sits on a row, and reports 24000 otherwise.
            if id == SQL_ATTR_ROW_NUMBER {
                continue;
            }
            let mut out: SqlULen = 0;
            let mut written: SqlInteger = 0;
            let ret = unsafe {
                sql_get_stmt_attr_w(
                    h.stmt,
                    id,
                    (&mut out as *mut SqlULen).cast(),
                    size_of::<SqlULen>() as SqlInteger,
                    &mut written,
                )
            };
            assert_ne!(
                ret, SQL_ERROR,
                "attribute {id} is recognized by msodbcsql but not served by this driver",
            );
        }
    }

    /// A connection attribute aimed at a statement stays `HY092`; the tables are
    /// scope-keyed precisely so this does not soften to `HYC00`.
    /// `SQL_ATTR_CURRENT_CATALOG` (109) is connection-only.
    #[test]
    fn connection_attribute_on_a_statement_stays_invalid() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, 109, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY092);
    }

    #[test]
    fn set_recognized_untracked_attribute_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CONCURRENCY,
                SQL_CONCUR_READ_ONLY as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_cursor_type_forward_only_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CURSOR_TYPE,
                SQL_CURSOR_FORWARD_ONLY as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_cursor_type_unsupported_substituted() {
        let h = TestHandles::with_env_dbc_stmt();
        // Any non-forward-only cursor (e.g. SQL_CURSOR_STATIC = 3) is
        // substituted with forward-only and reported via 01S02.
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_CURSOR_TYPE, 3 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);

        // The getter still reports the supported forward-only value.
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CURSOR_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_CURSOR_FORWARD_ONLY);
    }

    #[test]
    fn set_concurrency_unsupported_substituted() {
        let h = TestHandles::with_env_dbc_stmt();
        // Any writable concurrency (e.g. SQL_CONCUR_LOCK = 2) is substituted
        // with read-only and reported via 01S02.
        let ret = unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_CONCURRENCY, 2 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS_WITH_INFO);

        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CONCURRENCY,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_CONCUR_READ_ONLY);
    }

    #[test]
    fn set_paramset_size_one_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_PARAMSET_SIZE, 1 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn set_paramset_size_greater_than_one_is_stored() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_PARAMSET_SIZE, 100 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE), 100);
    }

    #[test]
    fn set_paramset_size_zero_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_ATTR_PARAMSET_SIZE, 0 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn get_stmt_attr_null_handle() {
        let mut out: SqlULen = 0;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                SQL_NULL_HANDLE,
                SQL_ATTR_ROW_ARRAY_SIZE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn get_unknown_attribute_rejected() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 7;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                9999,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        // Output must be left untouched on an invalid identifier.
        assert_eq!(out, 7);
    }

    #[test]
    fn get_concurrency_default_is_read_only() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CONCURRENCY,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_CONCUR_READ_ONLY);
    }

    #[test]
    fn get_cursor_type_default_is_forward_only() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_CURSOR_TYPE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, SQL_CURSOR_FORWARD_ONLY);
    }

    #[test]
    fn get_paramset_size_default_is_one() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 999;
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_PARAMSET_SIZE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        assert_eq!(out, 1);
    }

    fn desc_array_size(desc: SqlHandle) -> SqlULen {
        let mut out: SqlULen = 0;
        assert_eq!(
            unsafe {
                sql_get_desc_field_w(
                    desc,
                    0,
                    SQL_DESC_ARRAY_SIZE as SqlSmallInt,
                    (&mut out as *mut SqlULen).cast(),
                    0,
                    std::ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        out
    }

    fn set_desc_array_size_field(desc: SqlHandle, value: SqlULen) -> SqlReturn {
        unsafe {
            sql_set_desc_field_w(
                desc,
                0,
                SQL_DESC_ARRAY_SIZE as SqlSmallInt,
                value as SqlPointer,
                0,
            )
        }
    }

    #[test]
    fn array_size_statement_attributes_write_descriptor_headers() {
        let h = TestHandles::with_env_dbc_stmt();
        for (attribute, desc, value) in [
            (SQL_ATTR_PARAMSET_SIZE, h.apd(), 7),
            (SQL_ATTR_ROW_ARRAY_SIZE, h.ard(), 9),
        ] {
            assert_eq!(set_attr(h.stmt, attribute, value), SQL_SUCCESS);
            assert_eq!(desc_array_size(desc), value);
        }
    }

    #[test]
    fn array_size_descriptor_fields_are_visible_as_statement_attributes() {
        let h = TestHandles::with_env_dbc_stmt();
        for (desc, attribute, value) in [
            (h.apd(), SQL_ATTR_PARAMSET_SIZE, 11),
            (h.ard(), SQL_ATTR_ROW_ARRAY_SIZE, 13),
        ] {
            assert_eq!(set_desc_array_size_field(desc, value), SQL_SUCCESS);
            assert_eq!(get_attr(h.stmt, attribute), value);
        }
    }

    /// msodbcsql validates per *attribute*, not per field: `IsSetStmtOptionValid`
    /// clamps `SQL_ATTR_ROW_ARRAY_SIZE` to `INT32_MAX` with `01S02`, but lists
    /// `SQL_ATTR_PARAMSET_SIZE` under `// Attributes with no validation` and
    /// stores it verbatim. Making the descriptor canonical must not quietly
    /// impose the row-side bound on the parameter side.
    #[test]
    fn row_array_size_clamps_but_paramset_size_stores_verbatim() {
        let max = i32::MAX as SqlULen;
        let over = max + 1000;

        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, over),
            SQL_SUCCESS_WITH_INFO
        );
        // Read before any `sql_get_stmt_attr_w`, which frees diagnostics.
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(desc_array_size(h.ard()), max);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), max);

        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, over), SQL_SUCCESS);
        assert_eq!(
            desc_array_size(h.apd()),
            over,
            "PARAMSET_SIZE is stored verbatim, matching msodbcsql"
        );
        assert_eq!(get_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE), over);
    }

    /// The descriptor spelling clamps on *either* descriptor, independently of
    /// which attribute aliases it. `SQLSetDescField` is the one route msodbcsql
    /// bounds unconditionally, so an over-large APD write still clamps even
    /// though `SQL_ATTR_PARAMSET_SIZE` does not.
    #[test]
    fn descriptor_spelling_clamps_on_both_descriptors() {
        let max = i32::MAX as SqlULen;
        let h = TestHandles::with_env_dbc_stmt();
        for desc in [h.apd(), h.ard()] {
            assert_eq!(
                set_desc_array_size_field(desc, max + 1000),
                SQL_SUCCESS_WITH_INFO
            );
            assert_eq!(desc_array_size(desc), max);
        }
    }

    /// Exactly at the bound is not a clamp, so it must stay `SQL_SUCCESS` with
    /// no diagnostic — the off-by-one neighbour of the cases above.
    #[test]
    fn array_size_at_the_clamp_boundary_is_plain_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let max = i32::MAX as SqlULen;
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, max), SQL_SUCCESS);
        assert_eq!(desc_array_size(h.ard()), max);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, max), SQL_SUCCESS);
        assert_eq!(desc_array_size(h.apd()), max);
    }

    /// Registry entry 25 covers three spellings, not four: `SQL_ROWSET_SIZE`
    /// accepts 0 and stores it, matching msodbcsql. Rejecting it would widen
    /// the divergence for no observable gain — a conforming Driver Manager
    /// rejects a zero rowset size before either driver is called (measured,
    /// build 180735). Pinned so the entry and the code cannot drift apart.
    #[test]
    fn rowset_size_accepts_zero_like_msodbcsql() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ROWSET_SIZE, 7), SQL_SUCCESS);
        assert_eq!(set_attr(h.stmt, SQL_ROWSET_SIZE, 0), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ROWSET_SIZE), 0);

        // The two consumed spellings still reject it.
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 0), SQL_ERROR);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 0), SQL_ERROR);
    }

    /// Every rowset control is refused mid-fetch, because a fetch snapshots
    /// them and moving one would point it at buffers of the wrong size or
    /// shape. `SQL_ROWSET_SIZE` is in that list for consistency rather than
    /// necessity — nothing reads it today. Pinned so the guard is not narrowed
    /// back without the omission being noticed.
    #[test]
    fn rowset_controls_are_refused_during_a_fetch() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_FETCH_IN_PROGRESS);

        for attribute in [
            SQL_ATTR_ROW_ARRAY_SIZE,
            SQL_ROWSET_SIZE,
            SQL_ATTR_ROW_BIND_TYPE,
        ] {
            assert_eq!(
                set_attr(h.stmt, attribute, 4),
                SQL_ERROR,
                "attribute {attribute} must be refused mid-fetch"
            );
            assert_eq!(stmt_sql_state(h.stmt), ERR_FUNCTION_SEQUENCE.state);
        }

        stmt.inner
            .lock()
            .unwrap()
            .clear_state(STMT_STATE_FETCH_IN_PROGRESS);
        assert_eq!(set_attr(h.stmt, SQL_ROWSET_SIZE, 4), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ROWSET_SIZE), 4);
    }

    /// `SQL_ROWSET_SIZE` shares msodbcsql's array-size validation case with
    /// `SQL_ATTR_ROW_ARRAY_SIZE`, so it clamps the same way, without touching
    /// `SQL_DESC_ARRAY_SIZE`.
    #[test]
    fn rowset_size_clamps_without_touching_the_array_size() {
        let h = TestHandles::with_env_dbc_stmt();
        let max = i32::MAX as SqlULen;
        assert_eq!(
            set_attr(h.stmt, SQL_ROWSET_SIZE, max + 1000),
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(get_attr(h.stmt, SQL_ROWSET_SIZE), max);
        assert_eq!(
            desc_array_size(h.ard()),
            1,
            "SQL_ROWSET_SIZE is not an alias of the ARD header field"
        );
        assert_eq!(set_attr(h.stmt, SQL_ROWSET_SIZE, 7), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ROWSET_SIZE), 7);
    }

    /// The parameter-side half of the fetch gate above: `stage_execution`
    /// snapshots the parameter-set size and then walks the caller's
    /// `SQL_ATTR_PARAM_STATUS_PTR` array to that count, so both spellings of
    /// the value must refuse a mid-execute write or the array is walked past
    /// its extent.
    #[test]
    fn descriptor_array_size_is_refused_while_the_owning_statement_executes() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_EXEC_STARTED);

        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 9), SQL_ERROR);
        assert_eq!(
            set_desc_array_size_field(h.apd(), 9),
            SQL_ERROR,
            "the descriptor spelling must refuse it too"
        );
        assert_eq!(
            desc_array_size(h.apd()),
            1,
            "the size the execute snapshotted must not move"
        );

        stmt.inner
            .lock()
            .unwrap()
            .clear_state(STMT_STATE_EXEC_STARTED);
        assert_eq!(set_desc_array_size_field(h.apd(), 9), SQL_SUCCESS);
    }

    /// The cross-statement half: an explicit APD shared by two statements is
    /// `DescKind::Ad`, and the sibling's own `EXEC_STARTED` is clear, so
    /// nothing in the calling statement's state refuses this. Only a walk of
    /// every statement on the connection can see it.
    #[test]
    fn array_size_attribute_is_refused_while_a_sibling_executes_through_the_apd() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let shared = h.alloc_explicit_desc();
        let sibling = h.alloc_extra_stmt();
        for stmt in [h.stmt, sibling] {
            assert_eq!(set_desc(stmt, SQL_ATTR_APP_PARAM_DESC, shared), SQL_SUCCESS);
        }
        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 2), SQL_SUCCESS);

        let sibling_handle = unsafe { handle_from_raw::<StmtHandle>(sibling) };
        sibling_handle
            .inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_EXEC_STARTED);

        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 1000), SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), ERR_FUNCTION_SEQUENCE.state);
        assert_eq!(
            set_desc_array_size_field(shared, 1000),
            SQL_ERROR,
            "the descriptor spelling must refuse it on the sibling's behalf too"
        );
        assert_eq!(
            desc_array_size(shared),
            2,
            "the sibling's in-flight parameter-set size must not move"
        );

        sibling_handle
            .inner
            .lock()
            .unwrap()
            .clear_state(STMT_STATE_EXEC_STARTED);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 1000), SQL_SUCCESS);
        assert_eq!(h.free_extra_stmt(sibling), SQL_SUCCESS);
    }

    /// A rejected set must not disturb the canonical storage — otherwise a
    /// failed call would silently resize an application's array.
    #[test]
    fn array_size_zero_leaves_the_descriptor_untouched() {
        for (attribute, use_apd) in [
            (SQL_ATTR_PARAMSET_SIZE, true),
            (SQL_ATTR_ROW_ARRAY_SIZE, false),
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            let desc = if use_apd { h.apd() } else { h.ard() };

            assert_eq!(set_attr(h.stmt, attribute, 5), SQL_SUCCESS);
            assert_eq!(set_attr(h.stmt, attribute, 0), SQL_ERROR);
            assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
            assert_eq!(desc_array_size(desc), 5);
            assert_eq!(get_attr(h.stmt, attribute), 5);
        }
    }

    /// The two array sizes are aliases of *different* descriptors, so folding
    /// them onto one canonical field must not fold them onto each other.
    #[test]
    fn row_and_param_array_sizes_stay_on_separate_descriptors() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 3), SQL_SUCCESS);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 8), SQL_SUCCESS);

        assert_eq!(get_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE), 3);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), 8);
        assert_eq!(desc_array_size(h.apd()), 3);
        assert_eq!(desc_array_size(h.ard()), 8);
    }

    /// `SQLSetDescFieldW(shared_ard, SQL_DESC_ARRAY_SIZE, n)` is refused with
    /// `HY010` while any statement on the connection fetches through that
    /// descriptor (`DescHandle::update_definition`), and AB#48943 makes the
    /// attribute the same value, so the attribute spelling must refuse it too.
    /// A shared explicit ARD lets one statement reach another's rowset extent,
    /// whose in-flight fetch has already snapshotted a `row_status_ptr` sized
    /// for the old extent; the caller's own `STMT_STATE_FETCH_IN_PROGRESS`
    /// guard only looks at itself.
    #[test]
    fn array_size_attribute_is_refused_while_a_sibling_fetches_through_the_ard() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let shared = h.alloc_explicit_desc();
        let sibling = h.alloc_extra_stmt();
        for stmt in [h.stmt, sibling] {
            assert_eq!(set_desc(stmt, SQL_ATTR_APP_ROW_DESC, shared), SQL_SUCCESS);
        }
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 2), SQL_SUCCESS);

        let sibling_handle = unsafe { handle_from_raw::<StmtHandle>(sibling) };
        sibling_handle
            .inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_FETCH_IN_PROGRESS);

        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 4), SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), ERR_FUNCTION_SEQUENCE.state);
        assert_eq!(
            desc_array_size(shared),
            2,
            "the sibling's rowset extent must not move under its snapshot"
        );

        sibling_handle
            .inner
            .lock()
            .unwrap()
            .clear_state(STMT_STATE_FETCH_IN_PROGRESS);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 4), SQL_SUCCESS);
        assert_eq!(desc_array_size(shared), 4);
        assert_eq!(h.free_extra_stmt(sibling), SQL_SUCCESS);
    }

    /// The storage is the *effective* descriptor, so associating an explicit
    /// one swaps the value the attribute reports and writes, and the implicit
    /// descriptor keeps its own untouched until it is restored. This is the
    /// behaviour that makes a two-way statement/descriptor sync impossible and
    /// is why the descriptor was made canonical.
    #[test]
    fn array_size_follows_the_effective_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let implicit_apd = h.apd();
        let explicit = h.alloc_explicit_desc();

        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 5), SQL_SUCCESS);
        assert_eq!(set_desc_array_size_field(explicit, 42), SQL_SUCCESS);

        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, explicit),
            SQL_SUCCESS
        );
        assert_eq!(get_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE), 42);

        // A set now lands on the explicit descriptor, not the implicit one.
        assert_eq!(set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 9), SQL_SUCCESS);
        assert_eq!(desc_array_size(explicit), 9);
        assert_eq!(desc_array_size(implicit_apd), 5);

        // Restoring the implicit descriptor restores its own value.
        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, SQL_NULL_HANDLE),
            SQL_SUCCESS
        );
        assert_eq!(get_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE), 5);
    }

    /// Freeing an associated explicit descriptor resets the statement to its
    /// implicit one, so the reported array size must revert with it rather
    /// than keep reading through a dead handle.
    #[test]
    fn freeing_an_explicit_descriptor_reverts_the_array_size() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit = h.alloc_explicit_desc();

        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 4), SQL_SUCCESS);
        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, explicit),
            SQL_SUCCESS
        );
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 16), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), 16);

        assert_eq!(h.free_explicit_desc(explicit), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), 4);
        assert_eq!(desc_array_size(h.ard()), 4);
    }

    /// Panics while holding a descriptor's lock, leaving the mutex poisoned.
    fn poison_desc(desc: SqlHandle) {
        let handle = unsafe { handle_from_raw::<DescHandle>(desc) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.inner.lock().unwrap();
            panic!("poison the desc lock");
        }));
    }

    /// Reads `SQL_ATTR_PARAMSET_SIZE` without asserting success, for the
    /// failure paths below.
    fn try_get_paramset_size(stmt: SqlHandle) -> SqlReturn {
        let mut out: SqlULen = 0;
        unsafe {
            sql_get_stmt_attr_w(
                stmt,
                SQL_ATTR_PARAMSET_SIZE,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        }
    }

    /// Both array-size paths resolve the effective descriptor under the STMT
    /// lock and release it before taking the DESC lock, so the descriptor can
    /// be freed in that window. Each must fail cleanly with `HY000` rather
    /// than dereference freed memory — and both have to be covered, since the
    /// set and get paths resolve independently.
    #[test]
    fn array_size_fails_cleanly_on_a_freed_descriptor() {
        for get_path in [false, true] {
            let mut h = TestHandles::with_env_dbc_stmt();
            let explicit = h.alloc_explicit_desc();
            assert_eq!(
                set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, explicit),
                SQL_SUCCESS
            );
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            // Clearing the association first keeps `free_desc`'s reset walk
            // from undoing the setup; restoring it afterwards reproduces the
            // dangling pointer the resolve/lock window leaves behind. The free
            // still goes through the fixture so the descriptor is unregistered
            // from the DBC and untracked, leaving teardown's
            // outstanding-descriptor assertion satisfied.
            stmt.inner.lock().unwrap().active_apd = None;
            assert_eq!(h.free_explicit_desc(explicit), SQL_SUCCESS);
            stmt.inner.lock().unwrap().active_apd = Some(explicit);

            let rc = if get_path {
                try_get_paramset_size(h.stmt)
            } else {
                set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 4)
            };
            assert_eq!(rc, SQL_ERROR, "get_path={get_path}");
            assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY000);

            // Drop the dangling association before teardown walks it.
            stmt.inner.lock().unwrap().active_apd = None;
        }
    }

    /// The other arm of the same helper: a descriptor whose mutex is poisoned
    /// must fail the call rather than report a fabricated array size.
    #[test]
    fn array_size_fails_cleanly_on_a_poisoned_descriptor() {
        for get_path in [false, true] {
            let mut h = TestHandles::with_env_dbc_stmt();
            let explicit = h.alloc_explicit_desc();
            assert_eq!(
                set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, explicit),
                SQL_SUCCESS
            );
            poison_desc(explicit);

            let rc = if get_path {
                try_get_paramset_size(h.stmt)
            } else {
                set_attr(h.stmt, SQL_ATTR_PARAMSET_SIZE, 4)
            };
            assert_eq!(rc, SQL_ERROR, "get_path={get_path}");
            assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY000);
        }
    }

    /// Both array-size gets return early from the common attribute path to
    /// reach the descriptor, so they own the value-width write that path
    /// performs at its tail. Leaving it unwritten would hand the caller
    /// whatever was already in that memory.
    #[test]
    fn array_size_get_reports_the_value_width() {
        let h = TestHandles::with_env_dbc_stmt();
        for attribute in [SQL_ATTR_ROW_ARRAY_SIZE, SQL_ATTR_PARAMSET_SIZE] {
            let mut out: SqlULen = 999;
            let mut written: SqlInteger = -1;
            assert_eq!(
                unsafe {
                    sql_get_stmt_attr_w(
                        h.stmt,
                        attribute,
                        (&mut out as *mut SqlULen).cast(),
                        0,
                        &mut written,
                    )
                },
                SQL_SUCCESS
            );
            assert_eq!(out, 1, "both default to a single set/row");
            assert_eq!(written, size_of::<SqlULen>() as SqlInteger);

            // A null value pointer is still a success, and still reports the
            // width.
            written = -1;
            assert_eq!(
                unsafe {
                    sql_get_stmt_attr_w(h.stmt, attribute, std::ptr::null_mut(), 0, &mut written)
                },
                SQL_SUCCESS
            );
            assert_eq!(written, size_of::<SqlULen>() as SqlInteger);
        }
    }

    #[test]
    fn get_stmt_attr_null_value_ptr_is_noop_success() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_ARRAY_SIZE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
    }

    fn read_desc(stmt: SqlHandle, attribute: SqlInteger) -> (SqlReturn, SqlHandle) {
        let mut out: SqlHandle = SQL_NULL_HANDLE;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                stmt,
                attribute,
                &mut out as *mut SqlHandle as SqlPointer,
                0,
                std::ptr::null_mut(),
            )
        };
        (rc, out)
    }

    #[test]
    fn get_returns_the_four_implicit_descriptors() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt_ref = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };

        for (attr, expected) in [
            (SQL_ATTR_APP_ROW_DESC, stmt_ref.ard),
            (SQL_ATTR_APP_PARAM_DESC, stmt_ref.apd),
            (SQL_ATTR_IMP_ROW_DESC, stmt_ref.ird),
            (SQL_ATTR_IMP_PARAM_DESC, stmt_ref.ipd),
        ] {
            let (rc, out) = read_desc(h.stmt, attr);
            assert_eq!(rc, SQL_SUCCESS);
            assert!(!out.is_null());
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn get_implicit_descriptors_are_distinct() {
        let h = TestHandles::with_env_dbc_stmt();
        let all = [
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1,
            read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC).1,
            read_desc(h.stmt, SQL_ATTR_IMP_ROW_DESC).1,
            read_desc(h.stmt, SQL_ATTR_IMP_PARAM_DESC).1,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "descriptors {i} and {j} alias");
            }
        }
    }

    /// Regression: querying one of the four implicit descriptor attributes
    /// must clear stale diagnostics from an earlier failed call on this
    /// statement, same as every other `SQLGetStmtAttrW` attribute — ODBC
    /// resets a handle's diagnostic records at the start of every call
    /// except `SQLGetDiagRec`/`SQLGetDiagField`. An earlier implementation
    /// answered these four attributes before the lock (and `free_errors`)
    /// were reached, so a stale diagnostic from a prior failure survived a
    /// subsequent `SQLGetStmtAttrW(SQL_ATTR_APP_PARAM_DESC)` call.
    #[test]
    fn get_descriptor_attribute_clears_stale_diagnostics() {
        let h = TestHandles::with_env_dbc_stmt();
        // Any unrecognized attribute posts a diagnostic on this statement.
        let ret = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                0x7FFF,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert!(!stmt.inner.lock().unwrap().diag_records.is_empty());

        let (rc, _) = read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC);
        assert_eq!(rc, SQL_SUCCESS);
        assert!(
            stmt.inner.lock().unwrap().diag_records.is_empty(),
            "stale diagnostic from the prior failure was not cleared"
        );
    }

    // ---- S4: remaining statement attributes -------------------------------
    //
    // Every expectation below was measured against msodbcsql 18 rather than
    // read out of the ODBC headers; see `docs/attributes_plan.md` §8.

    /// Sets an attribute and returns the return code.
    fn set_attr(stmt: SqlHandle, attribute: SqlInteger, value: SqlULen) -> SqlReturn {
        unsafe { sql_set_stmt_attr_w(stmt, attribute, value as SqlPointer, 0) }
    }

    /// Reads an attribute back, asserting the get itself succeeded.
    fn get_attr(stmt: SqlHandle, attribute: SqlInteger) -> SqlULen {
        let mut out: SqlULen = 0;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                stmt,
                attribute,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS, "get of attribute {attribute} failed");
        out
    }

    /// The msodbcsql defaults, restated independently of the table the driver
    /// reads them from so the two can be compared rather than agreeing by
    /// construction.
    const MEASURED_DEFAULTS: &[(SqlInteger, SqlULen)] = &[
        (SQL_ATTR_NOSCAN, 0),
        (SQL_ATTR_MAX_LENGTH, 0),
        (SQL_ATTR_ASYNC_ENABLE, 0),
        (SQL_ATTR_KEYSET_SIZE, 0),
        (SQL_ROWSET_SIZE, 1),
        (SQL_ATTR_SIMULATE_CURSOR, SQL_SC_UNIQUE),
        (SQL_ATTR_RETRIEVE_DATA, SQL_RD_ON),
        (SQL_ATTR_USE_BOOKMARKS, 0),
        (SQL_ATTR_ENABLE_AUTO_IPD, 0),
        (SQL_ATTR_FETCH_BOOKMARK_PTR, 0),
        (SQL_ATTR_PARAM_BIND_OFFSET_PTR, 0),
        (SQL_ATTR_PARAM_BIND_TYPE, 0),
        (SQL_ATTR_PARAM_OPERATION_PTR, 0),
        (SQL_ATTR_PARAM_STATUS_PTR, 0),
        (SQL_ATTR_PARAMS_PROCESSED_PTR, 0),
        (SQL_ATTR_ROW_BIND_OFFSET_PTR, 0),
        (SQL_ATTR_ROW_OPERATION_PTR, 0),
        (SQL_ATTR_METADATA_ID, 0),
        (SQL_ATTR_CURSOR_SENSITIVITY, SQL_INSENSITIVE),
    ];

    /// A get before any set must answer the measured default, because an
    /// application that reads an attribute to decide whether to change it would
    /// otherwise take a different branch on each driver.
    #[test]
    fn inert_attribute_defaults_match_msodbcsql() {
        let h = TestHandles::with_env_dbc_stmt();
        for &(attribute, expected) in MEASURED_DEFAULTS {
            assert_eq!(get_attr(h.stmt, attribute), expected, "attr {attribute}");
        }
    }

    /// Guards [`MEASURED_DEFAULTS`] against drift: adding an identifier to the
    /// store without measuring its msodbcsql default would otherwise ship an
    /// invented value that no test ever looks at.
    #[test]
    fn every_inert_identifier_has_a_measured_default() {
        for attribute in InertStmtAttrs::identifiers() {
            assert!(
                MEASURED_DEFAULTS.iter().any(|&(id, _)| id == attribute),
                "attribute {attribute} is stored but has no asserted default"
            );
        }
    }

    /// The set/get asymmetry this slice closes: these were previously accepted
    /// and discarded, so a read-back reported a stale default.
    #[test]
    fn inert_attributes_round_trip_the_written_value() {
        let h = TestHandles::with_env_dbc_stmt();
        for (attribute, value) in [
            (SQL_ATTR_NOSCAN, 1),
            (SQL_ATTR_ASYNC_ENABLE, 1),
            (SQL_ROWSET_SIZE, 10),
            (SQL_ATTR_RETRIEVE_DATA, 0),
            (SQL_ATTR_USE_BOOKMARKS, 2),
            (SQL_ATTR_ENABLE_AUTO_IPD, 1),
            (SQL_ATTR_FETCH_BOOKMARK_PTR, 0x1234),
            (SQL_ATTR_PARAM_BIND_OFFSET_PTR, 0x2345),
            (SQL_ATTR_PARAM_BIND_TYPE, 16),
            (SQL_ATTR_PARAM_OPERATION_PTR, 0x3456),
            (SQL_ATTR_PARAM_STATUS_PTR, 0x4567),
            (SQL_ATTR_PARAMS_PROCESSED_PTR, 0x5678),
            (SQL_ATTR_ROW_BIND_OFFSET_PTR, 0x6789),
            (SQL_ATTR_ROW_OPERATION_PTR, 0x789a),
        ] {
            assert_eq!(
                set_attr(h.stmt, attribute, value),
                SQL_SUCCESS,
                "set {attribute}"
            );
            assert_eq!(get_attr(h.stmt, attribute), value, "readback {attribute}");
        }
    }

    #[test]
    fn metadata_id_rejects_identifier_mode_until_catalog_supports_it() {
        let h = TestHandles::with_env_dbc_stmt();

        assert_eq!(set_attr(h.stmt, SQL_ATTR_METADATA_ID, 0), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_METADATA_ID), 0);

        assert_eq!(set_attr(h.stmt, SQL_ATTR_METADATA_ID, 1), SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HYC00);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_METADATA_ID), 0);

        assert_eq!(set_attr(h.stmt, SQL_ATTR_METADATA_ID, 2), SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_METADATA_ID), 0);
    }

    /// `SQL_ROWSET_SIZE` shares a name with `SQL_ATTR_ROW_ARRAY_SIZE` but not a
    /// slot: msodbcsql keeps them independent, so treating one as an alias of
    /// the other would silently resize an application's rowset.
    #[test]
    fn rowset_size_is_independent_of_row_array_size() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ROWSET_SIZE, 7), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ROWSET_SIZE), 7);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), 1);

        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE, 5), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_ARRAY_SIZE), 5);
        assert_eq!(get_attr(h.stmt, SQL_ROWSET_SIZE), 7);
    }

    /// A non-zero `SQL_ATTR_MAX_LENGTH` is substituted rather than honored, and
    /// the substitution is announced. The cap is cosmetic on msodbcsql too — a
    /// longer value still comes back whole — so only the reported state matters.
    #[test]
    fn max_length_substitutes_and_warns() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_MAX_LENGTH, 10),
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_LENGTH), MSODBCSQL_MAX_LENGTH);
    }

    #[test]
    fn max_length_zero_is_accepted_silently() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ATTR_MAX_LENGTH, 0), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_LENGTH), 0);
    }

    /// Setting the already-substituted value is not itself a change, so it must
    /// not warn a second time.
    #[test]
    fn max_length_at_the_substituted_value_is_accepted_silently() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = set_attr(h.stmt, SQL_ATTR_MAX_LENGTH, MSODBCSQL_MAX_LENGTH);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_LENGTH), MSODBCSQL_MAX_LENGTH);
    }

    #[test]
    fn keyset_size_zero_accepted_nonzero_warns_and_stays_zero() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ATTR_KEYSET_SIZE, 0), SQL_SUCCESS);

        let rc = set_attr(h.stmt, SQL_ATTR_KEYSET_SIZE, 100);
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_KEYSET_SIZE), 0);
    }

    #[test]
    fn simulate_cursor_accepts_only_unique() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_SIMULATE_CURSOR, SQL_SC_UNIQUE),
            SQL_SUCCESS
        );

        let rc = set_attr(h.stmt, SQL_ATTR_SIMULATE_CURSOR, 0);
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_SIMULATE_CURSOR), SQL_SC_UNIQUE);
    }

    /// `SQL_UNSPECIFIED` means "whatever the driver does", so the get must
    /// resolve it to the driver's actual sensitivity instead of echoing the
    /// request. msodbcsql resolves it silently, with no `01S02`.
    #[test]
    fn cursor_sensitivity_unspecified_resolves_to_insensitive() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_CURSOR_SENSITIVITY, 0),
            SQL_SUCCESS
        );
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_SENSITIVITY),
            SQL_INSENSITIVE
        );
    }

    fn set_desc(stmt: SqlHandle, attribute: SqlInteger, value: SqlHandle) -> SqlReturn {
        unsafe { sql_set_stmt_attr_w(stmt, attribute, value as SqlPointer, 0) }
    }

    #[test]
    fn set_app_row_desc_associates_explicit_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc)
        );
        // APD is untouched.
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC).1, h.apd());
    }

    #[test]
    fn set_app_param_desc_associates_explicit_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC),
            (SQL_SUCCESS, desc)
        );
        // ARD is untouched.
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1, h.ard());
    }

    #[test]
    fn reassociation_replaces_previous_explicit_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc_a = h.alloc_explicit_desc();
        let desc_b = h.alloc_explicit_desc();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc_a), SQL_SUCCESS);
        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc_b), SQL_SUCCESS);
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc_b)
        );
    }

    #[test]
    fn reset_to_implicit_via_null() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();
        let implicit_ard = h.ard();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, SQL_NULL_HANDLE),
            SQL_SUCCESS
        );
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, implicit_ard)
        );
    }

    #[test]
    fn cursor_sensitivity_explicit_values_round_trip() {
        let h = TestHandles::with_env_dbc_stmt();
        for value in [1, 2] {
            assert_eq!(
                set_attr(h.stmt, SQL_ATTR_CURSOR_SENSITIVITY, value),
                SQL_SUCCESS
            );
            assert_eq!(get_attr(h.stmt, SQL_ATTR_CURSOR_SENSITIVITY), value);
        }
    }

    #[test]
    fn cursor_scrollable_nonscrollable_accepted() {
        let h = TestHandles::with_env_dbc_stmt();
        let rc = set_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE, SQL_NONSCROLLABLE);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE),
            SQL_NONSCROLLABLE
        );
    }

    /// Scrollability is the cursor type seen from the other side, so a request
    /// for a scrollable cursor is refused exactly as `SQL_ATTR_CURSOR_TYPE`
    /// refuses a non-forward-only type: substituted, and announced.
    #[test]
    fn cursor_scrollable_request_is_substituted_like_cursor_type() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE, 1),
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01S02);
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE),
            SQL_NONSCROLLABLE
        );
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_TYPE),
            SQL_CURSOR_FORWARD_ONLY
        );
    }

    /// The pair can never disagree, because scrollability is derived from the
    /// cursor type rather than stored alongside it.
    #[test]
    fn cursor_scrollable_stays_consistent_with_cursor_type() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_CURSOR_TYPE, 2),
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(
            get_attr(h.stmt, SQL_ATTR_CURSOR_SCROLLABLE),
            SQL_NONSCROLLABLE
        );
    }

    #[test]
    fn max_rows_defaults_to_unlimited_and_round_trips() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_ROWS), 0);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_MAX_ROWS, 3), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_ROWS), 3);
        assert_eq!(set_attr(h.stmt, SQL_ATTR_MAX_ROWS, 0), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_MAX_ROWS), 0);
    }

    /// `SQL_ATTR_ROW_NUMBER` is get-only; the set falls through to the
    /// recognition table, which knows msodbcsql rejects it.
    #[test]
    fn row_number_cannot_be_set() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_NUMBER, 1), SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY092);
    }

    /// Answerable only while the cursor sits on a row. A fresh statement has no
    /// cursor, so msodbcsql reports `24000` rather than a position.
    #[test]
    fn row_number_without_a_cursor_is_invalid_cursor_state() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 0;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_NUMBER,
                (&mut out as *mut SqlULen).cast(),
                0,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_24000);
    }

    /// Positioned, msodbcsql answers 0 rather than an ordinal: a forward-only
    /// cursor keeps no rowset origin to number rows against.
    #[test]
    fn row_number_on_a_positioned_cursor_is_zero() {
        let h = TestHandles::with_env_dbc_stmt();
        {
            let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
            stmt.inner.lock().unwrap().row_positioned = true;
        }
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_NUMBER), 0);
    }

    /// The inert table is a linear scan over identifiers, so a duplicate would
    /// make the second entry unreachable and pin its attribute to the first
    /// one's value.
    #[test]
    fn inert_attribute_identifiers_are_unique() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        let state = stmt.inner.lock().unwrap();
        let ids: Vec<SqlInteger> = InertStmtAttrs::identifiers().collect();
        assert!(!ids.is_empty());
        for (i, attribute) in ids.iter().enumerate() {
            assert!(
                !ids[..i].contains(attribute),
                "duplicate inert attribute {attribute}"
            );
            assert!(state.inert_attrs.get(*attribute).is_some());
        }
    }

    // ---- SQL Server vendor statement attributes (SQL_SOPT_SS_*) ----
    //
    // Every expectation below is a measurement taken from msodbcsql 18 with
    // `probe_vendor_stmt.py` / `probe_vendor_bounds.py`, not a reading of the
    // ODBC headers: two of these attributes accept values the headers do not
    // name, and two reject every value the headers do name.

    /// The vendor table is scanned linearly like the inert one, so a duplicate
    /// identifier would silently shadow the second entry's default and rule.
    #[test]
    fn vendor_attribute_identifiers_are_unique() {
        let ids: Vec<SqlInteger> = VendorStmtAttrs::identifiers().collect();
        assert!(!ids.is_empty());
        for (i, attribute) in ids.iter().enumerate() {
            assert!(
                !ids[..i].contains(attribute),
                "duplicate vendor attribute {attribute}"
            );
        }
    }

    /// `set` is documented to reject anything the table does not know rather
    /// than silently accept it. Callers check `is_settable` first, so this
    /// guard is defensive — but a future caller that forgets would otherwise
    /// write past the table.
    #[test]
    fn vendor_set_rejects_an_identifier_outside_the_table() {
        let mut attrs = VendorStmtAttrs::default();
        assert!(!attrs.set(SQL_ATTR_QUERY_TIMEOUT, 0));
        assert!(!attrs.set(9999, 1));
        assert_eq!(attrs.get(9999), None);
    }

    /// Defaults msodbcsql reports before any set. Four are non-zero
    /// (`TEXTPTR_LOGGING`, `NOCOUNT_STATUS`, `DEFER_PREPARE` and the five-day
    /// `QUERYNOTIFICATION_TIMEOUT`), which is the whole reason the table stores
    /// a default per attribute rather than zero-initialising.
    #[test]
    fn vendor_attribute_defaults_match_msodbcsql() {
        let h = TestHandles::with_env_dbc_stmt();
        for (attribute, expected) in [
            (SQL_SOPT_SS_TEXTPTR_LOGGING, 1),
            (SQL_SOPT_SS_CURRENT_COMMAND, 0),
            (SQL_SOPT_SS_HIDDEN_COLUMNS, 0),
            (SQL_SOPT_SS_NOBROWSETABLE, 0),
            (SQL_SOPT_SS_REGIONALIZE, 0),
            (SQL_SOPT_SS_CURSOR_OPTIONS, 0),
            (SQL_SOPT_SS_NOCOUNT_STATUS, 1),
            (SQL_SOPT_SS_DEFER_PREPARE, 1),
            (SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT, 432_000),
            (SQL_SOPT_SS_PARAM_FOCUS, 0),
            (SQL_SOPT_SS_NAME_SCOPE, 0),
            (SQL_SOPT_SS_COLUMN_ENCRYPTION, 0),
        ] {
            assert_eq!(
                get_attr(h.stmt, attribute),
                expected,
                "default for vendor attribute {attribute}"
            );
        }
    }

    /// The boolean-valued vendor attributes take 0 and 1 and nothing else.
    #[test]
    fn vendor_boolean_attributes_accept_only_zero_and_one() {
        for attribute in [
            SQL_SOPT_SS_TEXTPTR_LOGGING,
            SQL_SOPT_SS_HIDDEN_COLUMNS,
            SQL_SOPT_SS_NOBROWSETABLE,
            SQL_SOPT_SS_REGIONALIZE,
            SQL_SOPT_SS_DEFER_PREPARE,
        ] {
            let h = TestHandles::with_env_dbc_stmt();
            for value in [0, 1] {
                let ret = unsafe { sql_set_stmt_attr_w(h.stmt, attribute, value as SqlPointer, 0) };
                assert_eq!(ret, SQL_SUCCESS, "set {attribute} = {value}");
                assert_eq!(get_attr(h.stmt, attribute), value);
            }
            let ret = unsafe { sql_set_stmt_attr_w(h.stmt, attribute, 2 as SqlPointer, 0) };
            assert_eq!(ret, SQL_ERROR, "set {attribute} = 2 should be rejected");
            assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
        }
    }

    /// `SQL_SOPT_SS_CURSOR_OPTIONS` is a three-bit mask, so it accepts the whole
    /// 0..=7 range rather than an enumerated set — a detail the headers do not
    /// state and only the value sweep revealed.
    #[test]
    fn vendor_cursor_options_accepts_the_three_bit_range() {
        let h = TestHandles::with_env_dbc_stmt();
        for value in 0..=7 {
            let ret = unsafe {
                sql_set_stmt_attr_w(h.stmt, SQL_SOPT_SS_CURSOR_OPTIONS, value as SqlPointer, 0)
            };
            assert_eq!(ret, SQL_SUCCESS, "cursor options {value}");
            assert_eq!(get_attr(h.stmt, SQL_SOPT_SS_CURSOR_OPTIONS), value);
        }
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_SOPT_SS_CURSOR_OPTIONS, 8 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
    }

    /// `SQL_SOPT_SS_NAME_SCOPE` tops out at 3.
    #[test]
    fn vendor_name_scope_range_is_bounded() {
        let h = TestHandles::with_env_dbc_stmt();
        for value in 0..=3 {
            let ret = unsafe {
                sql_set_stmt_attr_w(h.stmt, SQL_SOPT_SS_NAME_SCOPE, value as SqlPointer, 0)
            };
            assert_eq!(ret, SQL_SUCCESS, "name scope {value}");
        }
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_SOPT_SS_NAME_SCOPE, 4 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
    }

    /// Zero is rejected for the query-notification timeout, unlike most ODBC
    /// timeouts where zero means "no limit". Measured, not assumed.
    #[test]
    fn vendor_query_notification_timeout_rejects_zero() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret = unsafe {
            sql_set_stmt_attr_w(
                h.stmt,
                SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT,
                0 as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
        // The rejected set must not have disturbed the default.
        assert_eq!(
            get_attr(h.stmt, SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT),
            432_000
        );

        for value in [1, 432_000, i32::MAX as SqlULen] {
            let ret = unsafe {
                sql_set_stmt_attr_w(
                    h.stmt,
                    SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT,
                    value as SqlPointer,
                    0,
                )
            };
            assert_eq!(ret, SQL_SUCCESS, "qn timeout {value}");
            assert_eq!(
                get_attr(h.stmt, SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT),
                value
            );
        }

        // The ceiling is i32::MAX, not the full SQLULEN width: on a 64-bit
        // build the pointer slot can carry more, and msodbcsql refuses it.
        for value in [i32::MAX as SqlULen + 1, u32::MAX as SqlULen] {
            let ret = unsafe {
                sql_set_stmt_attr_w(
                    h.stmt,
                    SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT,
                    value as SqlPointer,
                    0,
                )
            };
            assert_eq!(ret, SQL_ERROR, "qn timeout {value} should be rejected");
            assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
            assert_eq!(
                get_attr(h.stmt, SQL_SOPT_SS_QUERYNOTIFICATION_TIMEOUT),
                i32::MAX as SqlULen,
                "a rejected set must leave the previous value in place"
            );
        }
    }

    /// `SQL_NTS` is the only negative `StringLength` ODBC defines for a
    /// character attribute. msodbcsql answers `HY024` for any other negative
    /// value and keeps the stored string; reading it as empty would silently
    /// clear an attribute the caller never meant to touch.
    #[test]
    fn query_notification_strings_reject_a_negative_length_other_than_nts() {
        let h = TestHandles::with_env_dbc_stmt();
        for attribute in [
            SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT,
            SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS,
        ] {
            assert_eq!(
                set_str_attr(h.stmt, attribute, "SEED", SQL_NTS.into()),
                SQL_SUCCESS
            );

            for length in [-2, -5, -100] {
                let ret = set_str_attr(h.stmt, attribute, "REPLACED", length);
                assert_eq!(ret, SQL_ERROR, "attr {attribute} length {length}");
                assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
                assert_eq!(get_str_attr(h.stmt, attribute, 64).1, "SEED");
            }
        }
    }

    /// `SQL_SOPT_SS_PARAM_FOCUS` and `SQL_SOPT_SS_COLUMN_ENCRYPTION` name
    /// features neither driver offers here, and msodbcsql refuses every value
    /// for them — including the "off" value its own headers define, and
    /// including `COLUMN_ENCRYPTION` on a connection opened with
    /// `ColumnEncryption=Enabled`. They are recognized, so the answer is
    /// `HY024` (bad value) rather than `HY092` (bad identifier).
    #[test]
    fn vendor_unsupported_features_reject_every_value() {
        for attribute in [SQL_SOPT_SS_PARAM_FOCUS, SQL_SOPT_SS_COLUMN_ENCRYPTION] {
            for value in [0, 1, 2] {
                let h = TestHandles::with_env_dbc_stmt();
                let ret = unsafe { sql_set_stmt_attr_w(h.stmt, attribute, value as SqlPointer, 0) };
                assert_eq!(ret, SQL_ERROR, "set {attribute} = {value}");
                assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY024);
            }
        }
    }

    /// A rejected value leaves the previous one in place — a failed set is not
    /// a reset.
    #[test]
    fn vendor_rejected_value_leaves_previous_in_place() {
        let h = TestHandles::with_env_dbc_stmt();
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_SOPT_SS_CURSOR_OPTIONS, 5 as SqlPointer, 0) };
        assert_eq!(ret, SQL_SUCCESS);
        let ret =
            unsafe { sql_set_stmt_attr_w(h.stmt, SQL_SOPT_SS_CURSOR_OPTIONS, 99 as SqlPointer, 0) };
        assert_eq!(ret, SQL_ERROR);
        assert_eq!(get_attr(h.stmt, SQL_SOPT_SS_CURSOR_OPTIONS), 5);
    }

    /// The two get-only vendor attributes answer `HY092` on a set, not `HY024`:
    /// msodbcsql treats them as identifiers that are not settable at all rather
    /// than settable ones given a bad value.
    #[test]
    fn vendor_get_only_attributes_reject_set_as_bad_identifier() {
        for attribute in [SQL_SOPT_SS_CURRENT_COMMAND, SQL_SOPT_SS_NOCOUNT_STATUS] {
            let h = TestHandles::with_env_dbc_stmt();
            let ret = unsafe { sql_set_stmt_attr_w(h.stmt, attribute, 0 as SqlPointer, 0) };
            assert_eq!(ret, SQL_ERROR, "set {attribute}");
            assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_HY092);
        }
    }

    /// `SQL_SOPT_SS_CURRENT_COMMAND` is the ordinal of the command being
    /// processed, not a flag: it starts at 0, becomes 1 on the first result
    /// set, and advances with each further one.
    #[test]
    fn current_command_tracks_the_result_set_ordinal() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(get_attr(h.stmt, SQL_SOPT_SS_CURRENT_COMMAND), 0);

        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        stmt.inner.lock().unwrap().begin_batch(Vec::new());
        assert_eq!(get_attr(h.stmt, SQL_SOPT_SS_CURRENT_COMMAND), 1);

        stmt.inner.lock().unwrap().begin_result_set(Vec::new());
        assert_eq!(get_attr(h.stmt, SQL_SOPT_SS_CURRENT_COMMAND), 2);

        // A second execution restarts the count rather than continuing it.
        stmt.inner.lock().unwrap().begin_batch(Vec::new());
        assert_eq!(get_attr(h.stmt, SQL_SOPT_SS_CURRENT_COMMAND), 1);
    }

    /// Reads a string statement attribute into a fixed buffer, returning the
    /// return code, the decoded value and the reported byte length.
    fn get_str_attr(
        stmt: SqlHandle,
        attribute: SqlInteger,
        buffer_bytes: usize,
    ) -> (SqlReturn, String, SqlInteger) {
        let mut buf = vec![0u16; buffer_bytes.div_ceil(2)];
        let mut written: SqlInteger = -1;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                stmt,
                attribute,
                buf.as_mut_ptr().cast(),
                buffer_bytes as SqlInteger,
                &mut written,
            )
        };
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (rc, String::from_utf16_lossy(&buf[..end]), written)
    }

    /// Sets a string statement attribute from a UTF-16 buffer.
    fn set_str_attr(
        stmt: SqlHandle,
        attribute: SqlInteger,
        value: &str,
        byte_length: SqlInteger,
    ) -> SqlReturn {
        let utf16: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { sql_set_stmt_attr_w(stmt, attribute, utf16.as_ptr() as SqlPointer, byte_length) }
    }

    /// The two query-notification attributes are the only string-valued
    /// statement attributes, and they default to empty.
    #[test]
    fn query_notification_strings_default_to_empty() {
        let h = TestHandles::with_env_dbc_stmt();
        for attribute in [
            SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT,
            SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS,
        ] {
            let (rc, value, written) = get_str_attr(h.stmt, attribute, 64);
            assert_eq!(rc, SQL_SUCCESS);
            assert_eq!(value, "");
            assert_eq!(written, 0);
        }
    }

    #[test]
    fn query_notification_strings_round_trip_independently() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_str_attr(
                h.stmt,
                SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT,
                "msg",
                SQL_NTS.into()
            ),
            SQL_SUCCESS
        );
        assert_eq!(
            set_str_attr(
                h.stmt,
                SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS,
                "service=x",
                SQL_NTS.into()
            ),
            SQL_SUCCESS
        );

        let (rc, value, written) = get_str_attr(h.stmt, SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT, 64);
        assert_eq!((rc, value.as_str(), written), (SQL_SUCCESS, "msg", 6));
        let (rc, value, written) = get_str_attr(h.stmt, SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS, 64);
        assert_eq!(
            (rc, value.as_str(), written),
            (SQL_SUCCESS, "service=x", 18)
        );
    }

    #[test]
    fn query_notification_null_set_clears_the_value() {
        let h = TestHandles::with_env_dbc_stmt();
        for attribute in [
            SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT,
            SQL_SOPT_SS_QUERYNOTIFICATION_OPTIONS,
        ] {
            assert_eq!(
                set_str_attr(h.stmt, attribute, "SEED", SQL_NTS.into()),
                SQL_SUCCESS
            );
            let rc = unsafe {
                sql_set_stmt_attr_w(h.stmt, attribute, std::ptr::null_mut(), SQL_NTS.into())
            };
            assert_eq!(rc, SQL_SUCCESS, "clear attr {attribute}");
            assert_eq!(
                get_str_attr(h.stmt, attribute, 64),
                (SQL_SUCCESS, String::new(), 0)
            );
        }
    }

    /// `StringLength` on the set side is a **byte** count, so an explicit 6
    /// stores three characters. Treating it as a character count would store
    /// six and read past the caller's intent.
    #[test]
    fn query_notification_set_length_is_a_byte_count() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(
            set_str_attr(
                h.stmt,
                SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT,
                "abcdefghij",
                6
            ),
            SQL_SUCCESS
        );
        let (_, value, written) = get_str_attr(h.stmt, SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT, 64);
        assert_eq!((value.as_str(), written), ("abc", 6));
    }

    /// A short buffer truncates with `01004` but still reports the full byte
    /// length the value needs, so a caller can size a second call. A buffer
    /// exactly the size of the value still truncates, because the NUL needs
    /// room.
    #[test]
    fn query_notification_get_truncates_and_reports_full_length() {
        let h = TestHandles::with_env_dbc_stmt();
        set_str_attr(
            h.stmt,
            SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT,
            "abcdefghij",
            SQL_NTS.into(),
        );

        for (buffer_bytes, expected) in [(4, "a"), (10, "abcd"), (20, "abcdefghi")] {
            let (rc, value, written) =
                get_str_attr(h.stmt, SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT, buffer_bytes);
            assert_eq!(rc, SQL_SUCCESS_WITH_INFO, "buffer of {buffer_bytes} bytes");
            assert_eq!(value, expected);
            assert_eq!(written, 20, "full length is always reported");
            assert_eq!(stmt_sql_state(h.stmt), SQLSTATE_01004);
        }

        // One more SQLWCHAR of room for the terminator and it fits.
        let (rc, value, written) = get_str_attr(h.stmt, SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT, 22);
        assert_eq!(
            (rc, value.as_str(), written),
            (SQL_SUCCESS, "abcdefghij", 20)
        );
    }

    /// A null value pointer is the documented length-query form: it succeeds
    /// and reports the length without writing.
    #[test]
    fn query_notification_null_pointer_is_a_length_query() {
        let h = TestHandles::with_env_dbc_stmt();
        set_str_attr(
            h.stmt,
            SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT,
            "abcdefghij",
            SQL_NTS.into(),
        );
        let mut written: SqlInteger = -1;
        let rc = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_SOPT_SS_QUERYNOTIFICATION_MSGTEXT,
                std::ptr::null_mut(),
                0,
                &mut written,
            )
        };
        assert_eq!((rc, written), (SQL_SUCCESS, 20));
    }

    /// msodbcsql writes the value's width into `StringLength` on every
    /// successful integer get. Leaving it untouched hands the caller whatever
    /// happened to be in that memory, so this is a correctness fix and not
    /// cosmetic.
    #[test]
    fn integer_gets_report_the_value_width() {
        let h = TestHandles::with_env_dbc_stmt();
        for attribute in [
            SQL_ATTR_QUERY_TIMEOUT,
            SQL_ATTR_MAX_ROWS,
            SQL_ATTR_NOSCAN,
            SQL_ATTR_CURSOR_TYPE,
            SQL_ATTR_CONCURRENCY,
            SQL_ATTR_ROW_ARRAY_SIZE,
            SQL_ATTR_METADATA_ID,
            SQL_SOPT_SS_DEFER_PREPARE,
        ] {
            let mut out: SqlULen = 0;
            let mut written: SqlInteger = -1;
            let rc = unsafe {
                sql_get_stmt_attr_w(
                    h.stmt,
                    attribute,
                    (&mut out as *mut SqlULen).cast(),
                    size_of::<SqlULen>() as SqlInteger,
                    &mut written,
                )
            };
            assert_eq!(rc, SQL_SUCCESS, "get {attribute}");
            assert_eq!(
                written,
                size_of::<SqlULen>() as SqlInteger,
                "StringLength for attribute {attribute}",
            );
        }
    }

    /// On a failed get msodbcsql leaves `StringLength` alone, so the success
    /// write must not have been hoisted ahead of the error paths.
    #[test]
    fn failed_integer_get_leaves_string_length_untouched() {
        let h = TestHandles::with_env_dbc_stmt();
        let mut out: SqlULen = 0;
        let mut written: SqlInteger = -12345;
        // SQL_ATTR_ROW_NUMBER with no open cursor is 24000.
        let rc = unsafe {
            sql_get_stmt_attr_w(
                h.stmt,
                SQL_ATTR_ROW_NUMBER,
                (&mut out as *mut SqlULen).cast(),
                size_of::<SqlULen>() as SqlInteger,
                &mut written,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        assert_eq!(written, -12345);
    }

    #[test]
    fn reset_to_implicit_via_own_handle() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();
        let implicit_apd = h.apd();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, desc), SQL_SUCCESS);
        // ODBC spec: passing back the handle originally allocated for this
        // statement's APD is the other legal reset spelling, alongside null.
        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, implicit_apd),
            SQL_SUCCESS
        );
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC),
            (SQL_SUCCESS, implicit_apd)
        );
    }

    /// An implicit descriptor can only ever be reassigned as its own
    /// statement's ARD/APD (the reset case). Another statement's implicit
    /// ARD, or this statement's own IRD/IPD, must be rejected — HY017 per the
    /// ODBC reference ("was an implicitly allocated descriptor handle other
    /// than the handle originally allocated for the ARD or APD").
    #[test]
    fn set_app_row_desc_rejects_another_statements_implicit_descriptor() {
        use crate::api::sqlstate::SQLSTATE_HY017;

        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();
        let other_ard = read_desc(other_stmt, SQL_ATTR_APP_ROW_DESC).1;

        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, other_ard),
            SQL_ERROR
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner
                .lock()
                .unwrap()
                .diag_records
                .last()
                .unwrap()
                .sql_state,
            SQLSTATE_HY017
        );
        // Unchanged: still the implicit ARD.
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1, h.ard());
    }

    #[test]
    fn set_app_row_desc_rejects_own_ird_as_ard() {
        let h = TestHandles::with_env_dbc_stmt();
        let own_ird = h.ird();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, own_ird), SQL_ERROR);
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1, h.ard());
    }

    /// `SQLSetStmtAttrW(SQL_ATTR_APP_ROW_DESC/APP_PARAM_DESC)` rejects an
    /// explicit descriptor allocated on a different connection — HY024 per
    /// the ODBC reference.
    #[test]
    fn set_app_row_desc_rejects_cross_connection_descriptor() {
        use crate::api::sqlstate::SQLSTATE_HY024;

        let h = TestHandles::with_env_dbc_stmt();
        let other = h.alloc_other_connection();

        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, other.desc),
            SQL_ERROR
        );
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        assert_eq!(
            stmt.inner
                .lock()
                .unwrap()
                .diag_records
                .last()
                .unwrap()
                .sql_state,
            SQLSTATE_HY024
        );
        assert_eq!(read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC).1, h.ard());
    }

    /// One explicit descriptor may be associated with more than one
    /// statement at once (AB#47436 scope: "Preserve sound synchronization and
    /// lifetime rules when one explicit descriptor is associated with
    /// multiple statements").
    #[test]
    fn explicit_descriptor_can_be_shared_by_two_statements() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();
        let desc = h.alloc_explicit_desc();

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            set_desc(other_stmt, SQL_ATTR_APP_ROW_DESC, desc),
            SQL_SUCCESS
        );

        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc)
        );
        assert_eq!(
            read_desc(other_stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc)
        );
    }

    /// `SQLFreeHandle(SQL_HANDLE_DESC)` on an explicit descriptor currently
    /// associated with one or more statements resets every one of them back
    /// to their own implicit descriptor, rather than leaving a dangling
    /// pointer — mirrors msodbcsql's `FreeDesc(pADesc, NULL, ...)`.
    #[test]
    fn freeing_associated_descriptor_resets_statements_to_implicit() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let other_stmt = h.alloc_extra_stmt();
        let desc = h.alloc_explicit_desc();
        let implicit_ard = h.ard();
        let other_implicit_ard = read_desc(other_stmt, SQL_ATTR_APP_ROW_DESC).1;

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            set_desc(other_stmt, SQL_ATTR_APP_ROW_DESC, desc),
            SQL_SUCCESS
        );

        assert_eq!(h.free_explicit_desc(desc), SQL_SUCCESS);

        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, implicit_ard)
        );
        assert_eq!(
            read_desc(other_stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, other_implicit_ard)
        );
    }

    /// Freeing a statement that currently has an explicit descriptor
    /// associated does not touch the descriptor itself: it is DBC-owned, not
    /// STMT-owned, so it stays valid and can be reused on another statement.
    #[test]
    fn association_survives_statement_free_and_can_be_reused() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let desc = h.alloc_explicit_desc();
        let stmt_to_free = h.alloc_extra_stmt();
        assert_eq!(
            set_desc(stmt_to_free, SQL_ATTR_APP_ROW_DESC, desc),
            SQL_SUCCESS
        );

        assert_eq!(h.free_extra_stmt(stmt_to_free), SQL_SUCCESS);

        assert_eq!(set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, desc), SQL_SUCCESS);
        assert_eq!(
            read_desc(h.stmt, SQL_ATTR_APP_ROW_DESC),
            (SQL_SUCCESS, desc)
        );
    }

    /// Every header alias: attribute, owning descriptor, descriptor field.
    fn header_aliases(h: &TestHandles) -> [(SqlInteger, SqlHandle, SqlUSmallInt); 12] {
        [
            (SQL_ATTR_ROW_ARRAY_SIZE, h.ard(), SQL_DESC_ARRAY_SIZE),
            (SQL_ATTR_PARAMSET_SIZE, h.apd(), SQL_DESC_ARRAY_SIZE),
            (SQL_ATTR_ROW_BIND_TYPE, h.ard(), SQL_DESC_BIND_TYPE),
            (SQL_ATTR_PARAM_BIND_TYPE, h.apd(), SQL_DESC_BIND_TYPE),
            (
                SQL_ATTR_ROW_BIND_OFFSET_PTR,
                h.ard(),
                SQL_DESC_BIND_OFFSET_PTR,
            ),
            (
                SQL_ATTR_PARAM_BIND_OFFSET_PTR,
                h.apd(),
                SQL_DESC_BIND_OFFSET_PTR,
            ),
            (
                SQL_ATTR_ROW_OPERATION_PTR,
                h.ard(),
                SQL_DESC_ARRAY_STATUS_PTR,
            ),
            (
                SQL_ATTR_PARAM_OPERATION_PTR,
                h.apd(),
                SQL_DESC_ARRAY_STATUS_PTR,
            ),
            (SQL_ATTR_ROW_STATUS_PTR, h.ird(), SQL_DESC_ARRAY_STATUS_PTR),
            (
                SQL_ATTR_PARAM_STATUS_PTR,
                h.ipd(),
                SQL_DESC_ARRAY_STATUS_PTR,
            ),
            (
                SQL_ATTR_ROWS_FETCHED_PTR,
                h.ird(),
                SQL_DESC_ROWS_PROCESSED_PTR,
            ),
            (
                SQL_ATTR_PARAMS_PROCESSED_PTR,
                h.ipd(),
                SQL_DESC_ROWS_PROCESSED_PTR,
            ),
        ]
    }

    /// Distinct per alias, non-zero, and fits `SQL_DESC_BIND_TYPE`'s
    /// `SQLINTEGER`. Pointer values are never dereferenced by set/get.
    fn alias_value(index: usize) -> SqlULen {
        0x1000 + 0x10 * index
    }

    fn desc_get(desc: SqlHandle, field: SqlUSmallInt) -> SqlULen {
        let mut out: SqlULen = 0;
        assert_eq!(
            unsafe {
                sql_get_desc_field_w(
                    desc,
                    0,
                    field as SqlSmallInt,
                    (&mut out as *mut SqlULen).cast(),
                    0,
                    std::ptr::null_mut(),
                )
            },
            SQL_SUCCESS,
            "get descriptor field {field}"
        );
        out
    }

    fn desc_set(desc: SqlHandle, field: SqlUSmallInt, value: SqlULen) -> SqlReturn {
        unsafe { sql_set_desc_field_w(desc, 0, field as SqlSmallInt, value as SqlPointer, 0) }
    }

    fn desc_sql_state(desc: SqlHandle) -> [u8; 5] {
        let desc = unsafe { handle_from_raw::<DescHandle>(desc) };
        desc.inner.lock().unwrap().diag_records[0].sql_state
    }

    #[test]
    fn each_alias_attribute_is_visible_through_its_descriptor_field() {
        let h = TestHandles::with_env_dbc_stmt();
        for (index, (attribute, desc, field)) in header_aliases(&h).into_iter().enumerate() {
            let value = alias_value(index);
            assert_eq!(
                set_attr(h.stmt, attribute, value),
                SQL_SUCCESS,
                "set {attribute}"
            );
            assert_eq!(
                desc_get(desc, field),
                value,
                "attribute {attribute} must be the descriptor field {field}"
            );
        }
    }

    #[test]
    fn each_alias_descriptor_field_is_visible_through_its_attribute() {
        let h = TestHandles::with_env_dbc_stmt();
        for (index, (attribute, desc, field)) in header_aliases(&h).into_iter().enumerate() {
            let value = alias_value(index);
            assert_eq!(
                desc_set(desc, field, value),
                SQL_SUCCESS,
                "set field {field}"
            );
            assert_eq!(
                get_attr(h.stmt, attribute),
                value,
                "descriptor field {field} must be attribute {attribute}"
            );
        }
    }

    /// A copy in the statement's store would let the two spellings drift.
    #[test]
    fn no_descriptor_resident_attribute_is_kept_in_the_inert_store() {
        let h = TestHandles::with_env_dbc_stmt();
        let attrs = InertStmtAttrs::default();
        for (attribute, _, _) in header_aliases(&h) {
            assert!(!attrs.contains(attribute), "{attribute} is split storage");
        }
        assert!(!attrs.contains(SQL_ROWSET_SIZE));
    }

    /// Application-side aliases follow the effective ARD/APD; a newly
    /// associated descriptor reports its own defaults (1 for `SQL_ROWSET_SIZE`).
    #[test]
    fn application_aliases_follow_the_effective_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let cases = [
            (SQL_ATTR_ROW_BIND_TYPE, SQL_ATTR_APP_ROW_DESC, 24, 0),
            (
                SQL_ATTR_ROW_BIND_OFFSET_PTR,
                SQL_ATTR_APP_ROW_DESC,
                0x2000,
                0,
            ),
            (SQL_ATTR_ROW_OPERATION_PTR, SQL_ATTR_APP_ROW_DESC, 0x3000, 0),
            (SQL_ROWSET_SIZE, SQL_ATTR_APP_ROW_DESC, 5, 1),
            (SQL_ATTR_PARAM_BIND_TYPE, SQL_ATTR_APP_PARAM_DESC, 48, 0),
            (
                SQL_ATTR_PARAM_BIND_OFFSET_PTR,
                SQL_ATTR_APP_PARAM_DESC,
                0x4000,
                0,
            ),
            (
                SQL_ATTR_PARAM_OPERATION_PTR,
                SQL_ATTR_APP_PARAM_DESC,
                0x5000,
                0,
            ),
        ];
        for (attribute, association, value, fresh_default) in cases {
            // Fresh per case: one explicit descriptor has a single header.
            let explicit = h.alloc_explicit_desc();
            let implicit = if association == SQL_ATTR_APP_ROW_DESC {
                h.ard()
            } else {
                h.apd()
            };
            assert_eq!(set_attr(h.stmt, attribute, value), SQL_SUCCESS);

            assert_eq!(set_desc(h.stmt, association, explicit), SQL_SUCCESS);
            assert_eq!(
                get_attr(h.stmt, attribute),
                fresh_default,
                "{attribute} must report the newly associated descriptor's value"
            );
            assert_eq!(set_attr(h.stmt, attribute, value + 1), SQL_SUCCESS);

            assert_eq!(set_desc(h.stmt, association, implicit), SQL_SUCCESS);
            assert_eq!(
                get_attr(h.stmt, attribute),
                value,
                "{attribute} on the implicit descriptor must be untouched"
            );

            assert_eq!(set_desc(h.stmt, association, explicit), SQL_SUCCESS);
            assert_eq!(get_attr(h.stmt, attribute), value + 1);
            assert_eq!(set_desc(h.stmt, association, implicit), SQL_SUCCESS);
        }
    }

    #[test]
    fn implementation_aliases_do_not_follow_an_application_descriptor_swap() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let explicit = h.alloc_explicit_desc();
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_ROW_STATUS_PTR, 0x6000),
            SQL_SUCCESS
        );
        assert_eq!(
            set_attr(h.stmt, SQL_ATTR_PARAM_STATUS_PTR, 0x7000),
            SQL_SUCCESS
        );
        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_ROW_DESC, explicit),
            SQL_SUCCESS
        );
        assert_eq!(
            set_desc(h.stmt, SQL_ATTR_APP_PARAM_DESC, explicit),
            SQL_SUCCESS
        );
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_STATUS_PTR), 0x6000);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_PARAM_STATUS_PTR), 0x7000);
    }

    /// Both spellings store the full width; `SQLGetDescField` reports the low
    /// 32 bits (msodbcsql `GetADHeaderField`).
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_wide_bind_type_is_whole_through_the_attribute_and_narrowed_by_the_descriptor() {
        let h = TestHandles::with_env_dbc_stmt();
        let wide: SqlULen = 0x1_0000_0018;
        assert_eq!(set_attr(h.stmt, SQL_ATTR_ROW_BIND_TYPE, wide), SQL_SUCCESS);
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_BIND_TYPE), wide);

        let mut narrow: SqlInteger = -1;
        let mut length: SqlInteger = 0;
        assert_eq!(
            unsafe {
                sql_get_desc_field_w(
                    h.ard(),
                    0,
                    SQL_DESC_BIND_TYPE as SqlSmallInt,
                    (&raw mut narrow).cast(),
                    0,
                    &raw mut length,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(narrow, 0x18);
        assert_eq!(length, size_of::<SqlInteger>() as SqlInteger);

        let over_int32: SqlULen = 0x8000_0000;
        assert_eq!(
            desc_set(h.ard(), SQL_DESC_BIND_TYPE, over_int32),
            SQL_SUCCESS
        );
        assert_eq!(get_attr(h.stmt, SQL_ATTR_ROW_BIND_TYPE), over_int32);
    }

    /// The IRD/IPD pointer fields are refused mid-fetch/execute like their
    /// attribute spelling; other IRD writes keep `HY016`.
    #[test]
    fn implementation_header_pointers_are_refused_mid_call_through_the_descriptor() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        for (desc, flag) in [
            (h.ird(), STMT_STATE_FETCH_IN_PROGRESS),
            (h.ipd(), STMT_STATE_EXEC_STARTED),
        ] {
            stmt.inner.lock().unwrap().set_state(flag);
            for field in [SQL_DESC_ARRAY_STATUS_PTR, SQL_DESC_ROWS_PROCESSED_PTR] {
                assert_eq!(desc_set(desc, field, 0x8000), SQL_ERROR, "field {field}");
                assert_eq!(desc_sql_state(desc), ERR_FUNCTION_SEQUENCE.state);
                assert_eq!(desc_get(desc, field), 0, "the value must not move");
            }
            stmt.inner.lock().unwrap().clear_state(flag);
            for field in [SQL_DESC_ARRAY_STATUS_PTR, SQL_DESC_ROWS_PROCESSED_PTR] {
                assert_eq!(desc_set(desc, field, 0x8000), SQL_SUCCESS, "field {field}");
            }
        }

        stmt.inner
            .lock()
            .unwrap()
            .set_state(STMT_STATE_FETCH_IN_PROGRESS);
        assert_eq!(desc_set(h.ird(), SQL_DESC_ARRAY_SIZE, 4), SQL_ERROR);
        assert_eq!(
            desc_sql_state(h.ird()),
            *b"HY016",
            "the blanket IRD gate still answers first"
        );
        stmt.inner
            .lock()
            .unwrap()
            .clear_state(STMT_STATE_FETCH_IN_PROGRESS);
    }

    /// Both spellings of every header alias must refuse while the statement
    /// fetches or executes through the owning descriptor.
    #[test]
    fn both_spellings_of_each_alias_are_refused_mid_call() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        for (attribute, desc, field) in header_aliases(&h) {
            let flag = if desc == h.ard() || desc == h.ird() {
                STMT_STATE_FETCH_IN_PROGRESS
            } else {
                STMT_STATE_EXEC_STARTED
            };
            stmt.inner.lock().unwrap().set_state(flag);
            assert_eq!(
                set_attr(h.stmt, attribute, 8),
                SQL_ERROR,
                "attribute {attribute}"
            );
            assert_eq!(stmt_sql_state(h.stmt), ERR_FUNCTION_SEQUENCE.state);
            assert_eq!(
                desc_set(desc, field, 8),
                SQL_ERROR,
                "field {field} of {attribute}"
            );
            assert_eq!(desc_sql_state(desc), ERR_FUNCTION_SEQUENCE.state);
            stmt.inner.lock().unwrap().clear_state(flag);
        }
    }

    /// A sibling fetching/executing through a shared explicit descriptor
    /// refuses an alias write made through another statement.
    #[test]
    fn alias_attributes_are_refused_while_a_sibling_reads_the_shared_descriptor() {
        let mut h = TestHandles::with_env_dbc_stmt();
        let shared = h.alloc_explicit_desc();
        let sibling = h.alloc_extra_stmt();
        let sibling_handle = unsafe { handle_from_raw::<StmtHandle>(sibling) };
        for (association, flag, attribute) in [
            (
                SQL_ATTR_APP_ROW_DESC,
                STMT_STATE_FETCH_IN_PROGRESS,
                SQL_ATTR_ROW_BIND_OFFSET_PTR,
            ),
            (
                SQL_ATTR_APP_PARAM_DESC,
                STMT_STATE_EXEC_STARTED,
                SQL_ATTR_PARAM_BIND_TYPE,
            ),
        ] {
            for stmt in [h.stmt, sibling] {
                assert_eq!(set_desc(stmt, association, shared), SQL_SUCCESS);
            }
            sibling_handle.inner.lock().unwrap().set_state(flag);
            assert_eq!(set_attr(h.stmt, attribute, 64), SQL_ERROR);
            assert_eq!(stmt_sql_state(h.stmt), ERR_FUNCTION_SEQUENCE.state);
            assert_eq!(
                get_attr(h.stmt, attribute),
                0,
                "the shared value must not move"
            );
            sibling_handle.inner.lock().unwrap().clear_state(flag);
            assert_eq!(set_attr(h.stmt, attribute, 64), SQL_SUCCESS);
        }
        assert_eq!(h.free_extra_stmt(sibling), SQL_SUCCESS);
    }
}
