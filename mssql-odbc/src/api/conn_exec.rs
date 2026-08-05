// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection-level SQL execution.
//!
//! Transaction control (`SQLEndTran`) and the connection attributes that map
//! onto `SET` statements (`SQL_ATTR_AUTOCOMMIT`, `SQL_ATTR_TXN_ISOLATION`) need
//! to run a batch without an application statement handle. These helpers borrow
//! the connection's TDS client the same way `exec_common` does for a STMT, so
//! the "one active statement per connection" invariant still holds and no lock
//! is held across network I/O.

use tracing::error;

use super::sqlstate::*;
use crate::api::odbc_types::{SQL_ERROR, SqlReturn};
use crate::handles::DbcHandle;
use crate::handles::dbc::ConnectionState;

/// Runs `sql` as a plain batch on the connection and drains the response.
///
/// Returns `Err(SQL_ERROR)` with a diagnostic posted on the connection when the
/// connection is not usable or the server rejects the batch.
pub(crate) fn exec_on_connection(dbc: &DbcHandle, sql: &str, op: &str) -> Result<(), SqlReturn> {
    // Claim marker for `active_stmt`: the DBC's own address is a stable,
    // non-null value that can never collide with a real STMT handle.
    let claim = std::ptr::from_ref(dbc) as *mut std::ffi::c_void;

    let mut client = {
        let Ok(mut state) = dbc.inner.lock() else {
            error!("{op}: dbc mutex poisoned");
            return Err(SQL_ERROR);
        };
        if state.connection_state != ConnectionState::Connected {
            error!("{op}: connection is not open");
            post_diag(&mut state, ERR_CONNECTION_DOES_NOT_EXIST);
            return Err(SQL_ERROR);
        }
        // A statement holding an open cursor would otherwise block transaction
        // control and the `SET`-backed connection attributes forever. Spill its
        // remaining rows so the connection goes idle, which is what a
        // non-MARS session requires before another batch can run.
        if let Some(busy_stmt) = state.active_stmt {
            drop(state);
            if !crate::api::spill::try_release_connection(dbc, busy_stmt) {
                let Ok(mut state) = dbc.inner.lock() else {
                    error!("{op}: dbc mutex poisoned");
                    return Err(SQL_ERROR);
                };
                error!("{op}: connection is busy with another statement's results");
                post_diag(&mut state, ERR_CONNECTION_BUSY);
                return Err(SQL_ERROR);
            }
            let Ok(reacquired) = dbc.inner.lock() else {
                error!("{op}: dbc mutex poisoned");
                return Err(SQL_ERROR);
            };
            state = reacquired;
        }
        let Some(client) = state.client.take() else {
            error!("{op}: no active TDS client");
            post_diag(&mut state, ERR_NO_ACTIVE_TDS_CLIENT);
            return Err(SQL_ERROR);
        };
        state.active_stmt = Some(claim);
        client
    };

    let result = dbc.runtime.block_on(async {
        client.execute(sql.to_string(), ()).await?;
        client.close_query().await
    });

    let info_messages = client.take_info_messages();

    let Ok(mut state) = dbc.inner.lock() else {
        error!("{op}: dbc mutex poisoned returning client");
        return Err(SQL_ERROR);
    };
    state.client = Some(client);
    if state.active_stmt == Some(claim) {
        state.active_stmt = None;
    }
    post_tds_info_messages(&mut state, &info_messages);

    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            error!(%e, "{op}: connection-level batch failed");
            post_tds_error(&mut state, &e, SQLSTATE_HY000);
            Err(SQL_ERROR)
        }
    }
}
