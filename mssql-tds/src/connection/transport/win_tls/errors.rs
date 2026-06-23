// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Map Schannel `SECURITY_STATUS` and Win32 last-error codes into the crate's
//! error type so the dispatcher can report clean diagnostics.

use std::io;

use windows_sys::Win32::Foundation;

/// Format a Schannel `SECURITY_STATUS` value as a human-readable string.
/// Tracks the codes we expect to see; falls back to hex for anything else.
pub(crate) fn sec_status_message(status: i32) -> &'static str {
    // `windows-sys` exposes `SEC_*` constants as `u32`/`WIN32_ERROR` while
    // `InitializeSecurityContextW` returns `i32` (HRESULT-like).
    match status as u32 {
        x if x == Foundation::SEC_E_OK as u32 => "SEC_E_OK",
        x if x == Foundation::SEC_I_CONTINUE_NEEDED as u32 => "SEC_I_CONTINUE_NEEDED",
        x if x == Foundation::SEC_E_INCOMPLETE_MESSAGE as u32 => "SEC_E_INCOMPLETE_MESSAGE",
        x if x == Foundation::SEC_I_CONTEXT_EXPIRED as u32 => "SEC_I_CONTEXT_EXPIRED",
        x if x == Foundation::SEC_I_RENEGOTIATE as u32 => "SEC_I_RENEGOTIATE",
        x if x == Foundation::SEC_E_INVALID_TOKEN as u32 => "SEC_E_INVALID_TOKEN",
        x if x == Foundation::SEC_E_INVALID_HANDLE as u32 => "SEC_E_INVALID_HANDLE",
        x if x == Foundation::SEC_E_INTERNAL_ERROR as u32 => "SEC_E_INTERNAL_ERROR",
        x if x == Foundation::SEC_E_NO_AUTHENTICATING_AUTHORITY as u32 => {
            "SEC_E_NO_AUTHENTICATING_AUTHORITY"
        }
        x if x == Foundation::SEC_E_TARGET_UNKNOWN as u32 => "SEC_E_TARGET_UNKNOWN",
        x if x == Foundation::SEC_E_LOGON_DENIED as u32 => "SEC_E_LOGON_DENIED",
        x if x == Foundation::SEC_E_DECRYPT_FAILURE as u32 => "SEC_E_DECRYPT_FAILURE",
        x if x == Foundation::SEC_E_MESSAGE_ALTERED as u32 => "SEC_E_MESSAGE_ALTERED",
        x if x == Foundation::SEC_E_OUT_OF_SEQUENCE as u32 => "SEC_E_OUT_OF_SEQUENCE",
        x if x == Foundation::SEC_E_ALGORITHM_MISMATCH as u32 => "SEC_E_ALGORITHM_MISMATCH",
        x if x == Foundation::SEC_E_BUFFER_TOO_SMALL as u32 => "SEC_E_BUFFER_TOO_SMALL",
        x if x == Foundation::SEC_E_UNTRUSTED_ROOT as u32 => "SEC_E_UNTRUSTED_ROOT",
        x if x == Foundation::SEC_E_WRONG_PRINCIPAL as u32 => "SEC_E_WRONG_PRINCIPAL",
        x if x == Foundation::SEC_E_CERT_EXPIRED as u32 => "SEC_E_CERT_EXPIRED",
        x if x == Foundation::SEC_E_CERT_UNKNOWN as u32 => "SEC_E_CERT_UNKNOWN",
        _ => "unknown SECURITY_STATUS",
    }
}

/// Turn a non-success `SECURITY_STATUS` into an `io::Error` with an attached
/// symbolic name for diagnostics.
pub(crate) fn sec_status_to_io_error(status: i32, context: &str) -> io::Error {
    io::Error::other(format!(
        "{context}: {} (status=0x{:08X})",
        sec_status_message(status),
        status as u32
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN: &[(i32, &str)] = &[
        (Foundation::SEC_E_OK, "SEC_E_OK"),
        (Foundation::SEC_I_CONTINUE_NEEDED, "SEC_I_CONTINUE_NEEDED"),
        (
            Foundation::SEC_E_INCOMPLETE_MESSAGE,
            "SEC_E_INCOMPLETE_MESSAGE",
        ),
        (Foundation::SEC_I_CONTEXT_EXPIRED, "SEC_I_CONTEXT_EXPIRED"),
        (Foundation::SEC_I_RENEGOTIATE, "SEC_I_RENEGOTIATE"),
        (Foundation::SEC_E_INVALID_TOKEN, "SEC_E_INVALID_TOKEN"),
        (Foundation::SEC_E_INVALID_HANDLE, "SEC_E_INVALID_HANDLE"),
        (Foundation::SEC_E_INTERNAL_ERROR, "SEC_E_INTERNAL_ERROR"),
        (
            Foundation::SEC_E_NO_AUTHENTICATING_AUTHORITY,
            "SEC_E_NO_AUTHENTICATING_AUTHORITY",
        ),
        (Foundation::SEC_E_TARGET_UNKNOWN, "SEC_E_TARGET_UNKNOWN"),
        (Foundation::SEC_E_LOGON_DENIED, "SEC_E_LOGON_DENIED"),
        (Foundation::SEC_E_DECRYPT_FAILURE, "SEC_E_DECRYPT_FAILURE"),
        (Foundation::SEC_E_MESSAGE_ALTERED, "SEC_E_MESSAGE_ALTERED"),
        (Foundation::SEC_E_OUT_OF_SEQUENCE, "SEC_E_OUT_OF_SEQUENCE"),
        (
            Foundation::SEC_E_ALGORITHM_MISMATCH,
            "SEC_E_ALGORITHM_MISMATCH",
        ),
        (Foundation::SEC_E_BUFFER_TOO_SMALL, "SEC_E_BUFFER_TOO_SMALL"),
        (Foundation::SEC_E_UNTRUSTED_ROOT, "SEC_E_UNTRUSTED_ROOT"),
        (Foundation::SEC_E_WRONG_PRINCIPAL, "SEC_E_WRONG_PRINCIPAL"),
        (Foundation::SEC_E_CERT_EXPIRED, "SEC_E_CERT_EXPIRED"),
        (Foundation::SEC_E_CERT_UNKNOWN, "SEC_E_CERT_UNKNOWN"),
    ];

    #[test]
    fn sec_status_message_maps_every_known_code() {
        for (code, name) in KNOWN {
            assert_eq!(
                sec_status_message(*code),
                *name,
                "code 0x{:08X}",
                *code as u32
            );
        }
    }

    #[test]
    fn sec_status_message_unknown_falls_back() {
        assert_eq!(sec_status_message(0x1234_5678), "unknown SECURITY_STATUS");
    }

    #[test]
    fn sec_status_to_io_error_carries_context_name_and_hex() {
        let err = sec_status_to_io_error(Foundation::SEC_E_UNTRUSTED_ROOT, "TLS handshake");
        assert_eq!(err.kind(), io::ErrorKind::Other);
        let msg = err.to_string();
        assert!(msg.contains("TLS handshake"), "missing context: {msg}");
        assert!(msg.contains("SEC_E_UNTRUSTED_ROOT"), "missing name: {msg}");
        let hex = format!("{:08X}", Foundation::SEC_E_UNTRUSTED_ROOT as u32);
        assert!(msg.contains(&hex), "missing hex {hex}: {msg}");
    }
}
