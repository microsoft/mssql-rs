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
/// function's display name used in the panic-caught error log and the trailing
/// trace log.
macro_rules! ffi_entry {
    ($name:literal, $body:expr $(,)?) => {{
        let ret = match ::std::panic::catch_unwind(|| {
            $crate::init_tracing();
            $body
        }) {
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
