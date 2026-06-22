// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ALPN (Application-Layer Protocol Negotiation) helpers for the
//! Schannel-direct TLS engine.
//!
//! Schannel exposes ALPN in two places:
//!
//! 1. **Request**: on the first call to `InitializeSecurityContextW` the
//!    client passes a `SECBUFFER_APPLICATION_PROTOCOLS` input buffer holding
//!    a [`SEC_APPLICATION_PROTOCOLS`] blob describing the protocol list it
//!    wants the server to choose from. (See MS docs:
//!    <https://learn.microsoft.com/windows/win32/secauthn/application-protocol-negotiation>).
//! 2. **Reply**: after the handshake completes the client queries
//!    `SECPKG_ATTR_APPLICATION_PROTOCOL` to learn what the server selected.
//!
//! For SQL Server TDS 8 (Encrypt=Strict) the client advertises a single
//! protocol id `"tds/8.0"` and the server is expected to echo it. The
//! `mssql-tds` `native-tls` engine logs the negotiated proto but does not
//! fail on mismatch; we match that behaviour here so the two backends are
//! interchangeable.

use std::io;
use std::mem;

use windows_sys::Win32::Foundation;
use windows_sys::Win32::Security::Authentication::Identity;
use windows_sys::Win32::Security::Credentials;

use super::errors::sec_status_to_io_error;

/// Build the on-the-wire `SEC_APPLICATION_PROTOCOLS` blob for the given
/// list of protocol ids.
///
/// The struct layout (see windows-sys `SEC_APPLICATION_PROTOCOLS` /
/// `SEC_APPLICATION_PROTOCOL_LIST`):
///
/// ```text
/// u32  ProtocolListsSize          // size of everything that follows
/// // one SEC_APPLICATION_PROTOCOL_LIST per call site (we only emit one):
/// i32  ProtoNegoExt               // = SecApplicationProtocolNegotiationExt_ALPN
/// u16  ProtocolListSize           // size of the wire ProtocolList bytes
/// u8[] ProtocolList               // each entry: [len_u8, bytes...]
/// ```
///
/// Returns an empty `Vec` if `protocols` is empty (caller should treat
/// that as "do not request ALPN").
pub(crate) fn build_alpn_buffer(protocols: &[&str]) -> Vec<u8> {
    if protocols.is_empty() {
        return Vec::new();
    }

    // Wire-format protocol list: each entry is `len_u8 || bytes`.
    let mut wire_list: Vec<u8> = Vec::new();
    for proto in protocols {
        let bytes = proto.as_bytes();
        debug_assert!(
            bytes.len() <= u8::MAX as usize,
            "ALPN protocol id longer than 255 bytes"
        );
        wire_list.push(bytes.len() as u8);
        wire_list.extend_from_slice(bytes);
    }

    let proto_neg_ext_size = mem::size_of::<i32>(); // ProtoNegoExt
    let proto_list_size_size = mem::size_of::<u16>(); // ProtocolListSize
    let list_struct_size = proto_neg_ext_size + proto_list_size_size + wire_list.len();

    let mut out = Vec::with_capacity(mem::size_of::<u32>() + list_struct_size);
    // ProtocolListsSize
    out.extend_from_slice(&(list_struct_size as u32).to_le_bytes());
    // ProtoNegoExt
    out.extend_from_slice(&Identity::SecApplicationProtocolNegotiationExt_ALPN.to_le_bytes());
    // ProtocolListSize
    out.extend_from_slice(&(wire_list.len() as u16).to_le_bytes());
    // ProtocolList
    out.extend_from_slice(&wire_list);
    out
}

/// Query the protocol the server selected after a successful handshake.
///
/// Returns:
/// - `Ok(Some(proto))` — server selected `proto` via ALPN.
/// - `Ok(None)` — server did not negotiate any ALPN protocol (either it
///   didn't see our extension or it chose to ignore it).
/// - `Err(_)` — the SSPI call failed.
pub(crate) fn query_negotiated_alpn(ctx: &Credentials::SecHandle) -> io::Result<Option<Vec<u8>>> {
    // SAFETY: `SecPkgContext_ApplicationProtocol` is a fixed-size
    // POD struct; SSPI fills it in. `ctx` is a valid initialized
    // security context.
    let mut info: Identity::SecPkgContext_ApplicationProtocol = unsafe { mem::zeroed() };
    let status = unsafe {
        Identity::QueryContextAttributesW(
            ctx as *const _,
            Identity::SECPKG_ATTR_APPLICATION_PROTOCOL,
            &mut info as *mut _ as *mut _,
        )
    };
    if status != Foundation::SEC_E_OK {
        return Err(sec_status_to_io_error(
            status,
            "QueryContextAttributesW(SECPKG_ATTR_APPLICATION_PROTOCOL) failed",
        ));
    }
    Ok(interpret_alpn_result(&info))
}

