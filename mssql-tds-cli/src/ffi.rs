// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! C ABI, so the native ODBC `sqlcmd` can hand an invocation to this
//! implementation.
//!
//! The boundary is two functions and nothing else. [`sqlcmd_modern_claims`]
//! answers whether this side owns the command line; [`sqlcmd_modern_main`]
//! runs it and returns the exit code. No Rust type crosses, and no allocation
//! changes hands, so the two halves can be linked into one binary without
//! agreeing on anything but `argv`.
//!
//! The native tool's entry point is `main(char**)` on Unix but `wmain(WCHAR**)`
//! on Windows, so the pair is mirrored as `*_w` taking UTF-16. Both narrow to
//! the same `Vec<String>` and share every decision from there.
//!
//! What is claimed is deliberately narrow: the go-sqlcmd subcommand verbs, and
//! the long options that have no short form in the ODBC option string. A
//! command line the ODBC tool could have parsed is never taken.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

use crate::{cli::spec, modern};

/// Whether this implementation owns `argv`.
fn claims(args: &[String]) -> bool {
    if modern::claims(args) {
        return true;
    }
    args.iter().any(|arg| {
        arg.strip_prefix("--")
            .is_some_and(|rest| spec::is_long_only(rest.split('=').next().unwrap_or(rest)))
    })
}

/// Copies `argc`/`argv` into owned strings, skipping the program name.
///
/// Returns `None` if the vector is malformed or any argument is not UTF-8,
/// which callers treat as "not ours" rather than failing.
///
/// # Safety
/// `argv` must point to `argc` NUL-terminated strings, as `main` receives them.
unsafe fn collect(argc: c_int, argv: *const *const c_char) -> Option<Vec<String>> {
    if argv.is_null() || argc < 1 {
        return None;
    }

    let mut args = Vec::with_capacity((argc as usize).saturating_sub(1));
    // Skip argv[0]: the rest of the crate expects arguments without it.
    for i in 1..argc as isize {
        // SAFETY: the caller guarantees `argc` valid entries.
        let p = unsafe { *argv.offset(i) };
        if p.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees each entry is NUL-terminated.
        args.push(unsafe { CStr::from_ptr(p) }.to_str().ok()?.to_owned());
    }
    Some(args)
}

/// Non-zero when this implementation owns the command line, zero to let the
/// legacy path run unchanged.
///
/// # Safety
/// `argv` must point to `argc` NUL-terminated strings, as `main` receives them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlcmd_modern_claims(argc: c_int, argv: *const *const c_char) -> c_int {
    // SAFETY: forwarding the caller's own guarantee.
    match unsafe { collect(argc, argv) } {
        Some(args) => c_int::from(claims(&args)),
        None => 0,
    }
}

/// Runs the invocation and returns the process exit code.
///
/// Only meaningful once [`sqlcmd_modern_claims`] has returned non-zero.
///
/// # Safety
/// `argv` must point to `argc` NUL-terminated strings, as `main` receives them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlcmd_modern_main(argc: c_int, argv: *const *const c_char) -> c_int {
    // SAFETY: forwarding the caller's own guarantee.
    match unsafe { collect(argc, argv) } {
        Some(args) => crate::run(args),
        None => crate::exitcode::FAILURE,
    }
}

/// UTF-16 counterpart of [`collect`], for the `wmain` the Windows tool uses.
///
/// # Safety
/// `argv` must point to `argc` NUL-terminated wide strings.
#[cfg(windows)]
unsafe fn collect_wide(argc: c_int, argv: *const *const u16) -> Option<Vec<String>> {
    if argv.is_null() || argc < 1 {
        return None;
    }

    let mut args = Vec::with_capacity((argc as usize).saturating_sub(1));
    for i in 1..argc as isize {
        // SAFETY: the caller guarantees `argc` valid entries.
        let p = unsafe { *argv.offset(i) };
        if p.is_null() {
            return None;
        }
        let mut len = 0;
        // SAFETY: the caller guarantees the run is NUL-terminated.
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` units were just walked, so the slice is in bounds.
        let units = unsafe { std::slice::from_raw_parts(p, len) };
        args.push(String::from_utf16(units).ok()?);
    }
    Some(args)
}

/// Wide counterpart of [`sqlcmd_modern_claims`].
///
/// # Safety
/// `argv` must point to `argc` NUL-terminated wide strings, as `wmain`
/// receives them.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlcmd_modern_claims_w(argc: c_int, argv: *const *const u16) -> c_int {
    // SAFETY: forwarding the caller's own guarantee.
    match unsafe { collect_wide(argc, argv) } {
        Some(args) => c_int::from(claims(&args)),
        None => 0,
    }
}

/// Wide counterpart of [`sqlcmd_modern_main`].
///
/// # Safety
/// `argv` must point to `argc` NUL-terminated wide strings, as `wmain`
/// receives them.
#[cfg(windows)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlcmd_modern_main_w(argc: c_int, argv: *const *const u16) -> c_int {
    // SAFETY: forwarding the caller's own guarantee.
    match unsafe { collect_wide(argc, argv) } {
        Some(args) => crate::run(args),
        None => crate::exitcode::FAILURE,
    }
}
