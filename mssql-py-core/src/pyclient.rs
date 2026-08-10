// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared TDS client cell backing the Python cursors.
//!
//! The connection owns one [`SharedClient`] — an `Arc<Mutex<PyClient>>` holding
//! the connection in one of two interchangeable edges. The async [`TdsClient`]
//! drives every control-plane op (connect, execute, COLMETADATA, advance,
//! close, bulk copy). A sync cursor flips the cell to the reactor-free
//! [`TdsSyncClient`] for its row-pull hot loop and reverts before the next
//! control-plane op; the default `block_on` cursor and the coroutine
//! [`PyCoreAsyncCursor`](crate::async_cursor::PyCoreAsyncCursor) never flip and
//! always use the async arm.
//!
//! The flip runs IN PLACE under the lock via [`std::mem::replace`], so the `Arc`
//! clones each cursor holds keep pointing at the same cell. `into_sync`/
//! `into_async` (which consume the client by value) operate on the value moved
//! OUT of the enum, never on the `Arc`, so `Arc::try_unwrap` is never needed —
//! the L6 structural blocker (live cursor clones) is sidestepped entirely.
//!
//! A [`std::sync::Mutex`] (not tokio) backs the cell so the sync arm acquires
//! the lock with no `.await`, keeping its fetch path reactor-free. The default
//! cursor's async-arm ops wrap their `.await` work in `block_on` while holding
//! the guard on the current thread; `block_on` runs to completion on one thread,
//! so the `!Send` guard held across it is sound. The coroutine cursor instead
//! checks the owned client out of the cell (dropping the guard before any
//! `.await`) via [`with_async_client`], so its path never calls `block_on`.

use std::sync::{Arc, Mutex};

use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, StatementResult, TdsClient};
use mssql_tds::connection::tds_sync_client::{SyncConversion, TdsSyncClient};
use mssql_tds::datatypes::row_writer::RowWriter;
use pyo3::PyErr;
use pyo3::exceptions::PyRuntimeError;
use tokio::runtime::Handle;

use crate::utils::convert_tds_error;

/// The connection's TDS client in one of two interchangeable edges, plus two
/// transient sentinels that only ever exist between a take and a store.
pub(crate) enum PyClient {
    /// Async edge: control-plane + async-core fetch (`block_on`).
    Async(TdsClient),
    /// Reactor-free edge: the sync cursor's row-pull hot loop.
    Sync(TdsSyncClient),
    /// Held only between a `mem::replace`-out and the store-back of a flip.
    /// Observing it means a prior flip panicked mid-swap; treated as poisoned.
    Transitioning,
    /// Unrecoverable: a flip or revert failed and the connection is dead.
    Dead(String),
}

/// The shared, clonable client cell every cursor and the connection hold.
pub(crate) type SharedClient = Arc<Mutex<PyClient>>;

/// Wraps a freshly connected async client in a shared cell.
pub(crate) fn new_shared(client: TdsClient) -> SharedClient {
    Arc::new(Mutex::new(PyClient::Async(client)))
}

fn poisoned() -> PyErr {
    PyRuntimeError::new_err("TDS client mutex was poisoned by a panic")
}

fn dead(msg: &str) -> PyErr {
    PyRuntimeError::new_err(format!("Connection is unusable: {msg}"))
}

/// Reverts `guard` to the async edge in place and returns `&mut TdsClient`.
///
/// No-op when already async. On revert failure the cell is marked [`PyClient::Dead`]
/// and an error is surfaced. A `Transitioning`/`Dead` cell yields an error without
/// re-entering the flip. This is the "revert before control-plane" primitive
/// mirrored from the L5 ODBC edge.
pub(crate) fn ensure_async(guard: &mut PyClient) -> Result<&mut TdsClient, PyErr> {
    match std::mem::replace(guard, PyClient::Transitioning) {
        PyClient::Async(c) => *guard = PyClient::Async(c),
        PyClient::Sync(s) => match s.into_async() {
            Ok(c) => *guard = PyClient::Async(c),
            Err(e) => {
                let msg = e.to_string();
                *guard = PyClient::Dead(msg.clone());
                return Err(dead(&msg));
            }
        },
        PyClient::Transitioning => {
            let msg = "client left in a transitioning state";
            *guard = PyClient::Dead(msg.to_string());
            return Err(dead(msg));
        }
        PyClient::Dead(msg) => {
            let err = dead(&msg);
            *guard = PyClient::Dead(msg);
            return Err(err);
        }
    }
    match guard {
        PyClient::Async(c) => Ok(c),
        _ => unreachable!("ensure_async just stored the Async variant"),
    }
}

