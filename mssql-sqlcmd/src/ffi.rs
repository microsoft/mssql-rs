// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! C ABI for native sqlcmd.
//!
//! The header for C and C++ callers is `include/mssql_sqlcmd.h`.

/// The crate version, NUL-terminated, so a caller can confirm which build of
/// the library it linked.
static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// Returns the library's version as a NUL-terminated UTF-8 string, e.g.
/// `0.1.0`. The string is static: the caller must not free it.
#[unsafe(no_mangle)]
pub extern "C" fn mssql_sqlcmd_version() -> *const std::ffi::c_char {
    VERSION.as_ptr().cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_the_crate_version() {
        // SAFETY: `mssql_sqlcmd_version` returns a static NUL-terminated string.
        let version = unsafe { std::ffi::CStr::from_ptr(mssql_sqlcmd_version()) };
        assert_eq!(version.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
    }
}