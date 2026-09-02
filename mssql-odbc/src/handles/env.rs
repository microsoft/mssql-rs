// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::c_void;
use std::io;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex};

use tokio::runtime::Runtime;
use tracing::error;

use super::{HandleType, HasObjectType};
use crate::api::odbc_types::{SQL_OV_ODBC2, SQL_OV_ODBC3, SQL_OV_ODBC3_80};
use crate::error::{DiagRecord, HasDiagnostics};

/// ODBC environment attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OdbcVersion {
    /// Not yet set — calls requiring a version will fail with HY010.
    Unset = 0,
    Odbc2 = 2,
    Odbc3 = 3,
    Odbc3_80 = 380,
}

impl TryFrom<u32> for OdbcVersion {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            SQL_OV_ODBC2 => Ok(OdbcVersion::Odbc2),
            SQL_OV_ODBC3 => Ok(OdbcVersion::Odbc3),
            SQL_OV_ODBC3_80 => Ok(OdbcVersion::Odbc3_80),
            _ => Err(()),
        }
    }
}

/// Reports whether the Windows loader has begun tearing the process down.
///
/// `LdrShutdownProcess` sets this before it runs any `DLL_PROCESS_DETACH`
/// routine, and by then every thread but the one calling `ExitProcess` has
/// already been terminated — at an arbitrary instruction, possibly holding a
/// lock. Synchronizing with those threads is what the loader documentation
/// forbids during detach.
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

/// A Tokio runtime that never synchronizes with its worker threads when it
/// goes away.
///
/// `SQLFreeHandle(SQL_HANDLE_ENV)` can run long after the OS has already
/// force-terminated background threads — e.g. a caller that loads this driver
/// directly instead of through the ODBC Driver Manager may defer the free to a
/// C++ static destructor that only runs at `DLL_PROCESS_DETACH`, by which point
/// Windows has already killed every thread but the one tearing the process
/// down (AB#47509). The default `Runtime` drop unconditionally joins its worker
/// thread, which panics ("threads should not terminate unexpectedly") if that
/// thread no longer exists.
///
/// `shutdown_background` is not sufficient (AB#47510). It skips the *join*, but
/// it still signals the scheduler and takes its locks — locks a terminated
/// worker may have been holding at the instruction the OS stopped it on. The
/// result is a process that hangs on the way out instead of panicking, which is
/// the worse failure: it strands the host after its last statement already
/// succeeded, and it does so on the main-thread teardown path that previously
/// only printed the panic and exited. So once the loader reports the process is
/// already going away, the runtime is leaked deliberately and nothing is
/// signalled at all; the OS reclaims the threads moments later.
///
/// Outside that window `shutdown_background` still runs, so an application that
/// allocates and frees environments in a loop does not accumulate threads.
///
/// This lives on the shared value rather than on `EnvHandle` so the guarantee
/// holds for whichever owner happens to release the last reference. A DBC that
/// outlives its ENV — which a non-conformant caller can produce, since
/// `SQLFreeHandle(ENV)` only `debug_assert!`s that the connection list is empty
/// — would otherwise drop the runtime through the default joining path, which
/// is exactly the teardown this exists to avoid.
#[derive(Debug)]
pub(crate) struct SharedRuntime(ManuallyDrop<Runtime>);

impl SharedRuntime {
    fn new(runtime: Runtime) -> Self {
        Self(ManuallyDrop::new(runtime))
    }
}

impl std::ops::Deref for SharedRuntime {
    type Target = Runtime;

    fn deref(&self) -> &Runtime {
        &self.0
    }
}

impl Drop for SharedRuntime {
    fn drop(&mut self) {
        // SAFETY: `drop` runs at most once and nothing reads `self.0` after
        // this, so taking ownership out of the `ManuallyDrop` is sound. Taking
        // it by value is what lets us either forget the `Runtime` or call
        // `shutdown_background`, which consumes it; the default drop glue would
        // join instead.
        let runtime = unsafe { ManuallyDrop::take(&mut self.0) };
        if process_is_shutting_down() {
            std::mem::forget(runtime);
            return;
        }
        runtime.shutdown_background();
    }
}

/// Environment handle
///
/// One ENV is typically allocated per application. It owns connection handles
/// and stores environment-level attributes (ODBC version, connection pooling mode).
///
/// Thread-safety: The `inner` mutex protects mutable state. msodbcsql serializes
/// via an environment-level critical section (Unix) or relies on the Driver
/// Manager (Windows). We always protect with a mutex for safety regardless of platform.
/// `object_type` is set once at construction and never mutated; `inner` protects all mutable state.
#[derive(Debug)]
pub(crate) struct EnvHandle {
    pub(crate) object_type: HandleType,
    pub(crate) inner: Mutex<EnvState>,
    /// Shared Tokio runtime for all connections on this ENV, in an `Arc` so
    /// DBCs can hold a reference without lifetime issues. Shutdown is handled
    /// by `SharedRuntime`'s own `Drop`, so this handle needs no `Drop` of its
    /// own and the field is never vacated while the ENV is live.
    pub(crate) runtime: Arc<SharedRuntime>,
}

