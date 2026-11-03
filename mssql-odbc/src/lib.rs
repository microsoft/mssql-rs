// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod api;
mod auth;
mod connection;
mod conversion;
mod error;
mod handles;
mod params;
mod tracing_init;

// Internal parse/convert helpers re-exposed as safe wrappers for coverage-guided
// fuzzing (see `fuzz/`). The `fuzzing` cfg is set only by cargo-fuzz, so the
// shipped driver's FFI surface is unchanged.
#[cfg(fuzzing)]
pub mod fuzz_support;

#[cfg(test)]
pub(crate) mod test_support;

pub(crate) use tracing_init::init_tracing;

/// Initializes tracing and wraps an FFI entry-point body in `catch_unwind` so
/// a Rust panic crossing the C ABI is converted into `SQL_ERROR` rather than
/// unwinding into C (which is undefined behaviour). `$name` is the SQL
/// function's display name. The entry event runs after correlation is installed;
/// the scope outlives panic conversion and the return event, even when the body
/// frees the handle. A disabled sink never resolves the handle or enters TLS.
///
/// The untraced branch evaluates `$entry` only if a sink was installed after the
/// branch was chosen, normally by this call's `init_tracing`, so the first
/// traced call still logs its entry inside the panic boundary. The event goes
/// through `cold_entry` to keep its expansion out of the hot closure, which
/// otherwise stops inlining into the API and adds a frame to every untraced call.
macro_rules! ffi_entry {
    ($name:literal, $handle:expr, $entry:expr, $body:expr $(,)?) => {{
        if ::mssql_tds::trace_context::enabled() {
            let body = || {
                $entry;
                $body
            };
            // SAFETY: forwarded from the enclosing ODBC API's handle contract.
            unsafe {
                $crate::traced_entry($name, $handle, body, |result| {
                    $crate::ffi_entry!(@finish $name, result)
                })
            }
        } else {
            let result = ::std::panic::catch_unwind(|| {
                $crate::init_tracing();
                if ::mssql_tds::trace_context::enabled() {
                    $crate::cold_entry(|| $entry);
                }
                $body
            });
            $crate::ffi_entry!(@finish $name, result)
        }
    }};
    (@finish $name:literal, $result:expr) => {{
        let ret = match $result {
            ::std::result::Result::Ok(rc) => rc,
            ::std::result::Result::Err(_) => {
                ::tracing::error!(concat!($name, ": panic caught at FFI boundary"));
                $crate::api::odbc_types::SQL_ERROR
            }
        };
        if $crate::tracing_init::starts_execution($name) {
            ::tracing::debug!(?ret, concat!($name, " returning"));
        } else {
            ::tracing::trace!(?ret, concat!($name, " returning"));
        }
        ret
    }};
}
pub(crate) use ffi_entry;

#[cold]
#[inline(never)]
fn cold_entry(entry: impl FnOnce()) {
    entry();
}

// Separating the guard's unwind cleanup from the disabled path avoids inflating
// the few-nanosecond buffered/attribute APIs when no trace sink is installed.
// Windows release SQLGetEnvAttr: 11.3 ns baseline, about 13 ns with this split,
// versus 17.0 ns with the optional guard inside the common catch_unwind.
///
/// # Safety
/// A live `handle` and its parent must remain allocated until `body` starts;
/// the body may free them under its own ODBC lifetime contract.
#[cold]
unsafe fn traced_entry(
    name: &'static str,
    handle: api::odbc_types::SqlHandle,
    body: impl FnOnce() -> api::odbc_types::SqlReturn,
    finish: impl FnOnce(std::thread::Result<api::odbc_types::SqlReturn>) -> api::odbc_types::SqlReturn,
) -> api::odbc_types::SqlReturn {
    let mut scope = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        init_tracing();
        if !handles::process_is_shutting_down() {
            scope = Some(unsafe { tracing_init::context_for(handle, name) }.enter());
        }
        body()
    }));
    let ret = finish(result);
    drop(scope);
    ret
}

#[cfg(test)]
mod tests {
    use crate::api::odbc_types::{SQL_ERROR, SQL_SUCCESS};
    use std::sync::atomic::{AtomicBool, Ordering};

    const CHILD: &str = "MSSQLODBC_FFI_ENTRY_CHILD";

    /// Reruns `test` in a fresh process, where tracing is not yet initialised,
    /// and returns the child's stderr. `traced` installs the stderr sink.
    fn run_in_child(test: &str, traced: bool) -> String {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .env("MSSQL_TDS_TRACE_LEVEL", "trace")
            .env_remove("MSSQL_TDS_TRACE_DIR");
        if traced {
            command.env("MSSQL_TDS_TRACE", "true");
        } else {
            command.env_remove("MSSQL_TDS_TRACE");
        }
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "child failed: {stderr}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        stderr
    }

    struct PanicsOnDebug;

    impl std::fmt::Debug for PanicsOnDebug {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("entry formatter panic")
        }
    }

    #[test]
    fn the_call_that_initialises_tracing_logs_its_entry_event() {
        if std::env::var_os(CHILD).is_some() {
            assert!(!mssql_tds::trace_context::enabled());
            let ret = crate::ffi_entry!(
                "FirstCall",
                std::ptr::null_mut(),
                tracing::debug!("FirstCall called"),
                SQL_SUCCESS
            );
            assert_eq!(ret, SQL_SUCCESS);
            assert!(mssql_tds::trace_context::enabled());
            return;
        }
        let stderr = run_in_child(
            "tests::the_call_that_initialises_tracing_logs_its_entry_event",
            true,
        );
        let entry = stderr.find("FirstCall called").expect(&stderr);
        let finish = stderr.find("FirstCall returning").expect(&stderr);
        assert!(entry < finish);
    }

    #[test]
    fn a_panicking_entry_event_on_the_first_traced_call_returns_sql_error() {
        if std::env::var_os(CHILD).is_some() {
            let body_ran = AtomicBool::new(false);
            let ret = crate::ffi_entry!(
                "PanicEntry",
                std::ptr::null_mut(),
                tracing::debug!(arg = ?PanicsOnDebug, "PanicEntry called"),
                {
                    body_ran.store(true, Ordering::Relaxed);
                    SQL_SUCCESS
                }
            );
            assert_eq!(ret, SQL_ERROR);
            assert!(!body_ran.load(Ordering::Relaxed));
            return;
        }
        let stderr = run_in_child(
            "tests::a_panicking_entry_event_on_the_first_traced_call_returns_sql_error",
            true,
        );
        assert!(stderr.contains("entry formatter panic"), "{stderr}");
        assert!(
            stderr.contains("PanicEntry: panic caught at FFI boundary"),
            "{stderr}"
        );
    }

    #[test]
    fn untraced_calls_do_not_evaluate_the_entry_event() {
        if std::env::var_os(CHILD).is_some() {
            let entered = AtomicBool::new(false);
            let ret = crate::ffi_entry!(
                "Untraced",
                std::ptr::null_mut(),
                entered.store(true, Ordering::Relaxed),
                SQL_SUCCESS
            );
            assert_eq!(ret, SQL_SUCCESS);
            assert!(!entered.load(Ordering::Relaxed));
            assert!(!mssql_tds::trace_context::enabled());
            return;
        }
        run_in_child(
            "tests::untraced_calls_do_not_evaluate_the_entry_event",
            false,
        );
    }
}
