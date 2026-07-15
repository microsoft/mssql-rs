// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Once;

pub mod api;
mod auth;
mod connection;
mod error;
mod handles;
#[cfg(test)]
pub(crate) mod test_support;

static INIT_TRACING: Once = Once::new();

const ENV_TRACE: &str = "MSSQL_TDS_TRACE";
const ENV_TRACE_LEVEL: &str = "MSSQL_TDS_TRACE_LEVEL";
const DEFAULT_TRACE_LEVEL: &str = "warn";

pub(crate) fn init_tracing() {
    INIT_TRACING.call_once(|| {
        let enabled = std::env::var(ENV_TRACE)
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if !enabled {
            return;
        }

        let level = std::env::var(ENV_TRACE_LEVEL).unwrap_or_else(|_| DEFAULT_TRACE_LEVEL.into());
        let filter = tracing_subscriber::EnvFilter::try_new(level.as_str()).unwrap_or_else(|err| {
            eprintln!(
                "[mssql-odbc] ERROR: Invalid {ENV_TRACE_LEVEL} value '{level}': {err}. Falling back to '{DEFAULT_TRACE_LEVEL}'."
            );
            tracing_subscriber::EnvFilter::new(DEFAULT_TRACE_LEVEL)
        });

        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .try_init();
    });
}

/// Wraps an FFI entry-point body in `catch_unwind` so a Rust panic crossing
/// the C ABI is converted into `SQL_ERROR` rather than unwinding into C
/// (which is undefined behaviour). `$name` is the SQL function's display name
/// used in the panic-caught error log and the trailing trace log.
///
/// Caller is responsible for `crate::init_tracing()` — already done at the
/// export-layer call site in `api::exports`.
macro_rules! ffi_entry {
    ($name:literal, $body:expr $(,)?) => {{
        let ret = match ::std::panic::catch_unwind(|| $body) {
            ::std::result::Result::Ok(rc) => rc,
            ::std::result::Result::Err(_) => {
                ::tracing::error!(concat!($name, ": panic caught at FFI boundary"));
                $crate::api::odbc_types::SQL_ERROR
            }
        };
        ::tracing::trace!(?ret, concat!($name, " returning"));
        ret
    }};
}
pub(crate) use ffi_entry;
