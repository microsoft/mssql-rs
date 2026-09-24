// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Post-handshake server-certificate validation.
//!
//! Most of the validation work for the Schannel-direct path is already done
//! by the cred-kind / ISC-flag combination chosen in [`super::cred`] and
//! [`super::handshake`]:
//!
//! - [`CredKind::AutoValidate`]: SChannel performs full chain build + SSL
//!   policy check + hostname match inline during `InitializeSecurityContextW`
//!   (because we pass the target name and do NOT pass
//!   `ISC_REQ_MANUAL_CRED_VALIDATION`). No extra work after the handshake.
//! - [`CredKind::NoValidate`]: nothing to do. This is the
//!   `TrustServerCertificate=Yes` path that bypasses chain build entirely
//!   (the fix for Adam's regression).
//! - [`CredKind::ManualValidate`]: file-pin mode. We extract the server's
//!   DER and hand it to
//!   [`certificate_validator::validate_server_certificate`], which does the
//!   constant-time DER comparison and time-validity check already shipped
//!   with the native-tls path.
//!
//! This module's job is purely the SChannel-side glue:
//! [`query_remote_cert_der`] plus a [`validate_after_handshake`] dispatcher
//! that calls the right branch.

use std::io;
use std::ptr;

use tracing::debug;
use windows_sys::Win32::Foundation;
use windows_sys::Win32::Security::Authentication::Identity;
use windows_sys::Win32::Security::Cryptography;

use super::cred::CredKind;
use super::errors::sec_status_to_io_error;
use super::handshake::SecCtx;
use crate::core::CertificateSource;

/// Extract the server's leaf certificate in DER form from a completed
/// security context.
///
/// Wraps `QueryContextAttributesW(SECPKG_ATTR_REMOTE_CERT_CONTEXT)`
/// followed by a defensive copy out of the returned `CERT_CONTEXT` and a
/// matched `CertFreeCertificateContext` to release the SSPI-owned handle.
pub(crate) fn query_remote_cert_der(ctx: &SecCtx) -> io::Result<Vec<u8>> {
    let mut cert_ctx_ptr: *mut Cryptography::CERT_CONTEXT = ptr::null_mut();
    // SAFETY: ctx is initialized; SECPKG_ATTR_REMOTE_CERT_CONTEXT returns
    // a PCCERT_CONTEXT that the caller must release with
    // CertFreeCertificateContext.
    let status = unsafe {
        Identity::QueryContextAttributesW(
            ctx.raw(),
            Identity::SECPKG_ATTR_REMOTE_CERT_CONTEXT,
            &mut cert_ctx_ptr as *mut _ as *mut _,
        )
    };
    if status != Foundation::SEC_E_OK {
        return Err(sec_status_to_io_error(
            status,
            "QueryContextAttributesW(REMOTE_CERT_CONTEXT) failed",
        ));
    }
    if cert_ctx_ptr.is_null() {
        return Err(io::Error::other(
            "QueryContextAttributesW returned SEC_E_OK with null CERT_CONTEXT",
        ));
    }

    // SAFETY: cert_ctx_ptr is non-null and valid; pbCertEncoded /
    // cbCertEncoded describe the DER bytes.
    let der = unsafe {
        let cert_ctx = &*cert_ctx_ptr;
        std::slice::from_raw_parts(cert_ctx.pbCertEncoded, cert_ctx.cbCertEncoded as usize).to_vec()
    };

    // SAFETY: cert_ctx_ptr came from a successful Query and we own one
    // reference; free it.
    unsafe {
        Cryptography::CertFreeCertificateContext(cert_ctx_ptr);
    }

    Ok(der)
}

/// Apply post-handshake validation appropriate for `kind`.
///
/// Called by the engine ([`super::engine`]) immediately
/// after `SchannelTlsStream::connect` returns successfully but before the
/// stream is exposed to TDS-layer code.
pub(crate) fn validate_after_handshake(
    ctx: &SecCtx,
    kind: CredKind,
    pinned_certificate: Option<&CertificateSource>,
) -> Result<(), ValidationError> {
    match (kind, pinned_certificate) {
        (CredKind::AutoValidate, _) => {
            // Already validated inline by SChannel. Nothing to do.
            debug!("win_tls: validate skipped (AutoValidate; SChannel did chain build inline)");
            Ok(())
        }
        (CredKind::NoValidate, _) => {
            // TrustServerCertificate=Yes / LoginOnly: validation
            // intentionally skipped. This is the Adam-fix path.
            debug!("win_tls: validate skipped (NoValidate; TrustServerCertificate=Yes)");
            Ok(())
        }
        (CredKind::ManualValidate, Some(pinned)) => {
            let der = query_remote_cert_der(ctx).map_err(ValidationError::QueryCert)?;
            validate_pinned_cert(pinned, &der)
        }
        (CredKind::ManualValidate, None) => Err(ValidationError::ConfigMismatch(
            "CredKind::ManualValidate requires a pinned certificate".to_string(),
        )),
    }
}

/// Compare a queried server-cert DER against the user's pinned certificate
/// file.
///
/// Split out from [`validate_after_handshake`] so the pin decision and its
/// logging are exercisable without a live Schannel context (the FFI in
/// [`query_remote_cert_der`] cannot be driven from a unit test).
fn validate_pinned_cert(pinned: &CertificateSource, der: &[u8]) -> Result<(), ValidationError> {
    let result = super::super::certificate_validator::validate_server_certificate(pinned, der)
        .map_err(ValidationError::Pin);
    match &result {
        Ok(()) => debug!(
            der_len = der.len(),
            "win_tls: validate (ManualValidate) pin match OK"
        ),
        Err(e) => {
            debug!(der_len = der.len(), error = %e, "win_tls: validate (ManualValidate) pin FAILED")
        }
    }
    result
}

