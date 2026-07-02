// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TLS channel-binding (`tls-unique`, RFC 5929 §3) extraction for the
//! Schannel-direct engine.
//!
//! SQL Server's Extended Protection for Authentication (EPA) binds the
//! integrated-auth (Kerberos/NTLM) exchange to the TLS channel using the
//! `tls-unique` channel binding. On Windows this is obtained directly from
//! Schannel via `QueryContextAttributesW(SECPKG_ATTR_UNIQUE_BINDINGS)`,
//! exactly as the native SNI (`ssl.cpp::SetChannelBindings`) and msodbcsql
//! (`SNI_SslProvider.cpp::SetChannelBindings`) drivers do.
//!
//! The bytes Schannel returns are a complete `SEC_CHANNEL_BINDINGS` header
//! immediately followed by the application data (the TLS Finished message on
//! TLS 1.2; an all-zero region on TLS 1.3). We copy them out verbatim and
//! hand them straight to `InitializeSecurityContextW` as a
//! `SECBUFFER_CHANNEL_BINDINGS` input buffer — Schannel has already laid out
//! the exact bytes SSPI expects, so there is no hand-construction and no
//! TLS-version branching here.

use std::io;
use std::mem;

use windows_sys::Win32::Foundation;
use windows_sys::Win32::Security::Authentication::Identity;

use super::errors::sec_status_to_io_error;
use super::handshake::SecCtx;

/// Query the `tls-unique` channel binding token from a completed Schannel
/// security context.
///
/// Wraps `QueryContextAttributesW(SECPKG_ATTR_UNIQUE_BINDINGS)`: Schannel
/// fills a [`Identity::SecPkgContext_Bindings`] whose `Bindings` pointer
/// addresses a single SSPI-owned allocation of `BindingsLength` bytes. We
/// copy that buffer out and release it with `FreeContextBuffer`.
///
/// The returned `Vec<u8>` is a full `SEC_CHANNEL_BINDINGS` blob suitable for
/// passing verbatim as a `SECBUFFER_CHANNEL_BINDINGS` input to the SSPI
/// auth context.
//
// FUTURE: RFC 9266 `tls-exporter` is the correct long-term binding for TLS
// 1.3. Schannel currently reports an all-zero application-data region for
// 1.3, which matches what SQL Server expects today, so we pass it through
// unchanged. When tls-exporter support lands end-to-end, derive the 1.3
// token from the exporter instead of this all-zero pass-through.
pub(crate) fn query_unique_bindings(ctx: &SecCtx) -> io::Result<Vec<u8>> {
    // SAFETY: `SecPkgContext_Bindings` is a small POD struct that SSPI fills
    // in; zero-initialize it before the query.
    let mut bindings: Identity::SecPkgContext_Bindings = unsafe { mem::zeroed() };
    // SAFETY: `ctx` is an initialized security context; SECPKG_ATTR_UNIQUE_BINDINGS
    // writes a `SecPkgContext_Bindings` whose `Bindings` field the caller must
    // release with `FreeContextBuffer`.
    let status = unsafe {
        Identity::QueryContextAttributesW(
            ctx.raw(),
            Identity::SECPKG_ATTR_UNIQUE_BINDINGS,
            &mut bindings as *mut _ as *mut _,
        )
    };
    if status != Foundation::SEC_E_OK {
        return Err(sec_status_to_io_error(
            status,
            "QueryContextAttributesW(SECPKG_ATTR_UNIQUE_BINDINGS) failed",
        ));
    }
    if bindings.Bindings.is_null() || bindings.BindingsLength == 0 {
        // A non-null pointer with a zero length is still an SSPI-owned
        // allocation; free it before bailing so we don't leak the buffer.
        if !bindings.Bindings.is_null() {
            // SAFETY: `Bindings` is a context buffer returned by
            // QueryContextAttributesW on SEC_E_OK; release it exactly once.
            unsafe {
                Identity::FreeContextBuffer(bindings.Bindings as *mut _);
            }
        }
        return Err(io::Error::other(
            "QueryContextAttributesW(UNIQUE_BINDINGS) returned an empty buffer",
        ));
    }

    // SAFETY: on SEC_E_OK Schannel guarantees `Bindings` points at a single
    // allocation of `BindingsLength` bytes (SEC_CHANNEL_BINDINGS header plus
    // application data). Copy it out verbatim.
    let token = unsafe {
        std::slice::from_raw_parts(
            bindings.Bindings as *const u8,
            bindings.BindingsLength as usize,
        )
        .to_vec()
    };

    // SAFETY: `Bindings` is an SSPI-owned context buffer returned by
    // QueryContextAttributesW; release it exactly once.
    unsafe {
        Identity::FreeContextBuffer(bindings.Bindings as *mut _);
    }

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_on_dummy_ctx_fails_to_extract() {
        // The dummy context has a zeroed handle, so QueryContextAttributesW
        // returns an error status. Exercises the error branch without a live
        // Schannel handshake (the FFI cannot otherwise be driven from a unit
        // test).
        let dummy = SecCtx::for_test_only();
        assert!(query_unique_bindings(&dummy).is_err());
    }
}
