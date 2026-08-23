// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Apple-native crypto primitives for Always Encrypted.
//!
//! Used on macOS. RSA operations and key generation go through
//! Security.framework (`SecKey`, via the `security-framework` crate); AES-256-CBC,
//! HMAC-SHA256, and the secure RNG go through CommonCrypto (part of `libSystem`,
//! reached here with small `extern "C"` declarations). Both are approved crypto
//! providers under the compliance policy, mirroring how the TLS layer uses
//! Apple's Secure Transport on this platform instead of OpenSSL. See [`super`]
//! for the platform-selection rationale.

use core_foundation::base::TCFType;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use security_framework::key::{Algorithm, SecKey};
// `KeyType`/`GenerateKeyOptions` are only used by the (test-gated) key generator.
#[cfg(any(test, feature = "test-util"))]
use security_framework::key::{GenerateKeyOptions, KeyType};
use security_framework_sys::item::{
    kSecAttrKeyClass, kSecAttrKeyClassPrivate, kSecAttrKeyType, kSecAttrKeyTypeRSA,
};
use security_framework_sys::key::SecKeyCreateWithData;

use super::der;
use crate::core::TdsResult;
use crate::error::Error;

/// CommonCrypto bindings (`<CommonCrypto/CommonCrypto.h>`). These symbols live in
/// `libSystem`, which is always linked, so no explicit `#[link]` is required.
#[allow(non_camel_case_types)]
#[allow(non_upper_case_globals)]
mod cc {
    use core::ffi::c_void;

    pub type CCCryptorStatus = i32;
    pub type CCOperation = u32;
    pub type CCAlgorithm = u32;
    pub type CCOptions = u32;
    pub type CCHmacAlgorithm = u32;
    pub type CCRNGStatus = i32;

    pub const kCCEncrypt: CCOperation = 0;
    pub const kCCDecrypt: CCOperation = 1;
    pub const kCCAlgorithmAES: CCAlgorithm = 0;
    pub const kCCOptionPKCS7Padding: CCOptions = 1;
    pub const kCCSuccess: CCCryptorStatus = 0;
    pub const kCCHmacAlgSHA256: CCHmacAlgorithm = 2;

    unsafe extern "C" {
        #[allow(clippy::too_many_arguments)]
        pub fn CCCrypt(
            op: CCOperation,
            alg: CCAlgorithm,
            options: CCOptions,
            key: *const c_void,
            key_length: usize,
            iv: *const c_void,
            data_in: *const c_void,
            data_in_length: usize,
            data_out: *mut c_void,
            data_out_available: usize,
            data_out_moved: *mut usize,
        ) -> CCCryptorStatus;

        pub fn CCHmac(
            algorithm: CCHmacAlgorithm,
            key: *const c_void,
            key_length: usize,
            data: *const c_void,
            data_length: usize,
            mac_out: *mut c_void,
        );

        pub fn CCRandomGenerateBytes(bytes: *mut c_void, count: usize) -> CCRNGStatus;
    }
}

/// Maps a Security.framework `CFError` into a column-encryption error.
fn sec_err(context: &str, err: core_foundation::error::CFError) -> Error {
    Error::ColumnEncryptionError(format!("{context}: {err}"))
}

/// `errSecVerifyFailed` (-67808): the OSStatus Security.framework reports when a
/// signature simply does not match the data. This is a routine, expected
/// outcome (e.g. a tampered message), not an operational failure, so it must be
/// mapped to `Ok(false)` rather than propagated as an error.
const ERR_SEC_VERIFY_FAILED: i32 = -67808;

