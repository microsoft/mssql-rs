// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenSSL, loaded when it is first used rather than linked.
//!
//! On Linux, mssql-tds reaches OpenSSL through `openssl-sys` for TLS (native-tls)
//! and for its Always Encrypted primitives. Linked, the library would tie native
//! sqlcmd to the OpenSSL major version of the machine that built it: a library
//! built against OpenSSL 1.1 headers calls functions OpenSSL 3 and 4 no longer
//! export (`EVP_CIPHER_iv_length`, ...), one built against OpenSSL 3 calls
//! functions 1.1 lacks, and the executable needs that `libssl.so.N` to start at
//! all.
//!
//! This module defines every OpenSSL function the library calls. The first call
//! loads `libssl.so.4`, else `libssl.so.3`, else `libssl.so.1.1` (the order the
//! ODBC driver's DynamicOpenSSL uses, so both share one OpenSSL in a process),
//! and each function forwards to the loaded library's, under whichever of its
//! 1.1 or 3 names that library exports (OpenSSL 4 exports the 3 names). The
//! static library then needs no `-lssl` or `-lcrypto`, and the same build links
//! and runs with any of these versions.
//!
//! - No OpenSSL installed: every function returns its failure value (null, 0),
//!   so the `openssl` crate reports an error and only TLS is unavailable; the
//!   reason is written to stderr once.
//! - OpenSSL loaded but a function missing: the process aborts. A missing
//!   function must never pass for a successful call (a certificate check, say).
//!
//! The set of functions is checked when the package is built: the archive must
//! link without OpenSSL (see `.pipeline/scripts/build-mssql-sqlcmd-native.sh`).
//! A new OpenSSL call in a dependency fails that check until it is added here.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, Once, OnceLock, PoisonError};

/// The libraries tried, in order: OpenSSL 4, 3, then 1.1.
const LIBRARIES: [&CStr; 3] = [c"libssl.so.4", c"libssl.so.3", c"libssl.so.1.1"];

/// The loaded `libssl` (its dependency `libcrypto` comes with it), or null when
/// neither is installed. Silent: see [`library`] for the report.
fn load() -> *mut c_void {
    static HANDLE: OnceLock<usize> = OnceLock::new();
    *HANDLE.get_or_init(|| {
        for name in LIBRARIES {
            // SAFETY: a NUL-terminated name. RTLD_LOCAL keeps OpenSSL's symbols
            // out of the global scope, as the ODBC driver loads it. RTLD_NOW
            // (the driver uses RTLD_LAZY) binds every function at load, so a
            // library that cannot is skipped here instead of failing a later call.
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                return handle as usize;
            }
        }
        0
    }) as *mut c_void
}

/// The loaded `libssl`, or null when none is installed, in which case the
/// reason is written to stderr, once, because a caller that needs OpenSSL is
/// about to fail.
fn library() -> *mut c_void {
    let handle = load();
    if handle.is_null() {
        static REPORTED: Once = Once::new();
        REPORTED.call_once(|| eprintln!("mssql-sqlcmd: {}", not_found_message()));
    }
    handle
}

/// Whether an OpenSSL is installed, without the report [`library`] writes, for
/// callers that can do without it and must not add output of their own.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn is_available() -> bool {
    !load().is_null()
}

/// Why TLS is unavailable when no OpenSSL is installed, in the user's language.
fn not_found_message() -> String {
    let libraries = LIBRARIES.map(|name| name.to_string_lossy()).join(", ");
    crate::i18n::tr("openssl.not_found", &[("libraries", &libraries)])
}

/// Why the process stops when the loaded OpenSSL lacks `function`, in the
/// user's language.
fn unsupported_message(function: &CStr) -> String {
    crate::i18n::tr(
        "openssl.unsupported",
        &[("function", &function.to_string_lossy())],
    )
}

