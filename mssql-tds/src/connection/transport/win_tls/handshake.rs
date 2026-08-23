// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Synchronous Schannel handshake state machine.
//!
//! Drives `InitializeSecurityContextW` repeatedly against a caller-supplied
//! encrypted-input buffer, producing encrypted output bytes to send and
//! consuming input bytes the caller has read off the wire. The driver is
//! pure CPU (no I/O, no `await`). Async I/O is layered on top in
//! [`super::stream`].
//!
//! Mirrors the upstream `schannel-0.1.29::tls_stream::TlsStream::step_initialize`
//! (`tls_stream.rs:467-565`) and ODBC `SNI_SslProvider.cpp:1733-1840`. The
//! crucial divergence: we do **not** call `CertGetCertificateChain` or
//! `CertVerifyCertificateChainPolicy` inline. Validation runs after
//! `SEC_E_OK` in [`super::validate`] and is skipped entirely when
//! the [`CredKind`] requested manual validation. That's the whole point
//! of this module — see plan §0 Bug 1.
//!
//! On the `WouldBlock` / waker-park bug (Bug 2): this driver returns
//! [`StepOutcome::NeedMoreInput`] only when SChannel emitted
//! `SEC_E_INCOMPLETE_MESSAGE` AND consumed nothing. The async wrapper in
//! [`super::stream`] uses that signal to decide whether to park or
//! immediately re-enter — see plan §2.3.

use std::io;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, trace};
use windows_sys::Win32::Foundation;
use windows_sys::Win32::Security::Authentication::Identity;
use windows_sys::Win32::Security::Credentials;

use super::cred::{CredHandle, CredKind};
use super::errors::sec_status_to_io_error;
use super::sspi::{ISC_REQ_COMMON, secbuf, secbuf_desc};

fn sec_status_name(s: i32) -> &'static str {
    match s as u32 {
        x if x == Foundation::SEC_E_OK as u32 => "SEC_E_OK",
        x if x == Foundation::SEC_I_CONTINUE_NEEDED as u32 => "SEC_I_CONTINUE_NEEDED",
        x if x == Foundation::SEC_E_INCOMPLETE_MESSAGE as u32 => "SEC_E_INCOMPLETE_MESSAGE",
        x if x == Foundation::SEC_I_RENEGOTIATE as u32 => "SEC_I_RENEGOTIATE",
        x if x == Foundation::SEC_I_CONTEXT_EXPIRED as u32 => "SEC_I_CONTEXT_EXPIRED",
        x if x == Foundation::SEC_E_WRONG_PRINCIPAL as u32 => "SEC_E_WRONG_PRINCIPAL",
        x if x == Foundation::SEC_E_CERT_EXPIRED as u32 => "SEC_E_CERT_EXPIRED",
        x if x == Foundation::SEC_E_UNTRUSTED_ROOT as u32 => "SEC_E_UNTRUSTED_ROOT",
        x if x == Foundation::SEC_E_ILLEGAL_MESSAGE as u32 => "SEC_E_ILLEGAL_MESSAGE",
        x if x == Foundation::SEC_E_INVALID_TOKEN as u32 => "SEC_E_INVALID_TOKEN",
        x if x == Foundation::SEC_E_INVALID_HANDLE as u32 => "SEC_E_INVALID_HANDLE",
        x if x == Foundation::SEC_E_DECRYPT_FAILURE as u32 => "SEC_E_DECRYPT_FAILURE",
        x if x == Foundation::SEC_E_MESSAGE_ALTERED as u32 => "SEC_E_MESSAGE_ALTERED",
        _ => "OTHER",
    }
}

pub(crate) fn fmt_sec_status(s: i32) -> String {
    format!("{}(0x{:08x})", sec_status_name(s), s as u32)
}

/// Owned `SecurityContext`. Drops by calling `DeleteSecurityContext`.
pub(crate) struct SecCtx {
    handle: Credentials::SecHandle,
    initialized: bool,
}

// SAFETY: SChannel allows InitializeSecurityContextW / EncryptMessage /
// DecryptMessage to be called against the same context handle from
// different threads as long as callers serialize themselves (the async
// stream wrapper owns the context exclusively).
unsafe impl Send for SecCtx {}
unsafe impl Sync for SecCtx {}

impl Drop for SecCtx {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: handle was filled by InitializeSecurityContextW and is
            // valid until we call DeleteSecurityContext exactly once.
            unsafe {
                Identity::DeleteSecurityContext(&self.handle);
            }
        }
    }
}