/// Imports a PKCS#1 DER-encoded RSA private key into Security.framework.
fn import_rsa_private_key(pkcs1: &[u8]) -> TdsResult<SecKey> {
    let cf_data = CFData::from_buffer(pkcs1);
    let attrs = CFDictionary::from_CFType_pairs(&[
        (
            unsafe { CFString::wrap_under_get_rule(kSecAttrKeyType) },
            unsafe { CFString::wrap_under_get_rule(kSecAttrKeyTypeRSA) },
        ),
        (
            unsafe { CFString::wrap_under_get_rule(kSecAttrKeyClass) },
            unsafe { CFString::wrap_under_get_rule(kSecAttrKeyClassPrivate) },
        ),
    ]);

    let mut err = core::ptr::null_mut();
    let key_ref = unsafe {
        SecKeyCreateWithData(
            cf_data.as_concrete_TypeRef(),
            attrs.as_concrete_TypeRef(),
            &mut err,
        )
    };
    if key_ref.is_null() {
        let cf_err = unsafe { core_foundation::error::CFError::wrap_under_create_rule(err) };
        return Err(sec_err("failed to import RSA private key", cf_err));
    }
    Ok(unsafe { SecKey::wrap_under_create_rule(key_ref) })
}

/// Fills `buf` with cryptographically secure random bytes.
pub(crate) fn fill_random(buf: &mut [u8]) -> TdsResult<()> {
    let status = unsafe { cc::CCRandomGenerateBytes(buf.as_mut_ptr().cast(), buf.len()) };
    if status == cc::kCCSuccess {
        Ok(())
    } else {
        Err(Error::ColumnEncryptionError(format!(
            "random byte generation failed (CCRandomGenerateBytes status {status})"
        )))
    }
}

/// Computes `HMAC-SHA256(key, data)`.
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> TdsResult<[u8; 32]> {
    let mut out = [0u8; 32];
    unsafe {
        cc::CCHmac(
            cc::kCCHmacAlgSHA256,
            key.as_ptr().cast(),
            key.len(),
            data.as_ptr().cast(),
            data.len(),
            out.as_mut_ptr().cast(),
        );
    }
    Ok(out)
}

/// Runs AES-256-CBC with PKCS#7 padding in the requested direction.
fn aes_256_cbc(
    op: cc::CCOperation,
    key: &[u8; 32],
    iv: &[u8; 16],
    input: &[u8],
) -> TdsResult<Vec<u8>> {
    // CBC output never exceeds the input length plus one extra block of padding.
    let mut out = vec![0u8; input.len() + super::AES_BLOCK_LEN];
    let mut moved = 0usize;
    let status = unsafe {
        cc::CCCrypt(
            op,
            cc::kCCAlgorithmAES,
            cc::kCCOptionPKCS7Padding,
            key.as_ptr().cast(),
            key.len(),
            iv.as_ptr().cast(),
            input.as_ptr().cast(),
            input.len(),
            out.as_mut_ptr().cast(),
            out.len(),
            &mut moved,
        )
    };
    if status != cc::kCCSuccess {
        let verb = if op == cc::kCCEncrypt {
            "encryption"
        } else {
            "decryption"
        };
        return Err(Error::ColumnEncryptionError(format!(
            "AES-256-CBC {verb} failed (CCCrypt status {status})"
        )));
    }
    out.truncate(moved);
    Ok(out)
}

/// Encrypts `plaintext` with AES-256-CBC and PKCS#7 padding.
pub(crate) fn aes_256_cbc_encrypt(
    key: &[u8; 32],
    iv: &[u8; 16],
    plaintext: &[u8],
) -> TdsResult<Vec<u8>> {
    aes_256_cbc(cc::kCCEncrypt, key, iv, plaintext)
}

/// Decrypts AES-256-CBC ciphertext and strips PKCS#7 padding.
pub(crate) fn aes_256_cbc_decrypt(
    key: &[u8; 32],
    iv: &[u8; 16],
    ciphertext: &[u8],
) -> TdsResult<Vec<u8>> {
    aes_256_cbc(cc::kCCDecrypt, key, iv, ciphertext)
}

/// An RSA private key (with its public part) backed by a Security.framework
/// `SecKey`.
pub(crate) struct RsaKey {
    key: SecKey,
}

impl RsaKey {
    /// Returns the public half of this key pair.
    fn public_key(&self) -> TdsResult<SecKey> {
        self.key.public_key().ok_or_else(|| {
            Error::ColumnEncryptionError("failed to derive RSA public key".to_string())
        })
    }