/// Locks the cell, reverts to the async edge, and runs a control-plane closure.
fn with_async<R>(
    cell: &SharedClient,
    handle: &Handle,
    f: impl FnOnce(&Handle, &mut TdsClient) -> Result<R, PyErr>,
) -> Result<R, PyErr> {
    let mut guard = cell.lock().map_err(|_| poisoned())?;
    let client = ensure_async(&mut guard)?;
    f(handle, client)
}

/// Reverts the cell to the async edge without running any op (error-recovery /
/// pre-control-plane revert). Used by the sync cursor after a fetch error or
/// before a control-plane transition.
pub(crate) fn revert_to_async(cell: &SharedClient) -> Result<(), PyErr> {
    let mut guard = cell.lock().map_err(|_| poisoned())?;
    ensure_async(&mut guard)?;
    Ok(())
}

/// Flips the cell to the reactor-free sync edge in place.
///
/// Runs `into_sync` under `handle.enter()` so `Handle::try_current()` captures
/// the connection's runtime (ruling 3). A TLS/non-raw transport reports
/// [`SyncConversion::NotEligible`] and the cell stays async — the caller then
/// transparently uses the `block_on` fallback. A failed flip marks the cell dead.
pub(crate) fn flip_to_sync(cell: &SharedClient, handle: &Handle) -> Result<(), PyErr> {
    let mut guard = cell.lock().map_err(|_| poisoned())?;
    match std::mem::replace(&mut *guard, PyClient::Transitioning) {
        PyClient::Async(c) => {
            let converted = {
                let _entered = handle.enter();
                c.into_sync()
            };
            match converted {
                SyncConversion::Converted(s) => *guard = PyClient::Sync(s),
                SyncConversion::NotEligible(c) => *guard = PyClient::Async(c),
                SyncConversion::Failed(e) => {
                    let msg = e.to_string();
                    *guard = PyClient::Dead(msg.clone());
                    return Err(dead(&msg));
                }
            }
        }
        PyClient::Sync(s) => *guard = PyClient::Sync(s),
        PyClient::Transitioning => {
            let msg = "client left in a transitioning state";
            *guard = PyClient::Dead(msg.to_string());
            return Err(dead(msg));
        }
        PyClient::Dead(msg) => {
            let err = dead(&msg);
            *guard = PyClient::Dead(msg);
            return Err(err);
        }
    }
    Ok(())
}

/// Runs a query on the async edge and collapses forward to the first
/// row-returning result set. Returns `(rows_affected, on_rows)` captured on the
/// async client before any flip, so DML rowcount is oracle-faithful (rule C).
pub(crate) fn run_execute(
    cell: &SharedClient,
    handle: &Handle,
    query: String,
    timeout_secs: u32,
) -> Result<(i64, bool), PyErr> {
    with_async(cell, handle, |handle, client| {
        handle.block_on(async {
            if client.has_open_batch() {
                client.close_query().await.map_err(convert_tds_error)?;
            }
            let first = client
                .execute(
                    query,
                    ExecuteOptions {
                        timeout: Some(timeout_secs),
                        ..Default::default()
                    },
                )
                .await
                .map_err(convert_tds_error)?;
            if !matches!(first, StatementResult::Rows) {
                client.advance_to_rows().await.map_err(convert_tds_error)?;
            }
            Ok::<(i64, bool), PyErr>((client.last_rows_affected(), client.on_rows()))
        })
    })
}

/// Whether the current edge is positioned on a row set.
pub(crate) fn is_on_rows(cell: &SharedClient) -> Result<bool, PyErr> {
    let guard = cell.lock().map_err(|_| poisoned())?;
    Ok(match &*guard {
        PyClient::Async(c) => c.on_rows(),
        PyClient::Sync(s) => !s.get_metadata().is_empty(),
        PyClient::Transitioning | PyClient::Dead(_) => false,
    })
}

