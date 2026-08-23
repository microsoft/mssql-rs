// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Certificate validation module for ServerCertificate connection keyword
//!
//! This module implements certificate pinning by performing exact binary matching
//! between a user-provided certificate file and the server's certificate during
//! the SSL/TLS handshake.

use crate::core::TdsResult;
use crate::error::Error;
use native_tls::Certificate;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

/// Load a certificate from a file path and convert to DER format.
/// Supports both DER and PEM encoded X.509 certificates.
/// Uses native-tls Certificate API for automatic format detection and conversion.
///
/// # Arguments
/// * `path` - Path to the certificate file
///
/// # Returns
/// * `Ok(Vec<u8>)` - DER-encoded certificate data
/// * `Err(Error)` - File not found, IO error, or invalid format
pub fn load_certificate_from_file(path: &Path) -> TdsResult<Vec<u8>> {
    debug!("Loading certificate from file: {path:?}");

    // Check if file exists
    if !path.exists() {
        return Err(Error::CertificateNotFound {
            path: path.to_path_buf(),
        });
    }

    // Read certificate file
    let cert_data = fs::read(path).map_err(|e| Error::CertificateFileIoError {
        path: path.to_path_buf(),
        error: e.to_string(),
    })?;

    // Try to parse as PEM first, fall back to DER
    // native-tls handles the format detection and parsing
    let certificate = Certificate::from_pem(&cert_data)
        .or_else(|_| {
            debug!("Not PEM format, trying DER");
            Certificate::from_der(&cert_data)
        })
        .map_err(|_| Error::InvalidCertificateFormat {
            path: path.to_path_buf(),
        })?;

    // Convert to DER format for binary comparison
    let der_data = certificate
        .to_der()
        .map_err(|_| Error::InvalidCertificateFormat {
            path: path.to_path_buf(),
        })?;

    info!(
        "Successfully loaded certificate from: {path:?} ({} bytes)",
        der_data.len()
    );
    Ok(der_data)
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
/// * `user_cert_path` - Path to user-provided certificate file
/// * `server_cert_der` - DER-encoded server certificate from TLS handshake
///
/// # Returns
/// * `Ok(())` - Certificates match and server cert is valid
/// * `Err(Error)` - Validation failed
pub fn validate_server_certificate(user_cert_path: &Path, server_cert_der: &[u8]) -> TdsResult<()> {
    info!("Validating server certificate against: {user_cert_path:?}");

    // Step 1: Load user-provided certificate
    let user_cert_der = load_certificate_from_file(user_cert_path)?;

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
        let result = load_certificate_from_file(p);
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
        let result = load_certificate_from_file(cert_path);

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
        let result = load_certificate_from_file(cert_path);

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
        let result = load_certificate_from_file(cert_path);

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

        let pem_result = load_certificate_from_file(pem_path);
        let der_result = load_certificate_from_file(der_path);

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
        let der_bytes =
            load_certificate_from_file(cert_path).expect("Failed to load test certificate");

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
}