/// The function `names` (the first the library exports) resolves to, cached in
/// `slot`; null when no OpenSSL is installed. A library that exports none of
/// them is not one this build supports: the process aborts.
fn resolve(slot: &AtomicPtr<c_void>, names: &[&CStr]) -> *mut c_void {
    let cached = slot.load(Ordering::Acquire);
    if !cached.is_null() {
        return cached;
    }
    let handle = library();
    if handle.is_null() {
        return null_mut();
    }
    resolve_in(handle, slot, names)
}

/// The function `names` (the first `handle` exports) resolves to, cached in
/// `slot`. A library that exports none of them is not one this build supports:
/// the process aborts rather than pass a missing function off as a call.
fn resolve_in(handle: *mut c_void, slot: &AtomicPtr<c_void>, names: &[&CStr]) -> *mut c_void {
    for name in names {
        // SAFETY: a handle dlopen returned and a NUL-terminated name.
        let function = unsafe { libc::dlsym(handle, name.as_ptr()) };
        if !function.is_null() {
            slot.store(function, Ordering::Release);
            return function;
        }
    }
    eprintln!("mssql-sqlcmd: {}", unsupported_message(names[0]));
    std::process::abort();
}

/// Defines each function as a forwarder: `name[other names](args) -> ret =>
/// failure`, where the other names are the same function's under the other
/// OpenSSL major version and `failure` is returned when no OpenSSL is installed.
macro_rules! forward {
    ($( $name:ident [$($alt:ident),*] ($($arg:ident: $ty:ty),*) $(-> $ret:ty)? => $fail:expr; )*) => {$(
        #[unsafe(no_mangle)]
        #[allow(non_snake_case, clippy::missing_safety_doc, clippy::unused_unit)]
        pub unsafe extern "C" fn $name($($arg: $ty),*) $(-> $ret)? {
            static SLOT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
            let function = resolve(&SLOT, &[
                // SAFETY: a literal with one trailing NUL.
                unsafe { CStr::from_bytes_with_nul_unchecked(concat!(stringify!($name), "\0").as_bytes()) },
                $(unsafe { CStr::from_bytes_with_nul_unchecked(concat!(stringify!($alt), "\0").as_bytes()) },)*
            ]);
            if function.is_null() {
                return $fail;
            }
            // SAFETY: the library's function of this name, whose C signature
            // these argument and return types match.
            let function: unsafe extern "C" fn($($ty),*) $(-> $ret)? =
                unsafe { std::mem::transmute::<*mut c_void, _>(function) };
            unsafe { function($($arg),*) }
        }
    )*};
}

type P = *mut c_void;

