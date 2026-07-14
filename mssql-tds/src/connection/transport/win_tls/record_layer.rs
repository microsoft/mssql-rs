// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Synchronous Schannel record layer: encrypt / decrypt of application
//! data after the handshake completes.
//!
//! Operates on caller-owned encrypted-input and plaintext-output buffers
//! exactly like [`super::handshake::Handshake`] does for the handshake
//! phase. The async wrapper in [`super::stream`] glues this to tokio's
//! poll API.
//!
//! Reference: ODBC `Ssl::Encrypt` / `Ssl::Decrypt`
//! (`SNI_SslProvider.cpp:687-705` / `313-322`), and
//! `schannel-0.1.29::tls_stream.rs:843-947` for the EncryptMessage /
//! DecryptMessage / partial-record semantics we match.

use std::io;
use std::ptr;
use std::sync::Arc;

use tracing::{debug, trace};
use windows_sys::Win32::Foundation;
use windows_sys::Win32::Security::Authentication::Identity;

use super::cred::{CredHandle, CredKind};
use super::errors::sec_status_to_io_error;
use super::handshake::{SecCtx, fmt_sec_status};
use super::sspi::{ISC_REQ_COMMON, secbuf, secbuf_desc};

/// Sync record layer wrapping the post-handshake [`SecCtx`].
///
/// Carries the credential / target-name state needed to handle TLS 1.3
/// post-handshake messages (NewSessionTicket, KeyUpdate) which arrive
/// as `SEC_I_RENEGOTIATE` returns from `DecryptMessage` and must be
/// fed back through `InitializeSecurityContextW`. See the
/// `SEC_I_RENEGOTIATE` branch in [`RecordLayer::decrypt`] below.
pub(crate) struct RecordLayer {
    ctx: SecCtx,
    sizes: Identity::SecPkgContext_StreamSizes,
    /// Kept alive for the lifetime of the security context; also used by
    /// the post-handshake ISC redrive below.
    cred: Arc<CredHandle>,
    /// ISC bit derived from [`CredKind`] — must be re-applied to every
    /// post-handshake ISC call so SChannel keeps the same validation
    /// posture it had during the original handshake.
    extra_isc_bits: u32,
    /// UTF-16 (NUL-terminated) target name. `None` when the original
    /// handshake didn't supply one.
    domain_w: Option<Vec<u16>>,
}

impl RecordLayer {
    pub(crate) fn new(
        ctx: SecCtx,
        sizes: Identity::SecPkgContext_StreamSizes,
        cred: Arc<CredHandle>,
        kind: CredKind,
        server_name: &str,
    ) -> Self {
        let domain_w = if server_name.is_empty() {
            None
        } else {
            Some(server_name.encode_utf16().chain(Some(0u16)).collect())
        };
        Self {
            ctx,
            sizes,
            cred,
            extra_isc_bits: kind.manual_validation_isc_bit(),
            domain_w,
        }
    }

    pub(crate) fn ctx(&self) -> &SecCtx {
        &self.ctx
    }

    pub(crate) fn max_message(&self) -> usize {
        self.sizes.cbMaximumMessage as usize
    }

    pub(crate) fn header_len(&self) -> usize {
        self.sizes.cbHeader as usize
    }

    pub(crate) fn trailer_len(&self) -> usize {
        self.sizes.cbTrailer as usize
    }

    /// Encrypt `plaintext` into `out` as a single TLS record. `plaintext`'s
    /// length must be ≤ [`Self::max_message`]; callers split larger writes.
    pub(crate) fn encrypt(&self, plaintext: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        let header_len = self.header_len();
        let trailer_len = self.trailer_len();
        let total = header_len + plaintext.len() + trailer_len;

        out.clear();
        out.resize(total, 0);
        out[header_len..header_len + plaintext.len()].copy_from_slice(plaintext);

        // Carve into three non-overlapping slices: header / data / trailer.
        let (header, rest) = out.split_at_mut(header_len);
        let (data, trailer) = rest.split_at_mut(plaintext.len());

        // SAFETY: header/data/trailer are disjoint &mut slices of `out`.
        let mut bufs = unsafe {
            [
                secbuf(Identity::SECBUFFER_STREAM_HEADER, Some(header)),
                secbuf(Identity::SECBUFFER_DATA, Some(data)),
                secbuf(Identity::SECBUFFER_STREAM_TRAILER, Some(trailer)),
                secbuf(Identity::SECBUFFER_EMPTY, None),
            ]
        };
        let desc = unsafe { secbuf_desc(&mut bufs) };

        // SAFETY: bufs / desc are kept alive across the FFI call.
        let status = unsafe { Identity::EncryptMessage(self.ctx.raw(), 0, &desc, 0) };
        if status == Foundation::SEC_E_OK {
            // SChannel may have written fewer trailer bytes than max;
            // truncate to the actual record length.
            let actual_trailer = bufs[2].cbBuffer as usize;
            let actual = header_len + plaintext.len() + actual_trailer;
            out.truncate(actual);
            Ok(())
        } else {
            Err(sec_status_to_io_error(status, "EncryptMessage failed"))
        }
    }

