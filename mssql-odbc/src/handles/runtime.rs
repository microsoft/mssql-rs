// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io;
use std::mem::ManuallyDrop;
use std::ops::Deref;

use tokio::runtime::Runtime;
use tracing::error;

/// Reports whether the Windows loader has begun tearing the process down.
///
/// `LdrShutdownProcess` sets this before it runs any `DLL_PROCESS_DETACH`
/// routine, and by then every thread but the one calling `ExitProcess` has
/// already been terminated — at an arbitrary instruction, possibly holding a
/// lock. Synchronizing with those threads is what the loader documentation
/// forbids during detach, and it is exactly what a `tokio::runtime::Runtime`
/// teardown does.
#[cfg(windows)]
fn process_is_shutting_down() -> bool {
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlDllShutdownInProgress() -> u8;
    }
    // SAFETY: no arguments and no out-parameters; reads a loader flag.
    unsafe { RtlDllShutdownInProgress() != 0 }
}

/// Non-Windows platforms do not terminate the process's other threads before
/// running library teardown, so the runtime can always be shut down normally.
#[cfg(not(windows))]
fn process_is_shutting_down() -> bool {
    false
}

/// The Tokio runtime shared by an ENV and its child handles, wrapped so that
/// releasing the last reference during process shutdown never waits on the
/// runtime's threads.
///
/// `Runtime`'s teardown blocks until its worker and blocking-pool threads stop,
/// and takes the scheduler's locks to get there. That is correct while the
/// process is alive and fatal once it is exiting: a driver loaded into an
/// arbitrary host gets `SQLFreeHandle` called from teardown paths that run
/// under `LdrShutdownProcess`, after Windows has terminated every other thread.
/// The wait then never completes — hanging the host process after its last
/// statement succeeded — or trips std's `threads should not terminate
/// unexpectedly` panic. mssql-python reaches this path on every process that
/// used it: its extension module frees the pooled connection handles from its
/// CRT `onexit` table.
///
/// So the runtime is leaked, deliberately, when the loader is already shutting
/// the process down; the OS reclaims the threads moments later. Outside that
/// window it is dropped normally, which is what keeps an application that
/// allocates and frees environments in a loop from accumulating threads.
#[derive(Debug)]
pub(crate) struct SharedRuntime(ManuallyDrop<Runtime>);

impl SharedRuntime {
    pub(crate) fn new() -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .inspect_err(|e| {
                error!(%e, "failed to create Tokio runtime");
            })?;
        Ok(Self(ManuallyDrop::new(runtime)))
    }
}

impl Deref for SharedRuntime {
    type Target = Runtime;

    fn deref(&self) -> &Runtime {
        &self.0
    }
}

impl Drop for SharedRuntime {
    fn drop(&mut self) {
        // SAFETY: `Drop::drop` runs once and nothing reads `self.0` afterwards.
        let runtime = unsafe { ManuallyDrop::take(&mut self.0) };
        if process_is_shutting_down() {
            std::mem::forget(runtime);
            return;
        }
        drop(runtime);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn deref_exposes_the_runtime() {
        let runtime = SharedRuntime::new().expect("runtime");
        assert_eq!(runtime.block_on(async { 41 + 1 }), 42);
    }

    #[test]
    fn dropping_shuts_the_runtime_down_while_the_process_is_alive() {
        struct SetOnDrop(Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        // A normal free must tear the runtime down rather than leak it. Shutdown
        // drops the scheduler's tasks; a leaked runtime would keep this one
        // parked on the pending future forever.
        let dropped = Arc::new(AtomicBool::new(false));
        let runtime = SharedRuntime::new().expect("runtime");
        let guard = SetOnDrop(Arc::clone(&dropped));
        runtime.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });

        assert!(!process_is_shutting_down());
        drop(runtime);
        assert!(
            dropped.load(Ordering::SeqCst),
            "the runtime should have been shut down, not leaked"
        );
    }
}
