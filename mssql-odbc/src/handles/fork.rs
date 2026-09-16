// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Recovery for a fork performed while application ODBC calls are quiescent.
//! Tokio workers may still be alive at that point; none survive in the child.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::dbc::{ConnectionIdentity, ConnectionState};
use super::env::SharedRuntime;
use super::{DbcHandle, EnvHandle, HandleType, StmtHandle, live_handles};

static PROCESS_ID: AtomicU32 = AtomicU32::new(0);

fn state_for_abandonment<T>(mutex: &mut Mutex<T>) -> &mut T {
    match mutex.get_mut() {
        Ok(state) => state,
        Err(poisoned) => {
            // Only abandon resources here, never resume work on poisoned
            // state. Retain the poison flag for subsequent ordinary API calls.
            tracing::error!("inherited handle mutex poisoned; abandoning its process-local state");
            poisoned.into_inner()
        }
    }
}

pub(crate) fn ensure_current_process() -> io::Result<()> {
    let process_id = std::process::id();
    if PROCESS_ID.load(Ordering::Acquire) == process_id {
        return Ok(());
    }

    // The registry is used only by application ODBC calls, not Tokio workers.
    // Those calls must have finished before fork. This also serializes callers
    // if the child starts multiple threads before first using the driver.
    let handles = live_handles();
    let previous = PROCESS_ID.load(Ordering::Relaxed);
    if previous == process_id {
        return Ok(());
    }
    if previous != 0 {
        // Build everything before changing any handle, so an allocation failure
        // leaves recovery retryable. This runs in an ordinary API call, never in
        // an async-signal-restricted pthread_atfork child callback.
        let environments = handles
            .iter()
            .filter(|(_, kind)| **kind == HandleType::Env)
            .map(|(&address, _)| Ok((address, SharedRuntime::create()?)))
            .collect::<io::Result<Vec<_>>>()?;

        for (address, runtime) in environments {
            // SAFETY: the registry owns live handle addresses and records their
            // types. No pre-fork ODBC call survives (the caller forked outside
            // the driver), and post-fork calls cannot pass this gate until the
            // release-store below. Thus no Rust handle reference is in use.
            let env = unsafe { &mut *(address as *mut EnvHandle) };
            env.runtime = runtime;
        }
        for (&address, kind) in handles.iter() {
            match kind {
                HandleType::Dbc => {
                    // SAFETY: same exclusive child-process access as above.
                    let dbc = unsafe { &mut *(address as *mut DbcHandle) };
                    dbc.runtime = Arc::clone(&dbc.parent_env().runtime);
                    let state = state_for_abandonment(&mut dbc.inner);
                    // Dropping a Tokio socket deregisters it through the old
                    // reactor. Leave inherited transports to process exit;
                    // never send rollback, shutdown, or reset to the parent.
                    std::mem::forget(state.client.take());
                    state.connection_state = ConnectionState::Forked;
                    state.active_stmt = None;
                    state.local_tran_started = false;
                    state.server_isolation_unknown = false;
                    state.effective_vendor_settings = None;
                    state.effective_packet_size = None;
                    state.identity = ConnectionIdentity::default();
                }
                HandleType::Stmt => {
                    // SAFETY: same exclusive child-process access as above.
                    let stmt = unsafe { &mut *(address as *mut StmtHandle) };
                    let state = state_for_abandonment(&mut stmt.inner);
                    // A data-at-execution sequence owns its client on the STMT,
                    // rather than the DBC, between SQLPutData calls.
                    std::mem::forget(state.dae.take());
                    state.reset_cursor_state();
                }
                _ => {}
            }
        }
    }
    PROCESS_ID.store(process_id, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandonment_can_invalidate_poisoned_state_without_clearing_poison() {
        let mut state = Mutex::new(Some(42));
        let _ = std::panic::catch_unwind(|| {
            let _guard = state.lock().unwrap();
            panic!("poison state before fork recovery");
        });
        assert!(state.is_poisoned());
        *state_for_abandonment(&mut state) = None;
        assert!(state.is_poisoned());
        assert_eq!(*state.get_mut().unwrap_err().into_inner(), None);
    }
}