    /// Try to decrypt one TLS record out of `encrypted_in`.
    ///
    /// - On `Decrypted::Ok`: the consumed prefix is removed from
    ///   `encrypted_in`, the decrypted plaintext is appended to
    ///   `plaintext_out`.
    /// - On `Decrypted::NeedMoreInput`: `encrypted_in` is untouched — the
    ///   caller must append more bytes from the network and retry.
    /// - On `Decrypted::PeerClosed`: server sent close_notify.
    pub(crate) fn decrypt(
        &mut self,
        encrypted_in: &mut Vec<u8>,
        plaintext_out: &mut Vec<u8>,
    ) -> io::Result<Decrypted> {
        if encrypted_in.is_empty() {
            return Ok(Decrypted::NeedMoreInput);
        }

        // SECBUFFER_DATA initially points at the full encrypted prefix;
        // SChannel rewrites it in place with the decrypted bytes and uses
        // SECBUFFER_EXTRA to describe any trailing bytes that belong to
        // the next record.
        // SAFETY: each buffer references disjoint memory; index 0 covers
        // the whole encrypted_in, indices 1-3 are EMPTY placeholders.
        let mut bufs = unsafe {
            [
                secbuf(Identity::SECBUFFER_DATA, Some(&mut encrypted_in[..])),
                secbuf(Identity::SECBUFFER_EMPTY, None),
                secbuf(Identity::SECBUFFER_EMPTY, None),
                secbuf(Identity::SECBUFFER_EMPTY, None),
            ]
        };
        let desc = unsafe { secbuf_desc(&mut bufs) };

        // SAFETY: bufs / desc alive across the call.
        let status = unsafe { Identity::DecryptMessage(self.ctx.raw(), &desc, 0, ptr::null_mut()) };
        trace!(
            status = %fmt_sec_status(status),
            encrypted_in_len = encrypted_in.len(),
            "win_tls: DecryptMessage"
        );

        match status as u32 {
            x if x == Foundation::SEC_E_OK as u32 => {
                let (data_ptr, data_len, extra_ptr, extra_len) = {
                    let mut data: (*mut u8, usize) = (ptr::null_mut(), 0);
                    let mut extra: (*mut u8, usize) = (ptr::null_mut(), 0);
                    for b in &bufs {
                        match b.BufferType {
                            t if t == Identity::SECBUFFER_DATA => {
                                data = (b.pvBuffer as *mut u8, b.cbBuffer as usize);
                            }
                            t if t == Identity::SECBUFFER_EXTRA => {
                                extra = (b.pvBuffer as *mut u8, b.cbBuffer as usize);
                            }
                            _ => {}
                        }
                    }
                    (data.0, data.1, extra.0, extra.1)
                };

                if !data_ptr.is_null() && data_len > 0 {
                    // SAFETY: data_ptr/data_len point inside encrypted_in's
                    // current allocation, which is live for this scope.
                    let slice = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
                    plaintext_out.extend_from_slice(slice);
                }

                if extra_len > 0 && !extra_ptr.is_null() {
                    // SChannel leaves the unconsumed tail (the next record's
                    // bytes) as the suffix of `encrypted_in`. Compact it to
                    // the front in place, reusing the existing allocation
                    // instead of round-tripping the tail through a temporary
                    // `Vec` on every decrypted record.
                    let offset = encrypted_in.len() - extra_len;
                    debug_assert_eq!(
                        extra_ptr as usize,
                        encrypted_in.as_ptr() as usize + offset,
                        "SECBUFFER_EXTRA must be the suffix of encrypted_in"
                    );
                    compact_suffix(encrypted_in, extra_len);
                } else {
                    encrypted_in.clear();
                }
                Ok(Decrypted::Ok)
            }
            x if x == Foundation::SEC_E_INCOMPLETE_MESSAGE as u32 => Ok(Decrypted::NeedMoreInput),
            x if x == Foundation::SEC_I_CONTEXT_EXPIRED as u32 => {
                debug!("win_tls: peer closed TLS session (SEC_I_CONTEXT_EXPIRED)");
                Ok(Decrypted::PeerClosed)
            }
            x if x == Foundation::SEC_I_RENEGOTIATE as u32 => {
                // TLS 1.3 post-handshake message (NewSessionTicket /
                // KeyUpdate). SChannel signals these via SEC_I_RENEGOTIATE
                // and stashes the encrypted record in SECBUFFER_EXTRA.
                // We must re-invoke InitializeSecurityContextW with those
                // bytes to process the message, then resume decrypt with
                // anything ISC didn't consume.
                //
                // See https://learn.microsoft.com/windows/win32/secauthn/decryptmessage--schannel
                // ("In TLS 1.3, the SEC_I_RENEGOTIATE return value
                // indicates that a post-handshake message ... was
                // received").
                let extra_bytes: Vec<u8> = {
                    let mut found: Option<&Identity::SecBuffer> = None;
                    for b in &bufs {
                        if b.BufferType == Identity::SECBUFFER_EXTRA
                            && !b.pvBuffer.is_null()
                            && b.cbBuffer > 0
                        {
                            found = Some(b);
                            break;
                        }
                    }
                    match found {
                        Some(b) => {
                            // SAFETY: pvBuffer/cbBuffer point inside
                            // encrypted_in's current allocation.
                            unsafe {
                                std::slice::from_raw_parts(
                                    b.pvBuffer as *const u8,
                                    b.cbBuffer as usize,
                                )
                            }
                            .to_vec()
                        }
                        None => Vec::new(),
                    }
                };
                encrypted_in.clear();
                debug!(
                    extra_len = extra_bytes.len(),
                    "win_tls: TLS 1.3 post-handshake message (SEC_I_RENEGOTIATE)"
                );
                // After the post-handshake message is consumed there's no
                // application plaintext to deliver this round. If ISC fully
                // consumed the message we report `Decrypted::Ok` so the
                // caller re-enters to decrypt any trailing record bytes ISC
                // handed back. If the message was incomplete we report
                // `NeedMoreInput` directly so the caller reads from the wire
                // instead of re-decrypting the same buffered bytes (which
                // would only return `SEC_E_INCOMPLETE_MESSAGE` again).
                match self.process_post_handshake(extra_bytes, encrypted_in)? {
                    PostHandshake::Done => Ok(Decrypted::Ok),
                    PostHandshake::NeedMoreInput => Ok(Decrypted::NeedMoreInput),
                }
            }
            _ => {
                debug!(
                    status = %fmt_sec_status(status),
                    encrypted_in_len = encrypted_in.len(),
                    "win_tls: DecryptMessage FAILED"
                );
                Err(sec_status_to_io_error(status, "DecryptMessage failed"))
            }
        }
    }

