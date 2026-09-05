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
/// **Any teardown path that would touch the shared runtime must consult this
/// first.** `SharedRuntime::drop` is not the only one: an ODBC handle freed
/// from a host's `onexit` table cascades into cursor drains, `sp_unprepare`
/// round-trips, and data-at-execution unwinds, each of which reaches
/// `Runtime::block_on`. That is strictly worse than the `shutdown_background`
/// this type avoids — `block_on` parks the calling thread and needs the
/// scheduler's worker to drive the socket, so with that worker already
/// terminated the round-trip can never complete. Every such site is
/// best-effort cleanup the server redoes when the connection drops, so the
/// correct move during shutdown is to skip it (AB#47510).
///
/// Declared by hand rather than taken from the `windows` crate this crate
/// already depends on: the flag is an `ntdll` export that the Win32 metadata
/// does not cover, so neither `windows` 0.58 nor `windows-sys` 0.59 exposes it.
#[cfg(windows)]
pub(crate) fn process_is_shutting_down() -> bool {
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlDllShutdownInProgress() -> u8;
    }
    // SAFETY: `RtlDllShutdownInProgress` is an undocumented but stable `ntdll`
    // export whose signature — `BOOLEAN RtlDllShutdownInProgress(VOID)` — is
    // asserted by the declaration above rather than checked by an import
    // library. It takes no arguments, writes through no out-parameter, and
    // returns a one-byte `BOOLEAN`, so the only way this could be unsound is
    // if that signature were wrong: a mismatched return width would read
    // uninitialised bytes of the return register and make the flag arbitrary,
    // which would leak every runtime rather than corrupt memory. The name is
    // also resolved at load time, so a missing export fails the driver's own
    // load rather than dispatching to the wrong address.
    unsafe { RtlDllShutdownInProgress() != 0 }
}

/// Non-Windows platforms do not terminate the process's other threads before
/// running library teardown, so the runtime can always be used normally.
#[cfg(not(windows))]
pub(crate) fn process_is_shutting_down() -> bool {
    false
}

/// How `SharedRuntime` lets go of its runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleasePolicy {
    /// Drop the runtime normally, waiting for its worker and blocking-pool
    /// threads to exit before returning. Required whenever the process is
    /// alive: `SQLFreeHandle(SQL_HANDLE_ENV)` returning is the host's signal
    /// that it may unload `mssqlodbc.dll`, and a runtime thread still running
    /// Tokio, mio, or driver code from this module when it does is a
    /// use-after-unload (AB#47831).
    Join,
    /// Signal nothing at all and leak the runtime. The only safe option once
    /// the loader has terminated the threads a join would wait on; the OS
    /// reclaims them moments later.
    Leak,
}

/// Chooses how to let go of the runtime. Takes the loader flag as an argument
/// rather than reading it, so both arms are reachable from a test — a live
/// test process can never observe the shutting-down one.
fn release_policy(process_is_shutting_down: bool) -> ReleasePolicy {
    if process_is_shutting_down {
        ReleasePolicy::Leak
    } else {
        ReleasePolicy::Join
    }
}

fn release(runtime: Runtime, policy: ReleasePolicy) {
    match policy {
        ReleasePolicy::Join => drop(runtime),
        ReleasePolicy::Leak => std::mem::forget(runtime),
    }
}