/// Mutable state within an environment handle, protected by `inner`.
#[derive(Debug)]
pub(crate) struct EnvState {
    pub(crate) diag_records: Vec<DiagRecord>,
    pub(crate) odbc_version: OdbcVersion,
    #[allow(dead_code)]
    pub(crate) output_nts: bool,
    /// Active child DBC handles
    pub(crate) connections: Vec<*mut c_void>,
}

impl HasDiagnostics for EnvState {
    fn diag_records(&self) -> &[DiagRecord] {
        &self.diag_records
    }
    fn diag_records_mut(&mut self) -> &mut Vec<DiagRecord> {
        &mut self.diag_records
    }
}

impl EnvHandle {
    pub(crate) fn new() -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .inspect_err(|e| {
                error!(%e, "failed to create Tokio runtime");
            })?;
        Ok(Self {
            object_type: HandleType::Env,
            inner: Mutex::new(EnvState {
                diag_records: Vec::new(),
                odbc_version: OdbcVersion::Unset,
                output_nts: true, // SQL_ATTR_OUTPUT_NTS defaults to SQL_TRUE
                connections: Vec::new(),
            }),
            runtime: Arc::new(SharedRuntime::new(runtime)),
        })
    }
}

impl HasObjectType for EnvHandle {
    fn object_type_mut(&mut self) -> &mut HandleType {
        &mut self.object_type
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    const PARK: Duration = Duration::from_secs(5);

    /// Parks a blocking task on `runtime` and returns once it is confirmed
    /// running, so a later drop has in-flight blocking work to contend with.
    fn park_blocking_task(runtime: &SharedRuntime) {
        let (started_tx, started_rx) = mpsc::channel();
        runtime.spawn_blocking(move || {
            let _ = started_tx.send(());
            thread::sleep(PARK);
        });
        started_rx
            .recv_timeout(PARK)
            .expect("blocking task should have started");
    }

    fn assert_released_promptly(what: &str, release: impl FnOnce()) {
        let start = Instant::now();
        release();
        let elapsed = start.elapsed();
        assert!(
            elapsed < PARK / 2,
            "{what} blocked for {elapsed:?} waiting on the runtime's blocking work; \
             shutdown must detach via shutdown_background, not join"
        );
    }

    /// Guards against reintroducing a blocking join. The default `Runtime` drop
    /// waits out in-flight blocking work; `shutdown_background` detaches from
    /// it. An idle runtime joins its idle worker promptly under either one, so
    /// these park a blocking task to tell the two apart. The
    /// OS-already-killed-the-thread case that actually panicked (AB#47509)
    /// needs real process/DLL teardown and isn't reproducible from a unit test.
    #[test]
    fn dropping_env_handle_does_not_wait_for_blocking_work() {
        let env = EnvHandle::new().expect("failed to create EnvHandle for test");
        park_blocking_task(&env.runtime);

        assert_released_promptly("dropping EnvHandle", || drop(env));
    }

    /// The ENV is not always the last owner: `SQLFreeHandle(ENV)` only
    /// `debug_assert!`s that the connection list is empty, so a non-conformant
    /// caller can free the ENV while a DBC still holds a runtime reference.
    /// Shutdown must still detach when that straggler releases the last one.
    #[test]
    fn dropping_a_dbc_that_outlived_its_env_does_not_wait_for_blocking_work() {
        let env = EnvHandle::new().expect("failed to create EnvHandle for test");
        let dbc_runtime = Arc::clone(&env.runtime);
        park_blocking_task(&env.runtime);

        drop(env);

        assert_released_promptly("dropping the last DBC runtime reference", || {
            drop(dbc_runtime)
        });
    }

    /// Pins the precondition the two tests above rely on. They assert that a
    /// drop reaches `shutdown_background`, which only holds while the loader
    /// reports the process is still alive — if this ever read true in-process,
    /// they would pass by taking the leak path instead and would stop testing
    /// anything. The leak path itself needs real `DLL_PROCESS_DETACH` teardown
    /// and is not reproducible from a unit test (AB#47510).
    #[test]
    fn a_live_process_is_not_reported_as_shutting_down() {
        assert!(!process_is_shutting_down());
    }
}