forward! {
    // BIO
    BIO_clear_flags[](b: P, flags: c_int) => ();
    BIO_free_all[](b: P) => ();
    BIO_get_data[](a: P) -> P => null_mut();
    BIO_meth_free[](biom: P) => ();
    BIO_meth_new[](type_: c_int, name: *const c_char) -> P => null_mut();
    BIO_meth_set_create[](biom: P, create: P) -> c_int => 0;
    BIO_meth_set_ctrl[](biom: P, ctrl: P) -> c_int => 0;
    BIO_meth_set_destroy[](biom: P, destroy: P) -> c_int => 0;
    BIO_meth_set_puts[](biom: P, puts: P) -> c_int => 0;
    BIO_meth_set_read[](biom: P, read: P) -> c_int => 0;
    BIO_meth_set_write[](biom: P, write: P) -> c_int => 0;
    BIO_new[](type_: P) -> P => null_mut();
    BIO_new_mem_buf[](buf: P, len: c_int) -> P => null_mut();
    BIO_set_data[](a: P, data: P) => ();
    BIO_set_flags[](b: P, flags: c_int) => ();
    BIO_set_init[](a: P, init: c_int) => ();

    // Errors. Without OpenSSL there are none to report. ERR_get_error_line_data
    // and ERR_get_error_all are below.
    CRYPTO_get_ex_new_index[](class_index: c_int, argl: c_long, argp: P, new_func: P, dup_func: P, free_func: P) -> c_int => -1;
    ERR_func_error_string[](err: c_ulong) -> *const c_char => std::ptr::null();
    ERR_lib_error_string[](err: c_ulong) -> *const c_char => std::ptr::null();
    ERR_reason_error_string[](err: c_ulong) -> *const c_char => std::ptr::null();

    // EVP: the Always Encrypted primitives. OpenSSL 3 renamed the size and flag
    // accessors (EVP_CIPHER_iv_length is EVP_CIPHER_get_iv_length) and exports
    // only the new names, as 4 does; 1.1 has only the old ones.
    EVP_CIPHER_CTX_block_size[EVP_CIPHER_CTX_get_block_size](ctx: P) -> c_int => 0;
    EVP_CIPHER_CTX_get_block_size[EVP_CIPHER_CTX_block_size](ctx: P) -> c_int => 0;
    EVP_CIPHER_CTX_iv_length[EVP_CIPHER_CTX_get_iv_length](ctx: P) -> c_int => 0;
    EVP_CIPHER_CTX_get_iv_length[EVP_CIPHER_CTX_iv_length](ctx: P) -> c_int => 0;
    EVP_CIPHER_CTX_key_length[EVP_CIPHER_CTX_get_key_length](ctx: P) -> c_int => 0;
    EVP_CIPHER_CTX_get_key_length[EVP_CIPHER_CTX_key_length](ctx: P) -> c_int => 0;
    EVP_CIPHER_CTX_cipher[EVP_CIPHER_CTX_get0_cipher](ctx: P) -> P => null_mut();
    EVP_CIPHER_CTX_get0_cipher[EVP_CIPHER_CTX_cipher](ctx: P) -> P => null_mut();
    EVP_CIPHER_CTX_ctrl[](ctx: P, type_: c_int, arg: c_int, ptr: P) -> c_int => 0;
    EVP_CIPHER_CTX_free[](ctx: P) => ();
    EVP_CIPHER_CTX_new[]() -> P => null_mut();
    EVP_CIPHER_CTX_set_key_length[](ctx: P, keylen: c_int) -> c_int => 0;
    EVP_CIPHER_block_size[EVP_CIPHER_get_block_size](cipher: P) -> c_int => 0;
    EVP_CIPHER_get_block_size[EVP_CIPHER_block_size](cipher: P) -> c_int => 0;
    EVP_CIPHER_flags[EVP_CIPHER_get_flags](cipher: P) -> c_ulong => 0;
    EVP_CIPHER_get_flags[EVP_CIPHER_flags](cipher: P) -> c_ulong => 0;
    EVP_CIPHER_iv_length[EVP_CIPHER_get_iv_length](cipher: P) -> c_int => 0;
    EVP_CIPHER_get_iv_length[EVP_CIPHER_iv_length](cipher: P) -> c_int => 0;
    EVP_CipherFinal[](ctx: P, res: *mut u8, len: *mut c_int) -> c_int => 0;
    EVP_CipherUpdate[](ctx: P, outbuf: *mut u8, outlen: *mut c_int, inbuf: *const u8, inlen: c_int) -> c_int => 0;
    EVP_DecryptInit_ex[](ctx: P, cipher: P, impl_: P, key: *const u8, iv: *const u8) -> c_int => 0;
    EVP_DigestSign[](ctx: P, sigret: *mut u8, siglen: *mut usize, tbs: *const u8, tbslen: usize) -> c_int => 0;
    EVP_DigestSignFinal[](ctx: P, sig: *mut u8, siglen: *mut usize) -> c_int => 0;
    EVP_DigestSignInit[](ctx: P, pctx: P, type_: P, e: P, pkey: P) -> c_int => 0;
    EVP_DigestUpdate[](ctx: P, data: P, n: usize) -> c_int => 0;
    EVP_MD_CTX_free[](ctx: P) => ();
    EVP_MD_CTX_new[]() -> P => null_mut();
    EVP_PKEY_free[](k: P) => ();
    EVP_PKEY_new_mac_key[](type_: c_int, e: P, key: *const u8, keylen: c_int) -> P => null_mut();
    EVP_aes_256_cbc[]() -> P => null_mut();
    EVP_sha256[]() -> P => null_mut();

    // SSL and X509: TLS through native-tls.
    OPENSSL_init_ssl[](opts: u64, settings: P) -> c_int => 0;
    OpenSSL_version_num[]() -> c_ulong => 0;
    TLS_method[]() -> P => null_mut();
    PEM_read_bio_X509[](bio: P, out: P, callback: P, user_data: P) -> P => null_mut();
    SSL_CTX_ctrl[](ctx: P, cmd: c_int, larg: c_long, parg: P) -> c_long => 0;
    SSL_CTX_free[](ctx: P) => ();
    SSL_CTX_load_verify_locations[](ctx: P, ca_file: *const c_char, ca_path: *const c_char) -> c_int => 0;
    SSL_CTX_new[](method: P) -> P => null_mut();
    SSL_CTX_set_alpn_protos[](s: P, data: *const u8, len: c_uint) -> c_int => 1;
    SSL_CTX_set_cipher_list[](ssl: P, s: *const c_char) -> c_int => 0;
    SSL_CTX_set_default_verify_paths[](ctx: P) -> c_int => 0;
    SSL_CTX_set_options[](ctx: P, op: u64) -> u64 => 0;
    SSL_CTX_set_verify[](ctx: P, mode: c_int, verify_callback: P) => ();
    SSL_CTX_up_ref[](x: P) -> c_int => 0;
    SSL_connect[](ssl: P) -> c_int => -1;
    SSL_ctrl[](ssl: P, cmd: c_int, larg: c_long, parg: P) -> c_long => 0;
    SSL_do_handshake[](ssl: P) -> c_int => -1;
    SSL_free[](ssl: P) => ();
    SSL_get0_alpn_selected[](s: P, data: P, len: *mut c_uint) => ();
    SSL_get0_param[](ssl: P) -> P => null_mut();
    SSL_get_peer_certificate[SSL_get1_peer_certificate](ssl: P) -> P => null_mut();
    SSL_get1_peer_certificate[SSL_get_peer_certificate](ssl: P) -> P => null_mut();
    SSL_get_error[](ssl: P, ret: c_int) -> c_int => 1;
    SSL_get_ex_data[](ssl: P, idx: c_int) -> P => null_mut();
    SSL_get_rbio[](ssl: P) -> P => null_mut();
    // Fail closed: 1 is X509_V_ERR_UNSPECIFIED, never X509_V_OK (0).
    SSL_get_verify_result[](ssl: P) -> c_long => 1;
    SSL_new[](ctx: P) -> P => null_mut();
    SSL_read_ex[](ssl: P, buf: P, num: usize, readbytes: *mut usize) -> c_int => 0;
    SSL_set_bio[](ssl: P, rbio: P, wbio: P) => ();
    SSL_set_ex_data[](ssl: P, idx: c_int, data: P) -> c_int => 0;
    SSL_set_verify[](ssl: P, mode: c_int, verify_callback: P) => ();
    SSL_shutdown[](ssl: P) -> c_int => -1;
    SSL_write_ex[](ssl: P, buf: P, num: usize, written: *mut usize) -> c_int => 0;
    X509_VERIFY_PARAM_set1_host[](param: P, name: *const c_char, namelen: usize) -> c_int => 0;
    X509_VERIFY_PARAM_set1_ip[](param: P, ip: *const u8, iplen: usize) -> c_int => 0;
    X509_VERIFY_PARAM_set_hostflags[](param: P, flags: c_uint) => ();
    X509_free[](x: P) => ();
    X509_verify_cert_error_string[](n: c_long) -> *const c_char => std::ptr::null();
    d2i_X509[](a: P, pp: P, length: c_long) -> P => null_mut();
    i2d_X509[](x: P, buf: P) -> c_int => -1;
}