/// Column count of the current result-set metadata on whichever edge is active.
pub(crate) fn metadata_col_count(cell: &SharedClient) -> Result<usize, PyErr> {
    let guard = cell.lock().map_err(|_| poisoned())?;
    Ok(match &*guard {
        PyClient::Async(c) => c.get_metadata().len(),
        PyClient::Sync(s) => s.get_metadata().len(),
        PyClient::Transitioning | PyClient::Dead(_) => 0,
    })
}

/// Pulls one row into `writer`. The reactor-free sync arm needs no `block_on`;
/// the async arm (TLS fallback) drives `next_row_into().await` via `block_on`.
/// Both arms route through the same shared parse body, so results are
/// byte-identical.
pub(crate) fn fetch_row_into(
    cell: &SharedClient,
    handle: &Handle,
    writer: &mut (dyn RowWriter + Send),
) -> Result<bool, PyErr> {
    let mut guard = cell.lock().map_err(|_| poisoned())?;
    match &mut *guard {
        PyClient::Sync(s) => s.next_row_into(writer).map_err(convert_tds_error),
        PyClient::Async(c) => handle
            .block_on(async { c.next_row_into(writer).await })
            .map_err(convert_tds_error),
        PyClient::Transitioning => Err(dead("client left in a transitioning state")),
        PyClient::Dead(msg) => Err(dead(msg)),
    }
}

/// Reverts to the async edge (if flipped) and closes the current result set.
pub(crate) fn close_resultset(cell: &SharedClient, handle: &Handle) -> Result<(), PyErr> {
    with_async(cell, handle, |handle, client| {
        handle.block_on(async { client.close_query().await.map_err(convert_tds_error) })
    })
}

/// Reverts to the async edge (if flipped) and closes the connection.
pub(crate) fn close_connection(cell: &SharedClient, handle: &Handle) -> Result<(), PyErr> {
    with_async(cell, handle, |handle, client| {
        handle.block_on(async { client.close_connection().await.map_err(convert_tds_error) })
    })
}

/// Checks the async [`TdsClient`] out of the cell, runs an async op on it, then
/// stores it back — used by the coroutine [`PyCoreAsyncCursor`](crate::async_cursor::PyCoreAsyncCursor).
///
/// This is the async analog of [`with_async`]. Because the cell is backed by a
/// [`std::sync::Mutex`] (whose guard is `!Send`), an async task cannot hold the
/// guard across `.await`. Instead this "checks out" the owned client via
/// [`std::mem::replace`] (leaving [`PyClient::Transitioning`]), drops the guard,
/// awaits `f` on the owned value (which is `Send`), then re-locks and stores it
/// back as [`PyClient::Async`]. The store-back runs even when `f` errors, so a
/// mid-fetch error leaves the connection usable (ruling 4) rather than poisoned.
///
/// `f` returns the client alongside its result so ownership round-trips cleanly.
/// Must be polled inside a runtime context — the caller spawns it on the
/// connection's runtime so `TdsClient`'s I/O is driven by its own reactor.
pub(crate) async fn with_async_client<F, Fut, R>(cell: SharedClient, f: F) -> Result<R, PyErr>
where
    F: FnOnce(TdsClient) -> Fut,
    Fut: std::future::Future<Output = (TdsClient, Result<R, PyErr>)>,
{
    let taken = {
        let mut guard = cell.lock().map_err(|_| poisoned())?;
        std::mem::replace(&mut *guard, PyClient::Transitioning)
    };
    let client = match taken {
        PyClient::Async(c) => c,
        // The async cursor never flips to sync, but a sync cursor sharing the
        // cell may have left it on the sync edge; revert it here.
        PyClient::Sync(s) => match s.into_async() {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                if let Ok(mut guard) = cell.lock() {
                    *guard = PyClient::Dead(msg.clone());
                }
                return Err(dead(&msg));
            }
        },
        PyClient::Transitioning => {
            let msg = "client left in a transitioning state";
            if let Ok(mut guard) = cell.lock() {
                *guard = PyClient::Dead(msg.to_string());
            }
            return Err(dead(msg));
        }
        PyClient::Dead(msg) => {
            let err = dead(&msg);
            if let Ok(mut guard) = cell.lock() {
                *guard = PyClient::Dead(msg);
            }
            return Err(err);
        }
    };

    let (client, result) = f(client).await;

    {
        let mut guard = cell.lock().map_err(|_| poisoned())?;
        *guard = PyClient::Async(client);
    }
    result
}
