// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of `SQLCancel`.
//!
//! `SQLCancel` has two distinct jobs in ODBC. Against a statement executing on
//! another thread it asks the server to abandon the running command; against a
//! statement in the "Need Data" state it abandons a data-at-execution sequence
//! so the statement can be executed again. Only the second is implemented here
//! — asynchronous execution is not supported by this driver, so a cancel that
//! arrives outside a DAE sequence has nothing to interrupt and succeeds
//! trivially.

use tracing::{debug, error};

use super::exec_common::unwind_dae;
use crate::api::odbc_types::{SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlReturn};
use crate::error::free_errors;
use crate::handles::stmt::STMT_STATE_NEED_DATA;
use crate::handles::{HandleType, StmtHandle, handle_from_raw};

/// Cancels processing on a statement.
///
/// When the statement is awaiting data-at-execution input, the parked request
/// is discarded and the statement returns to its prepared state, which is what
/// the ODBC spec requires: "the application can then call `SQLExecute` or
/// `SQLExecDirect` again".
///
/// # Safety
/// - `statement_handle` must be a valid `STMT` handle allocated by
///   `SQLAllocHandle`.
pub(crate) unsafe fn sql_cancel(statement_handle: SqlHandle) -> SqlReturn {
    debug!(?statement_handle, "SQLCancel called");
    crate::ffi_entry!("SQLCancel", unsafe { sql_cancel_impl(statement_handle) })
}

unsafe fn sql_cancel_impl(statement_handle: SqlHandle) -> SqlReturn {
    if statement_handle.is_null() {
        error!("SQLCancel: statement_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let stmt = unsafe { handle_from_raw::<StmtHandle>(statement_handle) };
    debug_assert_eq!(
        stmt.object_type,
        HandleType::Stmt,
        "SQLCancel: handle is not a STMT"
    );

    let needs_data = {
        let Ok(mut stmt_state) = stmt.inner.lock() else {
            error!("SQLCancel: stmt mutex poisoned");
            return SQL_ERROR;
        };
        free_errors(&mut stmt_state);
        stmt_state.has_state(STMT_STATE_NEED_DATA)
    };

    if needs_data {
        debug!("SQLCancel: abandoning data-at-execution sequence");
        unwind_dae(stmt.parent_dbc(), stmt, statement_handle, None);
    }

    SQL_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_INVALID_HANDLE;
    use crate::handles::stmt::STMT_STATE_EXEC_STARTED;
    use crate::test_support::TestHandles;

    #[test]
    fn null_handle_returns_invalid_handle() {
        assert_eq!(SQL_INVALID_HANDLE, unsafe {
            sql_cancel(std::ptr::null_mut())
        });
    }

    #[test]
    fn cancel_outside_need_data_is_a_success_noop() {
        let h = TestHandles::with_env_dbc_stmt();
        assert_eq!(SQL_SUCCESS, unsafe { sql_cancel(h.stmt) });
    }

    #[test]
    fn cancel_clears_need_data_and_restores_prepared_plan() {
        let h = TestHandles::with_env_dbc_stmt();
        let stmt = unsafe { handle_from_raw::<StmtHandle>(h.stmt) };
        {
            let mut state = stmt.inner.lock().unwrap();
            state.set_state(STMT_STATE_NEED_DATA | STMT_STATE_EXEC_STARTED);
            state.dae_param_indices.push(0);
            state.dae_current_bytes_sent = 3;
            state.dae_param_data_first = true;
        }

        assert_eq!(SQL_SUCCESS, unsafe { sql_cancel(h.stmt) });

        let state = stmt.inner.lock().unwrap();
        assert!(!state.has_state(STMT_STATE_NEED_DATA));
        assert!(!state.has_state(STMT_STATE_EXEC_STARTED));
        assert!(state.dae_param_indices.is_empty());
        assert_eq!(state.dae_current_bytes_sent, 0);
        assert!(!state.dae_param_data_first);
    }
}
