// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Certificate validation module for ServerCertificate connection keyword
//!
//! This module implements certificate pinning by performing exact binary matching
//! between a user-provided certificate file and the server's certificate during
//! the SSL/TLS handshake.

use crate::core::{CertificateSource, TdsResult};
use crate::error::Error;
use native_tls::Certificate;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

/// Reads a certificate file, distinguishing a missing path from other I/O
/// failures.
fn read_certificate_file(path: &Path) -> TdsResult<Vec<u8>> {
    fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::CertificateNotFound {
            path: path.to_path_buf(),
        },
        _ => Error::CertificateFileIoError {
            path: path.to_path_buf(),
            error: e.to_string(),
        },
    })
}

/// Load a single certificate as DER. Files may be PEM or DER; only the first
/// certificate of a PEM file is used.
pub fn load_certificate(source: &CertificateSource) -> TdsResult<Vec<u8>> {
    let certificate = match source {
        CertificateSource::File(path) => {
            debug!("Loading certificate from file: {path:?}");
            let invalid_format = || Error::InvalidCertificateFormat { path: path.clone() };
            let data = read_certificate_file(path)?;
            let certificate = Certificate::from_pem(&data)
                .or_else(|_| Certificate::from_der(&data))
                .map_err(|_| invalid_format())?;
            let der = certificate.to_der().map_err(|_| invalid_format())?;
            info!("Loaded certificate from: {path:?} ({} bytes)", der.len());
            return Ok(der);
        }
        CertificateSource::Pem(data) => Certificate::from_pem(data)
            .map_err(|_| Error::InvalidCertificateData { expected: "PEM" })?,
        CertificateSource::Der(data) => Certificate::from_der(data)
            .map_err(|_| Error::InvalidCertificateData { expected: "DER" })?,
    };
    certificate
        .to_der()
        .map_err(|_| Error::InvalidCertificateData { expected: "DER" })
}

/// Load one or more CA certificates for use as trust roots. Files may be a
/// PEM bundle or a single DER certificate; every PEM block is loaded.
pub fn load_ca_certificates(source: &CertificateSource) -> TdsResult<Vec<Certificate>> {
    let (data, path) = match source {
        CertificateSource::File(path) => {
            debug!("Loading CA certificate(s) from file: {path:?}");
            (
                std::borrow::Cow::Owned(read_certificate_file(path)?),
                Some(path),
            )
        }
        CertificateSource::Pem(data) | CertificateSource::Der(data) => {
            (std::borrow::Cow::Borrowed(data.as_slice()), None)
        }
    };
    let invalid = |expected| match path {
        Some(path) => Error::InvalidCertificateFormat { path: path.clone() },
        None => Error::InvalidCertificateData { expected },
    };

    let certificates = match (source, split_pem_certificates(&data)) {
        (CertificateSource::Der(_), _) => {
            vec![Certificate::from_der(&data).map_err(|_| invalid("DER"))?]
        }
        (_, Some(blocks)) => blocks
            .iter()
            .map(|block| Certificate::from_pem(block).map_err(|_| invalid("PEM")))
            .collect::<TdsResult<Vec<_>>>()?,
        (CertificateSource::Pem(_), None) => return Err(invalid("PEM")),
        (_, None) => vec![Certificate::from_der(&data).map_err(|_| invalid("PEM or DER"))?],
    };

    if certificates.is_empty() {
        return Err(invalid("PEM or DER"));
    }
    info!("Loaded {} CA certificate(s)", certificates.len());
    Ok(certificates)
}

/// Split PEM data into individual certificate blocks so bundles are fully
/// loaded (`Certificate::from_pem` only parses the first block).
/// Returns `None` when the data contains no PEM certificate header.
fn split_pem_certificates(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let text = std::str::from_utf8(data).ok()?;
    if !text.contains(BEGIN) {
        return None;
    }

    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let after_begin = &rest[start..];
        let end = after_begin.find(END)? + END.len();
        blocks.push(after_begin.as_bytes()[..end].to_vec());
        rest = &after_begin[end..];
    }
    Some(blocks)
}