impl SecCtx {
    fn empty() -> Self {
        SecCtx {
            // SAFETY: zeroing a POD struct.
            handle: unsafe { mem::zeroed() },
            initialized: false,
        }
    }

    pub(crate) fn raw(&self) -> &Credentials::SecHandle {
        &self.handle
    }

    /// Mutable access to the underlying `SecHandle`. Needed by the
    /// record layer to drive post-handshake `InitializeSecurityContextW`
    /// calls (TLS 1.3 NewSessionTicket / KeyUpdate) using the same
    /// security context the handshake established.
    pub(crate) fn raw_mut(&mut self) -> &mut Credentials::SecHandle {
        &mut self.handle
    }

    /// Construct an empty `SecCtx` for tests that don't drive an actual
    /// handshake. The returned context is `!initialized`, so Drop is a
    /// no-op and it's safe to use as a placeholder in branches that
    /// don't touch the handle.
    #[cfg(test)]
    pub(crate) fn for_test_only() -> Self {
        Self::empty()
    }
}

/// Outcome of one [`Handshake::step`] call.
pub(crate) enum StepOutcome {
    /// Handshake completed with no trailing output to flush.
    Done {
        /// Stream sizes queried via `SECPKG_ATTR_STREAM_SIZES`. Required for
        /// sizing encrypt/decrypt buffers in the record layer.
        sizes: Identity::SecPkgContext_StreamSizes,
    },
    /// Handshake completed but a final encrypted handshake record still
    /// needs to be written to the network. The caller MUST flush `out`
    /// before transitioning to the record layer.
    DoneWithFlush {
        out: Vec<u8>,
        sizes: Identity::SecPkgContext_StreamSizes,
    },
    /// SChannel needs more bytes from the network before it can make
    /// progress. Caller should read more, append to its input buffer, and
    /// call [`Handshake::step`] again.
    NeedMoreInput,
    /// SChannel produced output that must be written to the network; after
    /// flushing it the caller still needs to read more input.
    WantWriteThenRead(Vec<u8>),
}

/// Stateful handshake driver. Owns the credential reference and the
/// SECURITY_STATUS-bearing security context across calls.
pub(crate) struct Handshake {
    #[allow(dead_code)] // kept alive for the lifetime of the security context
    cred: Arc<CredHandle>,
    ctx: SecCtx,
    /// Bit ORed into `fContextReq` for every ISC call, derived from the
    /// [`CredKind`] supplied at construction time.
    extra_isc_bits: u32,
    /// UTF-16 (with terminating NUL) form of the SNI hostname, or `None`
    /// to omit SNI.
    domain_w: Option<Vec<u16>>,
    /// True until the first ISC call has been issued. Determines whether
    /// we pass a `phContext` parameter and an input buffer descriptor.
    first_call: bool,
    /// Prebuilt `SEC_APPLICATION_PROTOCOLS` blob to advertise on the
    /// first ISC call, or `None` to skip ALPN negotiation entirely.
    alpn_blob: Option<Vec<u8>>,
    /// Diagnostic counters (debug logging only).
    kind: CredKind,
    started_at: Instant,
    step_count: u32,
    bytes_in_total: usize,
    bytes_out_total: usize,
}

impl Handshake {
    pub(crate) fn new(
        cred: Arc<CredHandle>,
        kind: CredKind,
        server_name: &str,
        alpn_blob: Option<Vec<u8>>,
    ) -> Self {
        let domain_w = if server_name.is_empty() {
            None
        } else {
            Some(server_name.encode_utf16().chain(Some(0u16)).collect())
        };
        debug!(
            kind = ?kind,
            server_name = %server_name,
            alpn_bytes = alpn_blob.as_ref().map(|b| b.len()).unwrap_or(0),
            "win_tls: handshake driver created"
        );
        Self {
            cred,
            ctx: SecCtx::empty(),
            extra_isc_bits: kind.manual_validation_isc_bit(),
            domain_w,
            first_call: true,
            alpn_blob,
            kind,
            started_at: Instant::now(),
            step_count: 0,
            bytes_in_total: 0,
            bytes_out_total: 0,
        }
    }