/// Interpret a populated `SecPkgContext_ApplicationProtocol` into the
/// negotiated protocol id.
///
/// Split out from [`query_negotiated_alpn`] so the result-decoding logic is
/// testable without a live security context (the `QueryContextAttributesW`
/// FFI in [`query_negotiated_alpn`] cannot be driven from a unit test).
///
/// Returns `None` when the server didn't negotiate a protocol or reported a
/// zero / out-of-range id length.
fn interpret_alpn_result(info: &Identity::SecPkgContext_ApplicationProtocol) -> Option<Vec<u8>> {
    if info.ProtoNegoStatus != Identity::SecApplicationProtocolNegotiationStatus_Success {
        return None;
    }
    let len = info.ProtocolIdSize as usize;
    if len == 0 || len > info.ProtocolId.len() {
        return None;
    }
    Some(info.ProtocolId[..len].to_vec())
}

/// Convenience: stringify a negotiated-ALPN query result for logging.
pub(crate) fn debug_proto(proto: &[u8]) -> String {
    match std::str::from_utf8(proto) {
        Ok(s) => s.to_string(),
        Err(_) => format!("{:02x?}", proto),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_protocols_yields_empty_buffer() {
        assert!(build_alpn_buffer(&[]).is_empty());
    }

    #[test]
    fn single_protocol_layout_is_correct() {
        let buf = build_alpn_buffer(&["tds/8.0"]);
        // Expected: 4 (ProtocolListsSize) + 4 (ProtoNegoExt) + 2 (ProtocolListSize) + 8 (wire list) = 18
        assert_eq!(buf.len(), 18);

        // ProtocolListsSize = size of everything after the first u32 = 14
        let lists_size = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(lists_size, 14);

        // ProtoNegoExt = ALPN = 2
        let ext = i32::from_le_bytes(buf[4..8].try_into().unwrap());
        assert_eq!(ext, Identity::SecApplicationProtocolNegotiationExt_ALPN);

        // ProtocolListSize = 8 (len byte + 7 chars)
        let list_size = u16::from_le_bytes(buf[8..10].try_into().unwrap());
        assert_eq!(list_size, 8);

        // ProtocolList = [7, 't','d','s','/','8','.','0']
        assert_eq!(buf[10], 7);
        assert_eq!(&buf[11..18], b"tds/8.0");
    }

    #[test]
    fn multiple_protocols_concatenate_with_length_prefixes() {
        let buf = build_alpn_buffer(&["h2", "http/1.1"]);
        // wire list: [2,'h','2', 8,'h','t','t','p','/','1','.','1'] = 12 bytes
        // header: 4 (ProtocolListsSize) + 4 (ProtoNegoExt) + 2 (ProtocolListSize) = 10
        assert_eq!(buf.len(), 10 + 12);
        let list_size = u16::from_le_bytes(buf[8..10].try_into().unwrap());
        assert_eq!(list_size, 12);
        assert_eq!(buf[10], 2);
        assert_eq!(&buf[11..13], b"h2");
        assert_eq!(buf[13], 8);
        assert_eq!(&buf[14..22], b"http/1.1");
    }

    #[test]
    fn debug_proto_renders_utf8_or_hex() {
        assert_eq!(debug_proto(b"tds/8.0"), "tds/8.0");
        // Invalid UTF-8 (lone continuation byte) falls back to hex.
        let s = debug_proto(&[0x80, 0x81]);
        assert!(s.starts_with('['));
    }

    fn alpn_info(status: i32, id: &[u8]) -> Identity::SecPkgContext_ApplicationProtocol {
        // SAFETY: the struct is plain-old-data; zeroing then setting fields is
        // exactly how SSPI hands it back.
        let mut info: Identity::SecPkgContext_ApplicationProtocol = unsafe { mem::zeroed() };
        info.ProtoNegoStatus = status;
        info.ProtocolIdSize = id.len() as u8;
        info.ProtocolId[..id.len()].copy_from_slice(id);
        info
    }

    #[test]
    fn interpret_alpn_result_returns_negotiated_proto() {
        let info = alpn_info(
            Identity::SecApplicationProtocolNegotiationStatus_Success,
            b"tds/8.0",
        );
        assert_eq!(interpret_alpn_result(&info), Some(b"tds/8.0".to_vec()));
    }

    #[test]
    fn interpret_alpn_result_none_when_status_not_success() {
        let info = alpn_info(
            Identity::SecApplicationProtocolNegotiationStatus_None,
            b"tds/8.0",
        );
        assert_eq!(interpret_alpn_result(&info), None);
    }

    #[test]
    fn interpret_alpn_result_none_on_zero_length() {
        let info = alpn_info(
            Identity::SecApplicationProtocolNegotiationStatus_Success,
            &[],
        );
        assert_eq!(interpret_alpn_result(&info), None);
    }

    #[test]
    fn query_negotiated_alpn_on_zeroed_handle_errors() {
        // A zeroed SecHandle => SEC_E_INVALID_HANDLE from
        // QueryContextAttributesW, exercising the FFI error-return path.
        let handle: Credentials::SecHandle = unsafe { mem::zeroed() };
        assert!(query_negotiated_alpn(&handle).is_err());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "ALPN protocol id longer than 255 bytes")]
    fn build_alpn_buffer_rejects_overlong_protocol_id() {
        let overlong = "x".repeat(256);
        let _ = build_alpn_buffer(&[overlong.as_str()]);
    }
}