/// A Tokio runtime whose teardown is chosen by whether the process is still
/// alive, because the two states have opposite requirements.
///
/// **Process alive — [`ReleasePolicy::Join`], the default `Runtime` drop.**
/// Returning from `SQLFreeHandle(SQL_HANDLE_ENV)` tells the host it may unload
/// `mssqlodbc.dll`. Any runtime thread still executing Tokio, mio, or driver
/// code statically linked into this module at that point is running code that
/// is about to be unmapped. [`#459`] released the runtime with
/// `shutdown_background()`, which signals the scheduler and returns without
/// waiting, and that produced an intermittent `STATUS_STACK_BUFFER_OVERRUN`
/// while mio's IOCP completion buffer was being destroyed — roughly one crash
/// per 45 runs of a single e2e binary (AB#47831). DLL/thread lifetime is the
/// leading hypothesis for that fault rather than an established mechanism; the
/// captured stack proves only where it lands, not why. Either way, waiting for
/// the threads is what makes the unload safe, so the join is not optional.
///
/// **Process shutting down — [`ReleasePolicy::Leak`].** `SQLFreeHandle` can
/// also run long after Windows has force-terminated every thread but the one
/// calling `ExitProcess`: a host that defers the free to a C++ static
/// destructor or a CRT `onexit` handler reaches it from `DLL_PROCESS_DETACH`
/// inside `LdrShutdownProcess`. Joining there panics with "threads should not
/// terminate unexpectedly" (AB#47509), and `shutdown_background()` is no better
/// — it still takes the scheduler's locks, which a worker terminated
/// mid-instruction may have been holding, hanging the process instead
/// (AB#47510). Nothing may be signalled or waited on, so the runtime is leaked
/// and the OS reclaims its threads moments later. The unload hazard above does
/// not apply: the process is not going to run any more of this module's code.
///
/// This lives on the shared value rather than on `EnvHandle` so the guarantee
/// holds for whichever owner happens to release the last reference. A DBC that
/// outlives its ENV — which a non-conformant caller can produce, since
/// `SQLFreeHandle(ENV)` only `debug_assert!`s that the connection list is empty
/// — would otherwise pick the policy for neither state.
///
/// [`#459`]: https://github.com/microsoft/mssql-rs/pull/459
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
        // it by value is what lets `release` choose between dropping the
        // `Runtime` and forgetting it; the default drop glue would always join.
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    const SETTLE: Duration = Duration::from_secs(5);
    /// Long enough that a release which does not wait will observably return
    /// while the task is still running, short enough not to stall the suite.
    const WORK: Duration = Duration::from_millis(300);

    /// Parks a blocking task that flips `finished` on its way out, and returns
    /// once the task is confirmed running so a later release has genuine
    /// in-flight blocking work to wait for.
    fn park_blocking_task(runtime: &Runtime, finished: Arc<AtomicBool>) {
        let (started_tx, started_rx) = mpsc::channel();
        runtime.spawn_blocking(move || {
            let _ = started_tx.send(());
            thread::sleep(WORK);
            finished.store(true, Ordering::SeqCst);
        });
        started_rx
            .recv_timeout(SETTLE)
            .expect("blocking task should have started");
    }

    /// The regression guard for AB#47831. Returning from `SQLFreeHandle(ENV)`
    /// is the host's cue that it may unload the DLL, so a live-process release
    /// must not return while a runtime thread is still executing code from this
    /// module. `shutdown_background` — the [#459] behaviour this replaces —
    /// signals and returns immediately, so it fails this test: the flag is
    /// still false when the release returns.
    ///
    /// [#459]: https://github.com/microsoft/mssql-rs/pull/459
    #[test]
    fn the_join_policy_waits_for_blocking_work_to_finish() {
        let runtime = new_runtime().expect("failed to build runtime for test");
        let finished = Arc::new(AtomicBool::new(false));
        park_blocking_task(&runtime, Arc::clone(&finished));

        release(runtime, ReleasePolicy::Join);

        assert!(
            finished.load(Ordering::SeqCst),
            "Join returned while a blocking task was still running; the host may \
             unload this DLL as soon as SQLFreeHandle(ENV) returns (AB#47831)"
        );
    }

    /// The same guarantee through the real teardown path, so the policy cannot
    /// be right while `Drop` fails to reach it.
    #[test]
    fn dropping_env_handle_waits_for_blocking_work_to_finish() {
        let env = EnvHandle::new().expect("failed to create EnvHandle for test");
        let finished = Arc::new(AtomicBool::new(false));
        park_blocking_task(&env.runtime, Arc::clone(&finished));

        drop(env);

        assert!(
            finished.load(Ordering::SeqCst),
            "dropping EnvHandle returned while a blocking task was still running"
        );
    }

    /// The ENV is not always the last owner: `SQLFreeHandle(ENV)` only
    /// `debug_assert!`s that the connection list is empty, so a non-conformant
    /// caller can free the ENV while a DBC still holds a runtime reference. The
    /// wait has to happen whenever the *last* reference goes, not whenever the
    /// ENV does.
    #[test]
    fn dropping_a_dbc_that_outlived_its_env_waits_for_blocking_work_to_finish() {
        let env = EnvHandle::new().expect("failed to create EnvHandle for test");
        let dbc_runtime = Arc::clone(&env.runtime);
        let finished = Arc::new(AtomicBool::new(false));
        park_blocking_task(&env.runtime, Arc::clone(&finished));

        drop(env);
        assert!(
            !finished.load(Ordering::SeqCst),
            "dropping the ENV must not have released the runtime; the DBC still holds a reference"
        );

        drop(dbc_runtime);

        assert!(
            finished.load(Ordering::SeqCst),
            "dropping the last DBC runtime reference returned while a blocking task was running"
        );
    }

    /// The mapping `Drop` relies on. Guards the condition itself: dropping it
    /// and always returning `Join` restores the AB#47510 hang, and this is the
    /// only test that can see that, since a live test process never observes
    /// the loader flag as true.
    #[test]
    fn only_a_shutting_down_process_selects_the_leak_policy() {
        assert_eq!(release_policy(true), ReleasePolicy::Leak);
        assert_eq!(release_policy(false), ReleasePolicy::Join);
    }

    /// The flag feeding the mapping above reads false while the process is
    /// alive, so a normal `SQLFreeHandle(ENV)` really does take `Join`.
    #[test]
    fn a_live_process_is_not_reported_as_shutting_down() {
        assert!(!process_is_shutting_down());
    }

    /// Runs a task on `handle` and reports whether its scheduler is still
    /// executing work. A released runtime stops polling, so the task never
    /// runs and the receiver times out; a leaked one keeps its worker alive.
    fn scheduler_still_runs_tasks(handle: &tokio::runtime::Handle) -> bool {
        let (ran_tx, ran_rx) = mpsc::channel();
        handle.spawn(async move {
            let _ = ran_tx.send(());
        });
        ran_rx.recv_timeout(SETTLE / 2).is_ok()
    }

    /// The regression guard for AB#47510. `Leak` exists so that nothing is
    /// signalled or waited on once the loader has terminated the scheduler's
    /// threads, and the only in-process evidence of "touched nothing" is that
    /// the scheduler is still running afterwards. Swapping this to either
    /// `Join` or `shutdown_background` flips it.
    #[test]
    fn the_leak_policy_touches_nothing_and_leaves_the_runtime_running() {
        let runtime = new_runtime().expect("failed to build runtime for test");
        let handle = runtime.handle().clone();

        release(runtime, ReleasePolicy::Leak);

        assert!(
            scheduler_still_runs_tasks(&handle),
            "Leak must not touch the scheduler; a released runtime stops polling, \
             which is exactly the teardown AB#47510 hangs on"
        );
    }
}