    /// Drive the handshake one step.
    ///
    /// `input` is the encrypted bytes the caller has buffered from the
    /// network so far. On return:
    /// - On `Done*` / `WantWriteThenRead` the caller must drop the leading
    ///   `*consumed` bytes from its buffer before the next call.
    /// - On `NeedMoreInput`, `*consumed` is 0 — leave the buffer intact
    ///   and append more bytes.
    pub(crate) fn step(
        &mut self,
        input: &mut [u8],
        consumed: &mut usize,
    ) -> io::Result<StepOutcome> {
        *consumed = 0;

        // Two input SecBuffers: the token and an empty placeholder
        // SChannel can populate with SECBUFFER_EXTRA describing any
        // unconsumed trailing bytes.
        // SAFETY: we own `input` for the duration of the FFI call.
        let mut in_bufs = unsafe {
            [
                secbuf(Identity::SECBUFFER_TOKEN, Some(input)),
                secbuf(Identity::SECBUFFER_EMPTY, None),
            ]
        };
        // SAFETY: `in_bufs` outlives the descriptor.
        let in_desc = unsafe { secbuf_desc(&mut in_bufs) };

        // First-call ALPN extension: SChannel reads the protocol list
        // from a SECBUFFER_APPLICATION_PROTOCOLS buffer presented on the
        // first ISC call. We own `alpn_blob` for the FFI duration.
        // SAFETY: `alpn_blob` is borrowed mutably (SSPI never writes it,
        // but `secbuf` takes `&mut [u8]` for type uniformity), kept alive
        // until ISC returns.
        let mut alpn_buf_storage = [unsafe { secbuf(Identity::SECBUFFER_EMPTY, None) }; 1];
        let alpn_desc = if self.first_call {
            self.alpn_blob.as_mut().map(|blob| {
                alpn_buf_storage[0] = unsafe {
                    secbuf(
                        Identity::SECBUFFER_APPLICATION_PROTOCOLS,
                        Some(blob.as_mut_slice()),
                    )
                };
                // SAFETY: `alpn_buf_storage` outlives this descriptor.
                unsafe { secbuf_desc(&mut alpn_buf_storage) }
            })
        } else {
            None
        };

        // One output token buffer with ALLOCATE_MEMORY semantics: SChannel
        // allocates it; we free via FreeContextBuffer below.
        let mut out_bufs = unsafe { [secbuf(Identity::SECBUFFER_TOKEN, None)] };
        let mut out_desc = unsafe { secbuf_desc(&mut out_bufs) };

        let domain_ptr = self
            .domain_w
            .as_ref()
            .map(|v| v.as_ptr())
            .unwrap_or(ptr::null());

        let mut attributes = 0u32;
        let phcontext = if self.first_call {
            ptr::null_mut()
        } else {
            &self.ctx.handle as *const _ as *mut _
        };
        let pinput = if self.first_call {
            // On the first call we either pass the ALPN-only descriptor
            // or nothing at all (no token bytes yet).
            alpn_desc
                .as_ref()
                .map(|d| d as *const _)
                .unwrap_or(ptr::null())
        } else {
            &in_desc as *const _
        };

        let fcontext_req = ISC_REQ_COMMON | self.extra_isc_bits;

        // SAFETY: all pointers are either valid borrows of the data above
        // or documented null values.
        let status = unsafe {
            Identity::InitializeSecurityContextW(
                self.cred.raw(),
                phcontext,
                domain_ptr,
                fcontext_req,
                0,
                0,
                pinput,
                0,
                &mut self.ctx.handle,
                &mut out_desc,
                &mut attributes,
                ptr::null_mut(),
            )
        };

        // After any non-INVALID_HANDLE result the context handle is populated.
        if !self.ctx.initialized && status != Foundation::SEC_E_INVALID_HANDLE {
            self.ctx.initialized = true;
        }
        let was_first = self.first_call;
        self.first_call = false;
        self.step_count += 1;
        let bytes_in_this_step = if was_first { 0 } else { input.len() };
        self.bytes_in_total += bytes_in_this_step;

        // Pull any output bytes SChannel produced; this frees the SSPI alloc.
        let out_bytes = take_owned_buffer(&mut out_bufs[0]);
        self.bytes_out_total += out_bytes.len();

        trace!(
            step = self.step_count,
            status = %fmt_sec_status(status),
            bytes_in = bytes_in_this_step,
            bytes_out = out_bytes.len(),
            attributes = format!("0x{:08x}", attributes),
            "win_tls: ISC step"
        );

        // Determine consumed-byte count from SECBUFFER_EXTRA, if any. On the
        // very first call we didn't supply an input descriptor so consumed
        // stays at 0.
        let extra_len: usize = if !was_first && in_bufs[1].BufferType == Identity::SECBUFFER_EXTRA {
            in_bufs[1].cbBuffer as usize
        } else {
            0
        };

        match status as u32 {
            x if x == Foundation::SEC_I_CONTINUE_NEEDED as u32 => {
                if !was_first {
                    *consumed = input.len().saturating_sub(extra_len);
                }
                if !out_bytes.is_empty() {
                    Ok(StepOutcome::WantWriteThenRead(out_bytes))
                } else {
                    Ok(StepOutcome::NeedMoreInput)
                }
            }
            x if x == Foundation::SEC_E_INCOMPLETE_MESSAGE as u32 => {
                // Need more bytes; do NOT consume anything.
                Ok(StepOutcome::NeedMoreInput)
            }
            x if x == Foundation::SEC_E_OK as u32 => {
                if !was_first {
                    *consumed = input.len().saturating_sub(extra_len);
                }
                if !was_first {
                    *consumed = input.len().saturating_sub(extra_len);
                }
                let sizes = self.query_stream_sizes()?;
                debug!(
                    kind = ?self.kind,
                    steps = self.step_count,
                    elapsed_ms = self.started_at.elapsed().as_millis() as u64,
                    bytes_in_total = self.bytes_in_total,
                    bytes_out_total = self.bytes_out_total,
                    extra_len = if was_first { 0 } else { extra_len },
                    max_message = sizes.cbMaximumMessage,
                    "win_tls: handshake complete"
                );
                if !out_bytes.is_empty() {
                    Ok(StepOutcome::DoneWithFlush {
                        out: out_bytes,
                        sizes,
                    })
                } else {
                    Ok(StepOutcome::Done { sizes })
                }
            }
            _ => {
                debug!(
                    kind = ?self.kind,
                    steps = self.step_count,
                    elapsed_ms = self.started_at.elapsed().as_millis() as u64,
                    bytes_in_total = self.bytes_in_total,
                    bytes_out_total = self.bytes_out_total,
                    status = %fmt_sec_status(status),
                    "win_tls: handshake FAILED at ISC"
                );
                Err(sec_status_to_io_error(
                    status,
                    "InitializeSecurityContextW failed",
                ))
            }
        }
    }