    /// Drive `InitializeSecurityContextW` once with `input` (the bytes
    /// SChannel handed back in `SECBUFFER_EXTRA` alongside a
    /// `SEC_I_RENEGOTIATE` from `DecryptMessage`). Any unconsumed
    /// trailing bytes are appended to `leftover`.
    ///
    /// For the messages SQL Server actually emits (TLS 1.3
    /// NewSessionTicket) ISC returns `SEC_E_OK` with no output token
    /// and no further-input requirement. KeyUpdate or true
    /// renegotiation would produce an output token that needs writing
    /// to the wire; we treat that as unsupported with an explicit
    /// error rather than silently dropping it. If we hit that in
    /// practice the fix is to surface a new variant up to the stream
    /// layer so it can use the socket.
    fn process_post_handshake(
        &mut self,
        input: Vec<u8>,
        leftover: &mut Vec<u8>,
    ) -> io::Result<PostHandshake> {
        let mut input_buf = input;

        // SAFETY: input_buf alive for the FFI call. Two-buffer input
        // descriptor mirrors Handshake::step's shape.
        let mut in_bufs = unsafe {
            [
                secbuf(Identity::SECBUFFER_TOKEN, Some(&mut input_buf[..])),
                secbuf(Identity::SECBUFFER_EMPTY, None),
            ]
        };
        let in_desc = unsafe { secbuf_desc(&mut in_bufs) };

        let mut out_bufs = unsafe { [secbuf(Identity::SECBUFFER_TOKEN, None)] };
        let mut out_desc = unsafe { secbuf_desc(&mut out_bufs) };

        let domain_ptr = self
            .domain_w
            .as_ref()
            .map(|v| v.as_ptr())
            .unwrap_or(ptr::null());

        let mut attributes = 0u32;
        let fcontext_req = ISC_REQ_COMMON | self.extra_isc_bits;

        // SAFETY: pointers are valid borrows; ctx is initialized
        // (we only get here from decrypt(), which requires a live
        // streaming context).
        let status = unsafe {
            Identity::InitializeSecurityContextW(
                self.cred.raw(),
                self.ctx.raw_mut(),
                domain_ptr,
                fcontext_req,
                0,
                0,
                &in_desc,
                0,
                self.ctx.raw_mut(),
                &mut out_desc,
                &mut attributes,
                ptr::null_mut(),
            )
        };

        let out_token = super::handshake::take_owned_buffer_pub(&mut out_bufs[0]);

        let extra_len: usize = if in_bufs[1].BufferType == Identity::SECBUFFER_EXTRA {
            in_bufs[1].cbBuffer as usize
        } else {
            0
        };
        if extra_len > 0 {
            let start = input_buf.len() - extra_len;
            leftover.extend_from_slice(&input_buf[start..]);
        }

        match status as u32 {
            x if x == Foundation::SEC_E_OK as u32 => {
                if !out_token.is_empty() {
                    return Err(io::Error::other(
                        "Schannel post-handshake produced an output token; \
                         KeyUpdate / true renegotiation is not yet supported \
                         by win_tls",
                    ));
                }
                Ok(PostHandshake::Done)
            }
            x if x == Foundation::SEC_I_CONTINUE_NEEDED as u32 => Err(io::Error::other(
                "Schannel post-handshake requested another roundtrip; \
                 KeyUpdate / true renegotiation is not yet supported by win_tls",
            )),
            x if x == Foundation::SEC_E_INCOMPLETE_MESSAGE as u32 => {
                // Not enough bytes for this post-handshake message. Put
                // the whole input back at the front of `leftover` so the
                // caller will read more from the wire and retry.
                prepend_leftover(&input_buf, leftover);
                Ok(PostHandshake::NeedMoreInput)
            }
            _ => Err(sec_status_to_io_error(
                status,
                "InitializeSecurityContextW (post-handshake) failed",
            )),
        }
    }
}