    /// Parses an RSA private key from PEM, accepting both PKCS#8
    /// (`-----BEGIN PRIVATE KEY-----`) and PKCS#1
    /// (`-----BEGIN RSA PRIVATE KEY-----`) encodings.
    pub(crate) fn from_pem(pem: &[u8]) -> TdsResult<Self> {
        let der_bytes = der::pem_to_der(pem)?;
        // `SecKeyCreateWithData` expects a bare PKCS#1 RSAPrivateKey for RSA keys.
        let pkcs1 = der::rsa_private_key_to_pkcs1(&der_bytes)?;
        let key = import_rsa_private_key(&pkcs1)?;
        Ok(Self { key })
    }

    /// Generates a fresh RSA key pair of the given modulus size in bits.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn generate(bits: u32) -> TdsResult<Self> {
        let mut options = GenerateKeyOptions::default();
        options.set_key_type(KeyType::rsa()).set_size_in_bits(bits);
        // With no keychain location set the key is ephemeral (not persisted).
        let key = SecKey::new(&options).map_err(|e| sec_err("RSA key generation failed", e))?;
        Ok(Self { key })
    }

    /// Serializes the private key to a PKCS#8 PEM document.
    #[cfg(test)]
    pub(crate) fn to_pkcs8_pem(&self) -> TdsResult<Vec<u8>> {
        // `external_representation` yields PKCS#1 RSAPrivateKey DER for RSA keys.
        let pkcs1 = self.key.external_representation().ok_or_else(|| {
            Error::ColumnEncryptionError("failed to export RSA private key".to_string())
        })?;
        let pkcs8 = der::pkcs1_to_pkcs8(pkcs1.bytes());
        Ok(der::der_to_pem(&pkcs8, "PRIVATE KEY"))
    }

    /// RSA-OAEP encryption using SHA-1 for both the OAEP hash and the MGF1 mask
    /// (the wrapping scheme Always Encrypted uses for column encryption keys).
    pub(crate) fn oaep_sha1_encrypt(&self, plaintext: &[u8]) -> TdsResult<Vec<u8>> {
        self.public_key()?
            .encrypt_data(Algorithm::RSAEncryptionOAEPSHA1, plaintext)
            .map_err(|e| sec_err("RSA-OAEP encryption failed", e))
    }

    /// RSA-OAEP decryption (SHA-1 OAEP hash and MGF1 mask).
    pub(crate) fn oaep_sha1_decrypt(&self, ciphertext: &[u8]) -> TdsResult<Vec<u8>> {
        self.key
            .decrypt_data(Algorithm::RSAEncryptionOAEPSHA1, ciphertext)
            .map_err(|e| sec_err("RSA-OAEP decryption failed", e))
    }

    /// Signs `data` with RSASSA-PKCS1v1.5 over SHA-256.
    pub(crate) fn pkcs1_sha256_sign(&self, data: &[u8]) -> TdsResult<Vec<u8>> {
        self.key
            .create_signature(Algorithm::RSASignatureMessagePKCS1v15SHA256, data)
            .map_err(|e| sec_err("RSA signing failed", e))
    }

    /// Verifies an RSASSA-PKCS1v1.5 / SHA-256 signature over `data`.
    pub(crate) fn pkcs1_sha256_verify(&self, data: &[u8], signature: &[u8]) -> TdsResult<bool> {
        match self.public_key()?.verify_signature(
            Algorithm::RSASignatureMessagePKCS1v15SHA256,
            data,
            signature,
        ) {
            Ok(valid) => Ok(valid),
            // Security.framework reports a simple verification mismatch (e.g. a
            // tampered message) as a `CFError` with `errSecVerifyFailed`, not as
            // a `false` return. That is an expected outcome, not an operational
            // failure, so it must map to `Ok(false)`.
            Err(e) if e.code() == ERR_SEC_VERIFY_FAILED as isize => Ok(false),
            Err(e) => Err(sec_err("RSA verification failed", e)),
        }
    }
}