    fn query_stream_sizes(&self) -> io::Result<Identity::SecPkgContext_StreamSizes> {
        // SAFETY: zero-init POD; ctx is initialized at this point.
        let mut sizes: Identity::SecPkgContext_StreamSizes = unsafe { mem::zeroed() };
        let status = unsafe {
            Identity::QueryContextAttributesW(
                &self.ctx.handle,
                Identity::SECPKG_ATTR_STREAM_SIZES,
                &mut sizes as *mut _ as *mut _,
            )
        };
        if status == Foundation::SEC_E_OK {
            Ok(sizes)
        } else {
            Err(sec_status_to_io_error(
                status,
                "QueryContextAttributesW(STREAM_SIZES) failed",
            ))
        }
    }

    /// Hand the underlying security context to the record layer once the
    /// handshake is complete. Leaves `self` with an uninitialized context
    /// so Drop is a no-op.
    pub(crate) fn into_ctx(mut self) -> SecCtx {
        let ctx = SecCtx {
            handle: self.ctx.handle,
            initialized: self.ctx.initialized,
        };
        self.ctx.initialized = false;
        ctx
    }

    /// Borrow the raw security context handle. Valid only after at least
    /// one ISC call has populated the handle.
    pub(crate) fn ctx_handle(&self) -> &Credentials::SecHandle {
        &self.ctx.handle
    }
}

/// Take ownership of an FFI-allocated `SecBuffer`'s payload as a
/// `Vec<u8>`, freeing the original allocation via `FreeContextBuffer`.
/// After this returns the input buffer is left as `(null, 0)`.
fn take_owned_buffer(buf: &mut Identity::SecBuffer) -> Vec<u8> {
    if buf.pvBuffer.is_null() || buf.cbBuffer == 0 {
        return Vec::new();
    }
    // SAFETY: cbBuffer bytes at pvBuffer are a valid initialised region.
    let slice =
        unsafe { std::slice::from_raw_parts(buf.pvBuffer as *const u8, buf.cbBuffer as usize) };
    let owned = slice.to_vec();
    // SAFETY: pvBuffer was allocated by SSPI (ISC_REQ_ALLOCATE_MEMORY).
    unsafe {
        Identity::FreeContextBuffer(buf.pvBuffer);
    }
    buf.pvBuffer = ptr::null_mut();
    buf.cbBuffer = 0;
    owned
}

