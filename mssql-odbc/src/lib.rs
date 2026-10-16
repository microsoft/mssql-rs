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
                $entry;
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
