// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Low-level FFI wrappers around the SSPI symbols we use.
//!
//! These wrappers exist so the rest of the module can spell `secbuf`,
//! `secbuf_desc`, etc. instead of inlining raw struct initializers. They are
//! intentionally thin — anything beyond layout assembly belongs in
//! `handshake` or `record_layer`.

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Security::Authentication::Identity;

/// Build a `SecBuffer` of the given type pointing at the given slice (or
/// `(null, 0)` when `bytes` is `None`).
///
/// # Safety
/// The returned `SecBuffer` borrows `bytes` for the duration it's passed to
/// SSPI. Callers must keep the source buffer alive across the FFI call.
pub(crate) unsafe fn secbuf(buftype: u32, bytes: Option<&mut [u8]>) -> Identity::SecBuffer {
    let (ptr, len) = match bytes {
        Some(b) => (b.as_mut_ptr(), b.len() as u32),
        None => (ptr::null_mut(), 0),
    };
    Identity::SecBuffer {
        BufferType: buftype,
        cbBuffer: len,
        pvBuffer: ptr as *mut c_void,
    }
}

/// Wrap a slice of `SecBuffer`s in a `SecBufferDesc` ready to hand to SSPI.
///
/// # Safety
/// Caller must keep `bufs` alive for the duration of the SSPI call that
/// receives the returned descriptor.
pub(crate) unsafe fn secbuf_desc(bufs: &mut [Identity::SecBuffer]) -> Identity::SecBufferDesc {
    Identity::SecBufferDesc {
        ulVersion: Identity::SECBUFFER_VERSION,
        cBuffers: bufs.len() as u32,
        pBuffers: bufs.as_mut_ptr(),
    }
}

/// ISC flags common to every client handshake step. Modelled on the
/// `schannel` crate (`schannel-0.1.29::lib.rs:79-86`), NOT a bit-for-bit
/// copy of ODBC's `SSL_CONTEXT_REQ` (`SNI_SslProvider.cpp:57-62`).
///
/// Deliberate divergences from ODBC's flag set:
///
/// * `ISC_REQ_ALLOCATE_MEMORY` — ODBC does NOT set this. It hands Schannel
///   a pre-allocated buffer from its own `SNI_Packet` pool and lets the SSP
///   write into it, so it never frees a handshake token. We have no such
///   pool, so we pass a null-pointer `SECBUFFER_TOKEN`, let Schannel size
///   and allocate the exact output token, copy it into a `Vec<u8>`, and
///   release the original via `FreeContextBuffer` (see
///   `handshake.rs::take_owned_buffer`). This avoids a guess-the-size /
///   `SEC_E_BUFFER_TOO_SMALL` retry loop. It costs a small heap alloc +
///   copy per handshake STEP (~3 per connection), never per data packet —
///   `EncryptMessage`/`DecryptMessage` operate on caller-owned buffers and
///   do not use this flag.
///
/// * `ISC_REQ_INTEGRITY` — we request it explicitly; ODBC relies on
///   `ISC_REQ_STREAM` implying record integrity. Harmless for TLS streams.
///
/// * `ISC_REQ_EXTENDED_ERROR` — we request it for ODBC parity. ODBC ORs
///   `ISC_RET_EXTENDED_ERROR` into `fContextReq` (`SSL_CONTEXT_REQ`,
///   `SNI_SslProvider.cpp:60`); the ISC_RET_ and ISC_REQ_ constants share
///   the same bit value (0x4000), so that is functionally a request for the
///   ISC_REQ_EXTENDED_ERROR input behaviour: "When errors occur, the remote
///   party will be notified" — i.e. on a context error Schannel emits a TLS
///   alert token to the peer instead of failing silently. See the
///   `fContextReq` table in the InitializeSecurityContext (Schannel) docs.
///
/// Deliberately NOT mirrored from ODBC:
///
/// * `ISC_RET_USE_SESSION_KEY` (0x20) — ODBC ORs this into `fContextReq`
///   too, but `ISC_REQ_USE_SESSION_KEY` is not a documented Schannel
///   `fContextReq` flag (it is a legacy/general SSPI flag with no defined
///   Schannel effect). We omit it; there is no behaviour to match.
///
/// Note: `ISC_REQ_MANUAL_CRED_VALIDATION` is added per-call in the handshake
/// driver depending on the `CredKind` selected — see [`super::cred::CredKind`].
/// This common bag does NOT include it so callers can opt in.
pub(crate) const ISC_REQ_COMMON: u32 = Identity::ISC_REQ_CONFIDENTIALITY
    | Identity::ISC_REQ_INTEGRITY
    | Identity::ISC_REQ_REPLAY_DETECT
    | Identity::ISC_REQ_SEQUENCE_DETECT
    | Identity::ISC_REQ_EXTENDED_ERROR
    | Identity::ISC_REQ_ALLOCATE_MEMORY
    | Identity::ISC_REQ_STREAM
    | Identity::ISC_REQ_USE_SUPPLIED_CREDS;