/// Same as [`take_owned_buffer`] but callable from sibling modules
/// that hold a `&mut SecBuffer` (e.g. record_layer's post-handshake
/// ISC redrive). Wraps the private helper above.
pub(crate) fn take_owned_buffer_pub(buf: &mut Identity::SecBuffer) -> Vec<u8> {
    take_owned_buffer(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_step_produces_client_hello() {
        let cred = super::super::cred::get_or_acquire(CredKind::NoValidate).unwrap();
        let mut hs = Handshake::new(cred, CredKind::NoValidate, "example.com", None);
        let mut input: [u8; 0] = [];
        let mut consumed = 0;
        let outcome = hs.step(&mut input, &mut consumed).expect("first step ok");
        match outcome {
            StepOutcome::WantWriteThenRead(client_hello) => {
                // TLS record: type=22 (handshake), version 03 0x (TLS 1.0+),
                // length, then handshake type=1 (ClientHello).
                assert!(client_hello.len() > 5);
                assert_eq!(client_hello[0], 0x16, "expected TLS handshake record");
                assert_eq!(client_hello[1], 0x03, "expected TLS major version 3");
                // ClientHello inside the record:
                assert_eq!(client_hello[5], 0x01, "expected ClientHello handshake type");
            }
            _ => panic!("first ISC call should emit ClientHello via WantWriteThenRead"),
        }
        assert_eq!(consumed, 0);
    }

    #[test]
    fn incomplete_message_consumes_nothing() {
        let cred = super::super::cred::get_or_acquire(CredKind::NoValidate).unwrap();
        let mut hs = Handshake::new(cred, CredKind::NoValidate, "example.com", None);
        // Bootstrap: discard the initial ClientHello.
        let _ = hs.step(&mut [], &mut 0).unwrap();
        // Now feed a partial TLS record: just the 5-byte header claiming a
        // big payload. SChannel should respond SEC_E_INCOMPLETE_MESSAGE.
        let mut partial = vec![0x16u8, 0x03, 0x03, 0x10, 0x00];
        let mut consumed = 999;
        let outcome = hs.step(&mut partial, &mut consumed).unwrap();
        assert!(matches!(outcome, StepOutcome::NeedMoreInput));
        assert_eq!(consumed, 0, "must NOT consume on incomplete message");
    }

    #[test]
    fn fmt_sec_status_names_known_codes_and_includes_hex() {
        let cases: &[(i32, &str)] = &[
            (Foundation::SEC_E_OK, "SEC_E_OK"),
            (Foundation::SEC_I_CONTINUE_NEEDED, "SEC_I_CONTINUE_NEEDED"),
            (
                Foundation::SEC_E_INCOMPLETE_MESSAGE,
                "SEC_E_INCOMPLETE_MESSAGE",
            ),
            (Foundation::SEC_I_RENEGOTIATE, "SEC_I_RENEGOTIATE"),
            (Foundation::SEC_I_CONTEXT_EXPIRED, "SEC_I_CONTEXT_EXPIRED"),
            (Foundation::SEC_E_WRONG_PRINCIPAL, "SEC_E_WRONG_PRINCIPAL"),
            (Foundation::SEC_E_CERT_EXPIRED, "SEC_E_CERT_EXPIRED"),
            (Foundation::SEC_E_UNTRUSTED_ROOT, "SEC_E_UNTRUSTED_ROOT"),
            (Foundation::SEC_E_ILLEGAL_MESSAGE, "SEC_E_ILLEGAL_MESSAGE"),
            (Foundation::SEC_E_INVALID_TOKEN, "SEC_E_INVALID_TOKEN"),
            (Foundation::SEC_E_INVALID_HANDLE, "SEC_E_INVALID_HANDLE"),
            (Foundation::SEC_E_DECRYPT_FAILURE, "SEC_E_DECRYPT_FAILURE"),
            (Foundation::SEC_E_MESSAGE_ALTERED, "SEC_E_MESSAGE_ALTERED"),
        ];
        for (code, name) in cases {
            let s = fmt_sec_status(*code);
            assert!(s.contains(name), "expected '{name}' in '{s}'");
            assert!(
                s.contains(&format!("{:08x}", *code as u32)),
                "expected hex of 0x{:08x} in '{s}'",
                *code as u32
            );
        }
    }

    #[test]
    fn fmt_sec_status_unknown_code_is_other() {
        let s = fmt_sec_status(0x1234_5678);
        assert!(s.contains("OTHER"), "expected 'OTHER' in '{s}'");
        assert!(s.contains("12345678"), "expected hex in '{s}'");
    }
}