/// Check if a certificate has expired.
/// Uses the X.509 notAfter field to determine expiry.
///
/// # Arguments
/// * `der_data` - DER-encoded certificate data
///
/// # Returns
/// * `Ok(true)` - Certificate has expired
/// * `Ok(false)` - Certificate is still valid
/// * `Err(Error)` - Unable to parse certificate
pub fn is_certificate_expired(der_data: &[u8]) -> TdsResult<bool> {
    use x509_parser::prelude::*;

    let (_, cert) = X509Certificate::from_der(der_data).map_err(|e| {
        Error::ProtocolError(format!(
            "Failed to parse certificate for expiry check: {}",
            e
        ))
    })?;

    // Get the current time
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| Error::ImplementationError(format!("System time error: {}", e)))?;

    // Get certificate validity period
    let not_after = cert.validity().not_after.timestamp();

    // Check if expired
    Ok(now.as_secs() as i64 > not_after)
}

/// Perform constant-time binary comparison of two byte arrays.
/// This prevents timing attacks during certificate validation.
///
/// # Arguments
/// * `a` - First byte array
/// * `b` - Second byte array
///
/// # Returns
/// * `true` if arrays are identical
/// * `false` if arrays differ in size or content
pub fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    // First check sizes (this is not timing-sensitive)
    if a.len() != b.len() {
        return false;
    }

    // Constant-time comparison of contents
    // Use XOR and accumulation to avoid short-circuit evaluation
    let mut result = 0u8;
    for (byte_a, byte_b) in a.iter().zip(b.iter()) {
        result |= byte_a ^ byte_b;
    }

    result == 0
}

