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
///
/// Declared by hand rather than taken from the `windows` crate this crate
/// already depends on: the flag is an `ntdll` export that the Win32 metadata
/// does not cover, so neither `windows` 0.58 nor `windows-sys` 0.59 exposes it.
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

/// How `SharedRuntime` lets go of its runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleasePolicy {
    /// Signal the scheduler to stop and return without joining. The worker
    /// threads wind themselves down, so nothing accumulates.
    Detach,
    /// Signal nothing at all and leak the runtime. The only safe option once
    /// the loader has terminated the threads the scheduler would synchronize
    /// with; the OS reclaims them moments later.
    Leak,
}

/// Chooses how to let go of the runtime. Takes the loader flag as an argument
/// rather than reading it, so both arms are reachable from a test — a live
/// test process can never observe the shutting-down one.
fn release_policy(process_is_shutting_down: bool) -> ReleasePolicy {
    if process_is_shutting_down {
        ReleasePolicy::Leak
    } else {
        ReleasePolicy::Detach
    }
}

fn release(runtime: Runtime, policy: ReleasePolicy) {
    match policy {
        ReleasePolicy::Detach => runtime.shutdown_background(),
        ReleasePolicy::Leak => std::mem::forget(runtime),
    }
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
/// already going away, [`ReleasePolicy::Leak`] signals nothing at all.
///
/// Outside that window [`ReleasePolicy::Detach`] runs, so an application that
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
        // it by value is what lets `release` either forget the `Runtime` or
        // call `shutdown_background`, both of which consume it; the default
        // drop glue would join instead.
        let runtime = unsafe { ManuallyDrop::take(&mut self.0) };
        release(runtime, release_policy(process_is_shutting_down()));
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

/// Builds the runtime an ENV owns. Extracted from `EnvHandle::new` so the
/// release-policy tests can construct one without an ENV around it.
fn new_runtime() -> io::Result<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .inspect_err(|e| {
            error!(%e, "failed to create Tokio runtime");
        })
}

impl EnvHandle {
    pub(crate) fn new() -> io::Result<Self> {
        let runtime = new_runtime()?;
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

    /// The mapping `Drop` relies on. Guards the condition itself: dropping it
    /// and always returning `Detach` restores the AB#47510 hang, and this is
    /// the only test that can see that, since a live test process never
    /// observes the loader flag as true.
    #[test]
    fn only_a_shutting_down_process_selects_the_leak_policy() {
        assert_eq!(release_policy(true), ReleasePolicy::Leak);
        assert_eq!(release_policy(false), ReleasePolicy::Detach);
    }

    /// The flag feeding the mapping above reads false while the process is
    /// alive, so a normal `SQLFreeHandle(ENV)` really does take `Detach`.
    #[test]
    fn a_live_process_is_not_reported_as_shutting_down() {
        assert!(!process_is_shutting_down());
    }

    /// Runs a task on `handle` and reports whether its scheduler is still
    /// executing work. A detached runtime stops polling, so the task never
    /// runs and the receiver times out; a leaked one keeps its worker alive.
    fn scheduler_still_runs_tasks(handle: &tokio::runtime::Handle) -> bool {
        let (ran_tx, ran_rx) = mpsc::channel();
        handle.spawn(async move {
            let _ = ran_tx.send(());
        });
        ran_rx.recv_timeout(PARK / 2).is_ok()
    }

    /// The regression guard for AB#47510. `Leak` exists so that nothing is
    /// signalled once the loader has terminated the scheduler's threads, and
    /// the only in-process evidence of "signalled nothing" is that the
    /// scheduler is still running afterwards. Swapping this back to
    /// `shutdown_background` — the #459 behaviour that still hung — flips it.
    #[test]
    fn the_leak_policy_signals_nothing_and_leaves_the_runtime_running() {
        let runtime = new_runtime().expect("failed to build runtime for test");
        let handle = runtime.handle().clone();

        release(runtime, ReleasePolicy::Leak);

        assert!(
            scheduler_still_runs_tasks(&handle),
            "Leak must not signal the scheduler; a shut-down runtime stops \
             polling, which is exactly the teardown AB#47510 hangs on"
        );
    }

    /// The other half of the pair: outside process shutdown the runtime really
    /// is released, so an application that allocates and frees environments in
    /// a loop does not accumulate live schedulers.
    #[test]
    fn the_detach_policy_stops_the_runtime() {
        let runtime = new_runtime().expect("failed to build runtime for test");
        let handle = runtime.handle().clone();

        release(runtime, ReleasePolicy::Detach);

        assert!(
            !scheduler_still_runs_tasks(&handle),
            "Detach must actually shut the runtime down, not leak it"
        );
    }
}