/// OpenSSL's `ERR_get_error_line_data`, which an `openssl` crate built against
/// 1.1 headers calls, with the source file name it returns made permanent.
/// That crate keeps the name as `&'static str`, as 1.1 returns string
/// literals; OpenSSL 3 and 4 return their own copy, which the error queue frees
/// when the slot is reused or the queue is cleared.
#[unsafe(no_mangle)]
#[allow(non_snake_case, clippy::missing_safety_doc)]
pub unsafe extern "C" fn ERR_get_error_line_data(
    file: *mut *const c_char,
    line: *mut c_int,
    data: P,
    flags: *mut c_int,
) -> c_ulong {
    static SLOT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
    let function = resolve(&SLOT, &[c"ERR_get_error_line_data"]);
    if function.is_null() {
        return 0;
    }
    // SAFETY: OpenSSL's ERR_get_error_line_data, whose signature this matches.
    let function: unsafe extern "C" fn(*mut *const c_char, *mut c_int, P, *mut c_int) -> c_ulong =
        unsafe { std::mem::transmute::<*mut c_void, _>(function) };
    let code = unsafe { function(file, line, data, flags) };
    // With no error queued OpenSSL returns 0 without setting `*file`.
    if code != 0 && !file.is_null() {
        // SAFETY: the caller's out-pointer, which OpenSSL just set.
        unsafe { *file = permanent(*file) };
    }
    code
}