/// Validate server certificate against user-provided certificate.
/// Performs expiry check and exact binary match.
///
/// # Arguments
/// * `pinned` - Certificate the server must present
/// * `server_cert_der` - DER-encoded server certificate from TLS handshake
///
/// # Returns
/// * `Ok(())` - Certificates match and server cert is valid
/// * `Err(Error)` - Validation failed
pub fn validate_server_certificate(
    pinned: &CertificateSource,
    server_cert_der: &[u8],
) -> TdsResult<()> {
    info!("Validating server certificate against pinned certificate");

    // Step 1: Load user-provided certificate
    let user_cert_der = load_certificate(pinned)?;

    // Step 2: Check server certificate expiry
    if is_certificate_expired(server_cert_der)? {
        return Err(Error::CertificateExpired);
    }

    // Step 3: Perform exact binary match
    if !constant_time_compare(&user_cert_der, server_cert_der) {
        debug!(
            "Certificate mismatch: user cert size={}, server cert size={}",
            user_cert_der.len(),
            server_cert_der.len()
        );
        return Err(Error::CertificateMismatch);
    }

    info!("Server certificate validation successful");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_certificate_file(path: impl AsRef<Path>) -> TdsResult<Vec<u8>> {
        load_certificate(&CertificateSource::File(path.as_ref().to_path_buf()))
    }

    fn load_ca_file(path: impl AsRef<Path>) -> TdsResult<Vec<Certificate>> {
        load_ca_certificates(&CertificateSource::File(path.as_ref().to_path_buf()))
    }

    #[test]
    fn test_constant_time_compare_equal() {
        let a = vec![1, 2, 3, 4, 5];
        let b = vec![1, 2, 3, 4, 5];
        assert!(constant_time_compare(&a, &b));
    }

    #[test]
    fn test_constant_time_compare_different() {
        let a = vec![1, 2, 3, 4, 5];
        let b = vec![1, 2, 3, 4, 6];
        assert!(!constant_time_compare(&a, &b));
    }

    #[test]
    fn test_constant_time_compare_different_sizes() {
        let a = vec![1, 2, 3];
        let b = vec![1, 2, 3, 4];
        assert!(!constant_time_compare(&a, &b));
    }

    #[test]
    fn test_load_certificate_file_not_found() {
        let p = Path::new("/nonexistent/path/cert.cer");
        let result = load_certificate_file(p);
        assert!(result.is_err());
        match result {
            Err(Error::CertificateNotFound { path }) => {
                assert_eq!(path, p);
            }
            _ => panic!("Expected CertificateNotFound error"),
        }
    }

    #[test]
    fn test_load_certificate_from_pem() {
        // Path relative to the crate root
        let cert_path = Path::new("tests/test_certificates/valid_cert.pem");
        let result = load_certificate_file(cert_path);

        match result {
            Ok(der_bytes) => {
                // Verify we got a DER-encoded certificate
                assert!(!der_bytes.is_empty(), "DER bytes should not be empty");
                // DER certificates typically start with 0x30 (SEQUENCE tag)
                assert_eq!(
                    der_bytes[0], 0x30,
                    "DER certificate should start with SEQUENCE tag"
                );
            }
            Err(e) => panic!("Failed to load PEM certificate: {:?}", e),
        }
    }

    #[test]
    fn test_load_certificate_from_der() {
        // Path relative to the crate root
        let cert_path = Path::new("tests/test_certificates/valid_cert.der");
        let result = load_certificate_file(cert_path);

        match result {
            Ok(der_bytes) => {
                // Verify we got a DER-encoded certificate
                assert!(!der_bytes.is_empty(), "DER bytes should not be empty");
                // DER certificates typically start with 0x30 (SEQUENCE tag)
                assert_eq!(
                    der_bytes[0], 0x30,
                    "DER certificate should start with SEQUENCE tag"
                );
            }
            Err(e) => panic!("Failed to load DER certificate: {:?}", e),
        }
    }

    #[test]
    fn test_load_certificate_invalid_format() {
        // Path relative to the crate root
        let cert_path = Path::new("tests/test_certificates/invalid_format.txt");
        let result = load_certificate_file(cert_path);

        assert!(result.is_err(), "Should fail to load invalid certificate");
        match result {
            Err(Error::InvalidCertificateFormat { path, .. }) => {
                assert_eq!(path, cert_path);
            }
            Err(e) => panic!("Expected InvalidCertificateFormat error, got: {:?}", e),
            Ok(_) => panic!("Should not succeed loading invalid certificate"),
        }
    }

    #[test]
    fn test_pem_and_der_certificates_produce_same_der() {
        // Both PEM and DER files contain the same certificate
        let pem_path = Path::new("tests/test_certificates/valid_cert.pem");
        let der_path = Path::new("tests/test_certificates/valid_cert.der");

        let pem_result = load_certificate_file(pem_path);
        let der_result = load_certificate_file(der_path);

        assert!(
            pem_result.is_ok(),
            "PEM certificate should load successfully"
        );
        assert!(
            der_result.is_ok(),
            "DER certificate should load successfully"
        );

        let pem_der = pem_result.unwrap();
        let der_der = der_result.unwrap();

        // Both should produce the same DER encoding
        assert_eq!(
            pem_der, der_der,
            "PEM and DER files should produce identical DER encodings"
        );
    }

    #[test]
    fn test_is_certificate_expired_valid() {
        // Our test certificate is valid for 10 years (3650 days)
        let cert_path = Path::new("tests/test_certificates/valid_cert.pem");
        let der_bytes = load_certificate_file(cert_path).expect("Failed to load test certificate");

        let result = is_certificate_expired(&der_bytes);
        assert!(result.is_ok(), "Certificate expiry check should succeed");
        assert!(!result.unwrap(), "Test certificate should not be expired");
    }

    #[test]
    fn test_constant_time_compare_all_zeros() {
        let a = vec![0u8; 100];
        let b = vec![0u8; 100];
        assert!(
            constant_time_compare(&a, &b),
            "All zeros should compare equal"
        );
    }

    #[test]
    fn test_constant_time_compare_single_bit_difference() {
        let mut a = vec![0u8; 32];
        let mut b = vec![0u8; 32];
        b[16] = 0x01; // Single bit difference in the middle

        assert!(
            !constant_time_compare(&a, &b),
            "Single bit difference should be detected"
        );

        // Test at different positions
        a = vec![0u8; 32];
        b = vec![0u8; 32];
        b[0] = 0x80; // First byte
        assert!(
            !constant_time_compare(&a, &b),
            "Difference at start should be detected"
        );

        a = vec![0u8; 32];
        b = vec![0u8; 32];
        b[31] = 0x01; // Last byte
        assert!(
            !constant_time_compare(&a, &b),
            "Difference at end should be detected"
        );
    }

    #[test]
    fn test_constant_time_compare_empty_slices() {
        let a: Vec<u8> = vec![];
        let b: Vec<u8> = vec![];
        assert!(
            constant_time_compare(&a, &b),
            "Empty slices should compare equal"
        );
    }

    #[test]
    fn test_load_ca_certificates_from_pem() {
        let certs = load_ca_file(Path::new("tests/test_certificates/valid_cert.pem"))
            .expect("PEM CA certificate should load");
        assert_eq!(certs.len(), 1);
    }

    #[test]
    fn test_load_ca_certificates_from_der() {
        let certs = load_ca_file(Path::new("tests/test_certificates/valid_cert.der"))
            .expect("DER CA certificate should load");
        assert_eq!(certs.len(), 1);
    }

    #[test]
    fn test_load_ca_certificates_from_pem_bundle() {
        let single = fs::read("tests/test_certificates/valid_cert.pem")
            .expect("test certificate should exist");
        let bundle_path = std::env::temp_dir().join("mssql_tds_ca_bundle_test.pem");
        let mut bundle = single.clone();
        bundle.extend_from_slice(&single);
        fs::write(&bundle_path, &bundle).expect("bundle should be writable");

        let certs = load_ca_file(&bundle_path).expect("PEM bundle should load fully");
        assert_eq!(certs.len(), 2);

        let _ = fs::remove_file(&bundle_path);
    }

    #[test]
    fn test_load_ca_certificates_not_found() {
        let path = Path::new("/nonexistent/path/ca.pem");
        assert!(matches!(
            load_ca_file(path),
            Err(Error::CertificateNotFound { .. })
        ));
    }

    #[test]
    fn test_load_ca_certificates_io_error() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_ca_file(directory.path()),
            Err(Error::CertificateFileIoError { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_load_ca_certificates_metadata_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("loop.pem");
        std::os::unix::fs::symlink("loop.pem", &path).unwrap();
        assert!(matches!(
            load_ca_file(&path),
            Err(Error::CertificateFileIoError { .. })
        ));
    }

    #[test]
    fn test_load_ca_certificates_invalid_format() {
        let path = Path::new("tests/test_certificates/invalid_format.txt");
        assert!(matches!(
            load_ca_file(path),
            Err(Error::InvalidCertificateFormat { .. })
        ));
    }

    #[test]
    fn test_load_ca_certificates_truncated_pem() {
        let truncated_path = std::env::temp_dir().join("mssql_tds_ca_truncated_test.pem");
        fs::write(
            &truncated_path,
            b"-----BEGIN CERTIFICATE-----\nnot a certificate\n",
        )
        .expect("file should be writable");

        assert!(matches!(
            load_ca_file(&truncated_path),
            Err(Error::InvalidCertificateFormat { .. })
        ));

        let _ = fs::remove_file(&truncated_path);
    }

    #[test]
    fn test_in_memory_sources_load() {
        let pem = fs::read("tests/test_certificates/valid_cert.pem").unwrap();
        let der = fs::read("tests/test_certificates/valid_cert.der").unwrap();
        let mut bundle = pem.clone();
        bundle.extend_from_slice(&pem);

        assert_eq!(
            load_certificate(&CertificateSource::Pem(pem.clone())).unwrap(),
            load_certificate(&CertificateSource::Der(der.clone())).unwrap()
        );
        assert_eq!(
            load_ca_certificates(&CertificateSource::Pem(bundle))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            load_ca_certificates(&CertificateSource::Der(der))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_in_memory_sources_reject_wrong_encoding() {
        let der = fs::read("tests/test_certificates/valid_cert.der").unwrap();
        for source in [
            CertificateSource::Pem(der.clone()),
            CertificateSource::Der(b"not a certificate".to_vec()),
        ] {
            assert!(matches!(
                load_ca_certificates(&source),
                Err(Error::InvalidCertificateData { .. })
            ));
            assert!(matches!(
                load_certificate(&source),
                Err(Error::InvalidCertificateData { .. })
            ));
        }
    }
}