/// Outcome of [`RecordLayer::decrypt`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decrypted {
    /// One record decrypted; output buffer extended.
    Ok,
    /// Insufficient input bytes for a full record; read more from the wire.
    NeedMoreInput,
    /// Server sent close_notify.
    PeerClosed,
}

/// Outcome of [`RecordLayer::process_post_handshake`].
#[derive(Debug, PartialEq, Eq)]
enum PostHandshake {
    /// Message fully consumed; retry decrypt on any leftover bytes.
    Done,
    /// Not enough bytes for the message; read more from the wire.
    NeedMoreInput,
}

/// Prepend `input` ahead of `leftover` so an incomplete post-handshake
/// message is retried, in order, once more bytes arrive from the wire.
fn prepend_leftover(input: &[u8], leftover: &mut Vec<u8>) {
    let mut combined = Vec::with_capacity(input.len() + leftover.len());
    combined.extend_from_slice(input);
    combined.extend_from_slice(leftover);
    *leftover = combined;
}

/// Move the trailing `extra_len` bytes of `buf` to the front and drop the
/// rest, keeping the existing allocation. Used on the decrypt hot path to
/// retain the unconsumed `SECBUFFER_EXTRA` tail without an extra allocation.
fn compact_suffix(buf: &mut Vec<u8>, extra_len: usize) {
    let offset = buf.len() - extra_len;
    if offset > 0 {
        buf.copy_within(offset.., 0);
    }
    buf.truncate(extra_len);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_record_layer(server_name: &str) -> RecordLayer {
        // SAFETY: SecPkgContext_StreamSizes is plain-old-data.
        let mut sizes: Identity::SecPkgContext_StreamSizes = unsafe { std::mem::zeroed() };
        sizes.cbHeader = 5;
        sizes.cbTrailer = 16;
        sizes.cbMaximumMessage = 16384;
        // A real cached credential (AcquireCredentialsHandleW succeeds on the
        // Windows test host); the dummy SecCtx keeps the FFI calls returning
        // SEC_E_INVALID_HANDLE so we exercise the error paths without a live
        // handshake.
        let cred = super::super::cred::get_or_acquire(CredKind::NoValidate)
            .expect("acquire test credential");
        RecordLayer::new(
            SecCtx::for_test_only(),
            sizes,
            cred,
            CredKind::NoValidate,
            server_name,
        )
    }

    #[test]
    fn new_empty_server_name_has_no_domain() {
        let rl = test_record_layer("");
        assert!(rl.domain_w.is_none());
    }

    #[test]
    fn new_with_server_name_sets_utf16_nul_terminated_domain() {
        let rl = test_record_layer("host");
        let domain = rl.domain_w.expect("domain should be set");
        assert_eq!(
            domain,
            vec![b'h' as u16, b'o' as u16, b's' as u16, b't' as u16, 0]
        );
    }

    #[test]
    fn accessors_reflect_stream_sizes() {
        let rl = test_record_layer("host");
        assert_eq!(rl.header_len(), 5);
        assert_eq!(rl.trailer_len(), 16);
        assert_eq!(rl.max_message(), 16384);
        // ctx() hands back the borrowed context.
        let _ = rl.ctx();
    }

    #[test]
    fn encrypt_with_invalid_handle_errors() {
        let rl = test_record_layer("host");
        let mut out = Vec::new();
        let err = rl.encrypt(b"hello", &mut out).unwrap_err();
        assert!(err.to_string().contains("EncryptMessage"));
    }

    #[test]
    fn decrypt_empty_input_needs_more() {
        let mut rl = test_record_layer("host");
        let mut enc = Vec::new();
        let mut plain = Vec::new();
        assert_eq!(
            rl.decrypt(&mut enc, &mut plain).unwrap(),
            Decrypted::NeedMoreInput
        );
    }

    #[test]
    fn decrypt_with_invalid_handle_errors() {
        let mut rl = test_record_layer("host");
        let mut enc = vec![0x17, 0x03, 0x03, 0x00, 0x05, 1, 2, 3, 4, 5];
        let mut plain = Vec::new();
        let err = rl.decrypt(&mut enc, &mut plain).unwrap_err();
        assert!(err.to_string().contains("DecryptMessage"));
    }

    #[test]
    fn process_post_handshake_with_invalid_handle_errors() {
        let mut rl = test_record_layer("host");
        let mut leftover = Vec::new();
        let err = rl
            .process_post_handshake(vec![1, 2, 3, 4], &mut leftover)
            .unwrap_err();
        assert!(err.to_string().contains("InitializeSecurityContextW"));
    }

    #[test]
    fn prepend_leftover_restores_input_before_existing_tail() {
        // Bytes that just arrived but didn't complete the post-handshake
        // message must be replayed first, ahead of anything already queued,
        // so the next decrypt sees the original on-wire order.
        let mut leftover = vec![3u8, 4, 5];
        prepend_leftover(&[1, 2], &mut leftover);
        assert_eq!(leftover, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn prepend_leftover_into_empty_tail() {
        let mut leftover = Vec::new();
        prepend_leftover(&[9, 8, 7], &mut leftover);
        assert_eq!(leftover, vec![9, 8, 7]);
    }

    #[test]
    fn prepend_leftover_empty_input_is_noop() {
        let mut leftover = vec![1u8, 2, 3];
        prepend_leftover(&[], &mut leftover);
        assert_eq!(leftover, vec![1, 2, 3]);
    }

    #[test]
    fn compact_suffix_retains_only_the_tail() {
        let mut buf = vec![1u8, 2, 3, 4, 5];
        compact_suffix(&mut buf, 2);
        assert_eq!(buf, vec![4, 5]);
    }

    #[test]
    fn compact_suffix_whole_buffer_is_noop() {
        let mut buf = vec![7u8, 8, 9];
        compact_suffix(&mut buf, 3);
        assert_eq!(buf, vec![7, 8, 9]);
    }

    #[test]
    fn compact_suffix_reuses_existing_allocation() {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let ptr_before = buf.as_ptr();
        let cap_before = buf.capacity();
        compact_suffix(&mut buf, 2);
        assert_eq!(buf, vec![5, 6]);
        assert_eq!(buf.as_ptr(), ptr_before);
        assert_eq!(buf.capacity(), cap_before);
    }

    #[test]
    fn compact_suffix_overlapping_ranges_move_correctly() {
        let mut buf = vec![1u8, 2, 3, 4, 5, 6];
        compact_suffix(&mut buf, 4);
        assert_eq!(buf, vec![3, 4, 5, 6]);
    }
}