/// A copy of the NUL-terminated `name` that lives as long as the process.
/// Each distinct name is copied once: they are OpenSSL's (and its providers')
/// source file names, a small fixed set.
fn permanent(name: *const c_char) -> *const c_char {
    static NAMES: Mutex<BTreeSet<&'static CStr>> = Mutex::new(BTreeSet::new());
    if name.is_null() {
        return name;
    }
    // SAFETY: OpenSSL returns a NUL-terminated string, valid until the queue
    // changes, which it cannot before this returns.
    let name = unsafe { CStr::from_ptr(name) };
    let mut names = NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(kept) = names.get(name) {
        return kept.as_ptr();
    }
    let kept: &'static CStr = Box::leak(CString::from(name).into_boxed_c_str());
    names.insert(kept);
    kept.as_ptr()
}

/// OpenSSL 3's `ERR_get_error_all`, which 1.1 lacks: there, the same error
/// from `ERR_get_error_line_data`, with no function name (1.1 records none
/// the 3 way either).
#[unsafe(no_mangle)]
#[allow(non_snake_case, clippy::missing_safety_doc)]
pub unsafe extern "C" fn ERR_get_error_all(
    file: P,
    line: *mut c_int,
    func: *mut *const c_char,
    data: P,
    flags: *mut c_int,
) -> c_ulong {
    static SLOT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
    static SLOT_1_1: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
    let handle = library();
    if handle.is_null() {
        return 0;
    }
    let mut all = SLOT.load(Ordering::Acquire);
    if all.is_null() {
        // SAFETY: a handle dlopen returned and a NUL-terminated name.
        all = unsafe { libc::dlsym(handle, c"ERR_get_error_all".as_ptr()) };
        SLOT.store(all, Ordering::Release);
    }
    if !all.is_null() {
        // SAFETY: OpenSSL 3's ERR_get_error_all, whose signature this matches.
        let all: unsafe extern "C" fn(P, *mut c_int, *mut *const c_char, P, *mut c_int) -> c_ulong =
            unsafe { std::mem::transmute::<*mut c_void, _>(all) };
        return unsafe { all(file, line, func, data, flags) };
    }
    let line_data = resolve(&SLOT_1_1, &[c"ERR_get_error_line_data"]);
    if !func.is_null() {
        // SAFETY: the caller's out-pointer for the function name.
        unsafe { *func = std::ptr::null() };
    }
    // SAFETY: OpenSSL 1.1's ERR_get_error_line_data, whose signature this matches.
    let line_data: unsafe extern "C" fn(P, *mut c_int, P, *mut c_int) -> c_ulong =
        unsafe { std::mem::transmute::<*mut c_void, _>(line_data) };
    unsafe { line_data(file, line, data, flags) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loaded library that lacks a function OpenSSL must export stops the
    /// process: a missing function never passes for a call (a certificate
    /// check, say). The abort happens in a child run of this test binary.
    #[test]
    fn a_missing_function_aborts() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "MSSQL_SQLCMD_OPENSSL_ABORT_CHILD";
        // libm exports no OpenSSL function: a loaded library that lacks them.
        let libm = || {
            // SAFETY: a NUL-terminated name.
            unsafe { libc::dlopen(c"libm.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) }
        };
        if std::env::var_os(CHILD).is_some() {
            static SLOT: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
            resolve_in(libm(), &SLOT, &[c"SSL_new", c"SSL_new_ex"]);
            unreachable!("resolve_in returned for a function the library lacks");
        }
        if libm().is_null() {
            eprintln!("skipped: libm.so.6 is not available");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "openssl_runtime::tests::a_missing_function_aborts",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{output:?}");
        assert!(
            stderr.contains(
                "mssql-sqlcmd: The loaded OpenSSL does not export SSL_new; \
                 it is not a supported version."
            ),
            "{stderr}"
        );
    }

    /// The messages come from the localization catalog, with the library and
    /// function names filled in.
    #[test]
    fn messages_are_localized() {
        assert_eq!(
            not_found_message(),
            "OpenSSL was not found (libssl.so.4, libssl.so.3, libssl.so.1.1); \
             TLS connections are unavailable."
        );
        assert_eq!(
            unsupported_message(c"SSL_new"),
            "The loaded OpenSSL does not export SSL_new; it is not a supported version."
        );
    }

    /// The functions resolve in the installed OpenSSL under whichever of their
    /// names it exports, and behave as OpenSSL's own: a cipher's sizes, an
    /// error string.
    #[test]
    fn functions_resolve_in_the_installed_openssl() {
        assert!(!library().is_null(), "no OpenSSL 4, 3 or 1.1 is installed");
        // SAFETY: OpenSSL functions with valid arguments.
        unsafe {
            let cipher = EVP_aes_256_cbc();
            assert!(!cipher.is_null());
            assert_eq!(EVP_CIPHER_iv_length(cipher), 16);
            assert_eq!(EVP_CIPHER_get_iv_length(cipher), 16);
            assert_eq!(EVP_CIPHER_block_size(cipher), 16);
            assert_eq!(EVP_CIPHER_get_block_size(cipher), 16);
            let ctx = EVP_CIPHER_CTX_new();
            assert!(!ctx.is_null());
            let key = [0u8; 32];
            let iv = [0u8; 16];
            assert_eq!(
                EVP_DecryptInit_ex(ctx, cipher, null_mut(), key.as_ptr(), iv.as_ptr()),
                1
            );
            assert_eq!(EVP_CIPHER_CTX_key_length(ctx), 32);
            assert_eq!(EVP_CIPHER_CTX_get_key_length(ctx), 32);
            assert_eq!(EVP_CIPHER_CTX_get0_cipher(ctx), EVP_CIPHER_CTX_cipher(ctx));
            EVP_CIPHER_CTX_free(ctx);
            // X509_V_ERR_CERT_HAS_EXPIRED
            let text = CStr::from_ptr(X509_verify_cert_error_string(10));
            assert_eq!(text.to_str().unwrap(), "certificate has expired");
            let mut file = std::ptr::null::<c_char>();
            let mut line = 0;
            let mut func = std::ptr::null::<c_char>();
            let mut data = std::ptr::null::<c_char>();
            let mut flags = 0;
            assert_eq!(
                ERR_get_error_all(
                    (&raw mut file).cast(),
                    &mut line,
                    &mut func,
                    (&raw mut data).cast(),
                    &mut flags
                ),
                0,
                "no error is queued"
            );
        }
    }

    /// Raises an error from source file `file` in this thread's queue.
    fn raise(file: &CStr) {
        let handle = library();
        let symbol = |name: &CStr| {
            // SAFETY: a handle dlopen returned and a NUL-terminated name.
            let function = unsafe { libc::dlsym(handle, name.as_ptr()) };
            assert!(!function.is_null(), "{name:?} is not exported");
            function
        };
        // SAFETY: OpenSSL's error functions, with the signatures they have.
        unsafe {
            if libc::dlsym(handle, c"ERR_new".as_ptr()).is_null() {
                // OpenSSL 1.1
                let put: unsafe extern "C" fn(c_int, c_int, c_int, *const c_char, c_int) =
                    std::mem::transmute::<*mut c_void, _>(symbol(c"ERR_put_error"));
                put(20, 0, 1, file.as_ptr(), 1);
            } else {
                let new: unsafe extern "C" fn() =
                    std::mem::transmute::<*mut c_void, _>(symbol(c"ERR_new"));
                let debug: unsafe extern "C" fn(*const c_char, c_int, *const c_char) =
                    std::mem::transmute::<*mut c_void, _>(symbol(c"ERR_set_debug"));
                let set: unsafe extern "C" fn(c_int, c_int, *const c_char, ...) =
                    std::mem::transmute::<*mut c_void, _>(symbol(c"ERR_set_error"));
                new();
                debug(file.as_ptr(), 1, std::ptr::null());
                set(20, 1, std::ptr::null());
            }
        }
    }

    /// The source file name ERR_get_error_line_data returns, which a 1.1-built
    /// `openssl` crate keeps as `&'static str`, outlives the error queue: here
    /// the queue is cleared (OpenSSL 3 and 4 free their copy) and the next
    /// error's name, the same length, would take its memory.
    #[test]
    fn error_file_names_outlive_the_error_queue() {
        assert!(!library().is_null(), "no OpenSSL 4, 3 or 1.1 is installed");
        let handle = library();
        // SAFETY: a handle dlopen returned and a NUL-terminated name.
        let clear = unsafe { libc::dlsym(handle, c"ERR_clear_error".as_ptr()) };
        assert!(!clear.is_null());
        // SAFETY: OpenSSL's ERR_clear_error, which takes no arguments.
        let clear: unsafe extern "C" fn() = unsafe { std::mem::transmute::<*mut c_void, _>(clear) };
        let pop = || {
            let mut file = std::ptr::null::<c_char>();
            let mut line = 0;
            let mut data = std::ptr::null::<c_char>();
            let mut flags = 0;
            // SAFETY: valid out-pointers.
            let code = unsafe {
                ERR_get_error_line_data(&mut file, &mut line, (&raw mut data).cast(), &mut flags)
            };
            assert_ne!(code, 0, "an error is queued");
            file
        };

        // SAFETY: OpenSSL's ERR_clear_error.
        unsafe { clear() };
        raise(c"first.c");
        let first = pop();
        // SAFETY: OpenSSL's ERR_clear_error.
        unsafe { clear() };
        raise(c"other.c");
        // SAFETY: the pointer ERR_get_error_line_data returned, which must stay valid.
        assert_eq!(unsafe { CStr::from_ptr(first) }, c"first.c");
        let other = pop();
        assert_eq!(unsafe { CStr::from_ptr(other) }, c"other.c");
        // The same name is kept once.
        raise(c"first.c");
        assert_eq!(pop(), first);
        // SAFETY: OpenSSL's ERR_clear_error.
        unsafe { clear() };

        // With no error queued OpenSSL leaves `file` as the caller set it, so
        // an unset (here invalid) pointer is returned unread.
        let unset = std::ptr::dangling::<c_char>();
        let mut file = unset;
        let mut line = 0;
        let mut data = std::ptr::null::<c_char>();
        let mut flags = 0;
        // SAFETY: valid out-pointers; `file` must not be read.
        let code = unsafe {
            ERR_get_error_line_data(&mut file, &mut line, (&raw mut data).cast(), &mut flags)
        };
        assert_eq!(code, 0, "no error is queued");
        assert_eq!(file, unset);
    }
}