/// Errors produced by [`validate_after_handshake`].
#[derive(Debug)]
pub(crate) enum ValidationError {
    /// `QueryContextAttributesW` failed.
    QueryCert(io::Error),
    /// File-pin DER mismatch / load error / expired cert.
    Pin(crate::error::Error),
    /// Caller wired up the validation path incorrectly.
    ConfigMismatch(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueryCert(e) => write!(f, "failed to fetch remote certificate: {e}"),
            Self::Pin(e) => write!(f, "certificate pin validation failed: {e}"),
            Self::ConfigMismatch(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ValidationError {}

impl From<ValidationError> for crate::error::Error {
    fn from(e: ValidationError) -> Self {
        match e {
            ValidationError::Pin(inner) => inner,
            other => crate::error::Error::ImplementationError(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn novalidate_is_a_noop() {
        let dummy = SecCtx::for_test_only();
        let r = validate_after_handshake(&dummy, CredKind::NoValidate, None);
        assert!(r.is_ok());
    }

    #[test]
    fn autovalidate_is_a_noop_too() {
        let dummy = SecCtx::for_test_only();
        let r = validate_after_handshake(&dummy, CredKind::AutoValidate, None);
        assert!(r.is_ok());
    }

    #[test]
    fn manualvalidate_without_path_is_config_error() {
        let dummy = SecCtx::for_test_only();
        let r = validate_after_handshake(&dummy, CredKind::ManualValidate, None);
        assert!(matches!(r, Err(ValidationError::ConfigMismatch(_))));
    }

    #[test]
    fn manualvalidate_with_path_on_dummy_ctx_fails_to_query_cert() {
        // The dummy context has a zeroed handle, so QueryContextAttributesW
        // returns SEC_E_INVALID_HANDLE. Exercises the query_remote_cert_der
        // error branch and the ManualValidate+Some(path) arm.
        let dummy = SecCtx::for_test_only();
        let pinned = CertificateSource::File("nonexistent-cert.cer".into());
        let r = validate_after_handshake(&dummy, CredKind::ManualValidate, Some(&pinned));
        assert!(matches!(r, Err(ValidationError::QueryCert(_))));
    }

    #[test]
    fn query_remote_cert_der_on_dummy_ctx_errors() {
        // Zeroed handle => SEC_E_INVALID_HANDLE, so the SEC_E_OK guard fails
        // and we get the QueryContextAttributesW error path.
        let dummy = SecCtx::for_test_only();
        assert!(query_remote_cert_der(&dummy).is_err());
    }

    fn fixture(name: &str) -> CertificateSource {
        CertificateSource::File(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("test_certificates")
                .join(name),
        )
    }

    fn fixture_bytes(name: &str) -> Vec<u8> {
        match fixture(name) {
            CertificateSource::File(path) => std::fs::read(path).expect("read der fixture"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn validate_pinned_cert_matches_identical_der() {
        // Pin file and queried DER are the same (non-expired) certificate, so
        // the pin check succeeds. Covers the ManualValidate happy path that the
        // dummy-context tests cannot reach.
        let der = fixture_bytes("valid_cert.der");
        let r = validate_pinned_cert(&fixture("valid_cert.der"), &der);
        assert!(r.is_ok(), "expected pin match, got {r:?}");
    }

    #[test]
    fn validate_pinned_cert_missing_pin_file_is_pin_error() {
        // Pin path does not exist => the validator fails to load the user cert
        // and the error is surfaced as ValidationError::Pin.
        let der = fixture_bytes("valid_cert.der");
        let r = validate_pinned_cert(
            &CertificateSource::File("definitely-missing-pin.cer".into()),
            &der,
        );
        assert!(matches!(r, Err(ValidationError::Pin(_))));
    }

    #[test]
    fn validate_pinned_cert_mismatch_is_pin_error() {
        // A DER that differs from the pinned certificate is rejected as a Pin
        // error (whether it trips the expiry-parse step or the constant-time
        // comparison, both map to ValidationError::Pin).
        let mut der = fixture_bytes("valid_cert.der");
        let mid = der.len() / 2;
        der[mid] ^= 0xFF;
        let r = validate_pinned_cert(&fixture("valid_cert.der"), &der);
        assert!(matches!(r, Err(ValidationError::Pin(_))));
    }

    #[test]
    fn validation_error_display_renders_each_variant() {
        let q = ValidationError::QueryCert(io::Error::other("boom"));
        assert!(q.to_string().contains("fetch remote certificate"));

        let p = ValidationError::Pin(crate::error::Error::NoServerCertificate);
        assert!(p.to_string().contains("pin validation failed"));

        let c = ValidationError::ConfigMismatch("wired wrong".to_string());
        assert_eq!(c.to_string(), "wired wrong");
    }

    #[test]
    fn validation_error_converts_to_crate_error() {
        // Pin passes the inner error through unchanged.
        let pin = ValidationError::Pin(crate::error::Error::NoServerCertificate);
        assert!(matches!(
            crate::error::Error::from(pin),
            crate::error::Error::NoServerCertificate
        ));

        // Non-pin variants collapse into ImplementationError.
        let cfg = ValidationError::ConfigMismatch("nope".to_string());
        assert!(matches!(
            crate::error::Error::from(cfg),
            crate::error::Error::ImplementationError(_)
        ));

        let q = ValidationError::QueryCert(io::Error::other("boom"));
        assert!(matches!(
            crate::error::Error::from(q),
            crate::error::Error::ImplementationError(_)
        ));
    }
}
