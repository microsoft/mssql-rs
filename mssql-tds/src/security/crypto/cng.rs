// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows CNG-backed crypto primitives for Always Encrypted.
//!
//! Used on Windows so the `column-encryption` feature does not pull in
//! `openssl-sys`, which cannot build on the MSVC toolchain without a system
//! OpenSSL. All primitives go through the Cryptography API: Next Generation
//! (CNG, `bcrypt.dll`) plus the CryptoAPI ASN.1 helpers (`crypt32.dll`) for
//! PEM/DER decoding — the same `windows-sys` `Win32_Security_Cryptography`
//! bindings already used by the in-tree Schannel TLS engine, so no new
//! dependency is required. See [`super`] for the platform-selection rationale.
//!
//! The on-the-wire output matches the OpenSSL backend byte-for-byte, so values
//! stay interoperable with SQL Server regardless of which platform produced
//! them.

// Some primitives are only used by tests or by one consumer; keep the full
// surface so the cell cipher and CEK wrapping paths share one backend.
#![allow(dead_code)]

use core::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_AES_ALGORITHM, BCRYPT_ALG_HANDLE, BCRYPT_ALG_HANDLE_HMAC_FLAG, BCRYPT_BLOCK_PADDING,
    BCRYPT_CHAINING_MODE, BCRYPT_HASH_HANDLE, BCRYPT_KEY_HANDLE, BCRYPT_OAEP_PADDING_INFO,
    BCRYPT_PAD_OAEP, BCRYPT_PAD_PKCS1, BCRYPT_PKCS1_PADDING_INFO, BCRYPT_RSA_ALGORITHM,
    BCRYPT_SHA1_ALGORITHM, BCRYPT_SHA256_ALGORITHM, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDecrypt, BCryptDestroyHash,
    BCryptDestroyKey, BCryptEncrypt, BCryptExportKey, BCryptFinalizeKeyPair, BCryptFinishHash,
    BCryptGenRandom, BCryptGenerateKeyPair, BCryptGenerateSymmetricKey, BCryptGetProperty,
    BCryptHashData, BCryptImportKeyPair, BCryptOpenAlgorithmProvider, BCryptSetProperty,
    BCryptSignHash, BCryptVerifySignature, CRYPT_ALGORITHM_IDENTIFIER, CRYPT_INTEGER_BLOB,
    CRYPT_PRIVATE_KEY_INFO, CRYPT_STRING_BASE64, CRYPT_STRING_BASE64HEADER, CryptBinaryToStringA,
    CryptDecodeObjectEx, CryptEncodeObjectEx, CryptStringToBinaryA, LEGACY_RSAPRIVATE_BLOB,
    PKCS_7_ASN_ENCODING, PKCS_PRIVATE_KEY_INFO, PKCS_RSA_PRIVATE_KEY, X509_ASN_ENCODING,
    szOID_RSA_RSA,
};

use crate::core::TdsResult;
use crate::error::Error;

/// `STATUS_SUCCESS`: the NTSTATUS value returned by CNG on success.
const STATUS_SUCCESS: i32 = 0;
/// `STATUS_INVALID_SIGNATURE`: returned by `BCryptVerifySignature` when the
/// signature is well-formed but does not match (a normal "invalid", not a fault).
const STATUS_INVALID_SIGNATURE: i32 = 0xC000_A000u32 as i32;

/// Combined ASN.1 encoding type accepted by the CryptoAPI decode/encode helpers.
const ASN_ENCODING: u32 = X509_ASN_ENCODING | PKCS_7_ASN_ENCODING;

/// Maps a failed NTSTATUS into a column-encryption error with context.
fn nt_err(context: &str, status: i32) -> Error {
    Error::ColumnEncryptionError(format!("{context} failed (NTSTATUS {status:#010x})"))
}

/// Returns `Ok(())` for `STATUS_SUCCESS`, otherwise an error with context.
fn nt_check(context: &str, status: i32) -> TdsResult<()> {
    if status == STATUS_SUCCESS {
        Ok(())
    } else {
        Err(nt_err(context, status))
    }
}

/// Maps a failed CryptoAPI `BOOL` result into an error carrying the last Win32 error.
fn win32_err(context: &str) -> Error {
    let code = unsafe { GetLastError() };
    Error::ColumnEncryptionError(format!("{context} failed (Win32 error {code:#010x})"))
}

/// Closes a CNG algorithm provider handle on drop.
struct AlgGuard(BCRYPT_ALG_HANDLE);
impl Drop for AlgGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { BCryptCloseAlgorithmProvider(self.0, 0) };
        }
    }
}

/// Destroys a CNG hash handle on drop.
struct HashGuard(BCRYPT_HASH_HANDLE);
impl Drop for HashGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { BCryptDestroyHash(self.0) };
        }
    }
}

/// Destroys a CNG (symmetric) key handle on drop.
struct KeyGuard(BCRYPT_KEY_HANDLE);
impl Drop for KeyGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { BCryptDestroyKey(self.0) };
        }
    }
}

/// Opens a CNG algorithm provider for `alg_id` (e.g. `BCRYPT_AES_ALGORITHM`).
unsafe fn open_alg(alg_id: windows_sys::core::PCWSTR, flags: u32) -> TdsResult<BCRYPT_ALG_HANDLE> {
    let mut handle: BCRYPT_ALG_HANDLE = ptr::null_mut();
    nt_check("BCryptOpenAlgorithmProvider", unsafe {
        BCryptOpenAlgorithmProvider(&mut handle, alg_id, ptr::null(), flags)
    })?;
    Ok(handle)
}

/// Reads a u32-valued property (such as `ObjectLength`) from a CNG object.
unsafe fn get_u32_property(handle: BCRYPT_ALG_HANDLE, name: &str) -> TdsResult<u32> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut value = 0u32;
    let mut written = 0u32;
    nt_check("BCryptGetProperty", unsafe {
        BCryptGetProperty(
            handle,
            wide.as_ptr(),
            (&mut value as *mut u32).cast::<u8>(),
            4,
            &mut written,
            0,
        )
    })?;
    Ok(value)
}

/// Fills `buf` with cryptographically secure random bytes.
pub(crate) fn fill_random(buf: &mut [u8]) -> TdsResult<()> {
    nt_check("BCryptGenRandom", unsafe {
        BCryptGenRandom(
            ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    })
}

/// Computes a SHA-256 digest (used internally before RSA sign/verify).
unsafe fn sha256(data: &[u8]) -> TdsResult<[u8; 32]> {
    let alg = unsafe { open_alg(BCRYPT_SHA256_ALGORITHM, 0) }?;
    let _alg = AlgGuard(alg);
    let mut hash: BCRYPT_HASH_HANDLE = ptr::null_mut();
    nt_check("BCryptCreateHash", unsafe {
        BCryptCreateHash(alg, &mut hash, ptr::null_mut(), 0, ptr::null(), 0, 0)
    })?;
    let _hash = HashGuard(hash);
    nt_check("BCryptHashData", unsafe {
        BCryptHashData(hash, data.as_ptr(), data.len() as u32, 0)
    })?;
    let mut out = [0u8; 32];
    nt_check("BCryptFinishHash", unsafe {
        BCryptFinishHash(hash, out.as_mut_ptr(), 32, 0)
    })?;
    Ok(out)
}

/// Computes `HMAC-SHA256(key, data)`.
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> TdsResult<[u8; 32]> {
    let alg = unsafe { open_alg(BCRYPT_SHA256_ALGORITHM, BCRYPT_ALG_HANDLE_HMAC_FLAG) }?;
    let _alg = AlgGuard(alg);
    let mut hash: BCRYPT_HASH_HANDLE = ptr::null_mut();
    nt_check("BCryptCreateHash", unsafe {
        BCryptCreateHash(
            alg,
            &mut hash,
            ptr::null_mut(),
            0,
            key.as_ptr(),
            key.len() as u32,
            0,
        )
    })?;
    let _hash = HashGuard(hash);
    nt_check("BCryptHashData", unsafe {
        BCryptHashData(hash, data.as_ptr(), data.len() as u32, 0)
    })?;
    let mut out = [0u8; 32];
    nt_check("BCryptFinishHash", unsafe {
        BCryptFinishHash(hash, out.as_mut_ptr(), 32, 0)
    })?;
    Ok(out)
}

/// Encrypts `plaintext` with AES-256-CBC and PKCS#7 padding.
pub(crate) fn aes_256_cbc_encrypt(
    key: &[u8; 32],
    iv: &[u8; 16],
    plaintext: &[u8],
) -> TdsResult<Vec<u8>> {
    unsafe { aes_256_cbc(key, iv, plaintext, true) }
}

/// Decrypts AES-256-CBC ciphertext and strips PKCS#7 padding.
pub(crate) fn aes_256_cbc_decrypt(
    key: &[u8; 32],
    iv: &[u8; 16],
    ciphertext: &[u8],
) -> TdsResult<Vec<u8>> {
    unsafe { aes_256_cbc(key, iv, ciphertext, false) }
}

/// Shared AES-256-CBC (PKCS#7) transform. `encrypt` selects direction.
unsafe fn aes_256_cbc(
    key: &[u8; 32],
    iv: &[u8; 16],
    input: &[u8],
    encrypt: bool,
) -> TdsResult<Vec<u8>> {
    let alg = unsafe { open_alg(BCRYPT_AES_ALGORITHM, 0) }?;
    let _alg = AlgGuard(alg);

    // Select CBC chaining (CNG defaults algorithms to CBC, but set it explicitly).
    let cbc: Vec<u16> = "ChainingModeCBC"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    nt_check("BCryptSetProperty(ChainingMode)", unsafe {
        BCryptSetProperty(
            alg,
            BCRYPT_CHAINING_MODE,
            cbc.as_ptr().cast::<u8>(),
            (cbc.len() * 2) as u32,
            0,
        )
    })?;

    // Allocate the key-object buffer CNG needs for the symmetric key; it must
    // outlive the key handle, so it is declared before the key guard.
    let object_len = unsafe { get_u32_property(alg, "ObjectLength") }?;
    let mut key_object = vec![0u8; object_len as usize];
    let mut key_handle: BCRYPT_KEY_HANDLE = ptr::null_mut();
    nt_check("BCryptGenerateSymmetricKey", unsafe {
        BCryptGenerateSymmetricKey(
            alg,
            &mut key_handle,
            key_object.as_mut_ptr(),
            key_object.len() as u32,
            key.as_ptr(),
            32,
            0,
        )
    })?;
    let _key = KeyGuard(key_handle);

    // CNG overwrites the IV buffer in place, so feed it a fresh copy each call.
    let transform = |out: *mut u8, out_cap: u32, out_len: &mut u32| -> i32 {
        let mut iv_buf = *iv;
        if encrypt {
            unsafe {
                BCryptEncrypt(
                    key_handle,
                    input.as_ptr(),
                    input.len() as u32,
                    ptr::null(),
                    iv_buf.as_mut_ptr(),
                    16,
                    out,
                    out_cap,
                    out_len,
                    BCRYPT_BLOCK_PADDING,
                )
            }
        } else {
            unsafe {
                BCryptDecrypt(
                    key_handle,
                    input.as_ptr(),
                    input.len() as u32,
                    ptr::null(),
                    iv_buf.as_mut_ptr(),
                    16,
                    out,
                    out_cap,
                    out_len,
                    BCRYPT_BLOCK_PADDING,
                )
            }
        }
    };

    let label = if encrypt {
        "BCryptEncrypt"
    } else {
        "BCryptDecrypt"
    };
    let mut needed = 0u32;
    nt_check(label, transform(ptr::null_mut(), 0, &mut needed))?;
    let mut out = vec![0u8; needed as usize];
    let mut written = 0u32;
    nt_check(
        label,
        transform(out.as_mut_ptr(), out.len() as u32, &mut written),
    )?;
    out.truncate(written as usize);
    Ok(out)
}

/// An RSA private key (with its public part) held by CNG.
///
/// Wraps the owning algorithm provider and the key handle. The handles are
/// only ever used for read-only crypto operations, which CNG permits from
/// multiple threads, so the type is `Send`/`Sync`.
pub(crate) struct RsaKey {
    alg: BCRYPT_ALG_HANDLE,
    key: BCRYPT_KEY_HANDLE,
}

// SAFETY: CNG key and algorithm-provider handles are safe to use concurrently
// for the read-only operations performed here; no interior state is mutated.
unsafe impl Send for RsaKey {}
unsafe impl Sync for RsaKey {}

impl Drop for RsaKey {
    fn drop(&mut self) {
        unsafe {
            if !self.key.is_null() {
                BCryptDestroyKey(self.key);
            }
            if !self.alg.is_null() {
                BCryptCloseAlgorithmProvider(self.alg, 0);
            }
        }
    }
}

impl RsaKey {
    /// Parses an RSA private key from PEM, accepting both PKCS#8
    /// (`-----BEGIN PRIVATE KEY-----`) and PKCS#1
    /// (`-----BEGIN RSA PRIVATE KEY-----`) encodings.
    pub(crate) fn from_pem(pem: &[u8]) -> TdsResult<Self> {
        unsafe {
            let der = pem_to_der(pem)?;

            // PKCS#8 wraps the PKCS#1 RSAPrivateKey in a PrivateKeyInfo; unwrap
            // it if present, otherwise treat the DER as PKCS#1 directly.
            let pkcs1_der = match decode_object(PKCS_PRIVATE_KEY_INFO, &der) {
                Ok(info_buf) => {
                    let info = &*info_buf.as_ptr().cast::<CRYPT_PRIVATE_KEY_INFO>();
                    std::slice::from_raw_parts(
                        info.PrivateKey.pbData,
                        info.PrivateKey.cbData as usize,
                    )
                    .to_vec()
                }
                Err(_) => der,
            };

            // PKCS#1 RSAPrivateKey DER -> legacy CAPI private blob -> CNG key.
            let capi_blob = decode_object(PKCS_RSA_PRIVATE_KEY, &pkcs1_der)?;
            Self::import_capi(&capi_blob)
        }
    }

    /// Generates a fresh RSA key pair of the given modulus size in bits.
    pub(crate) fn generate(bits: u32) -> TdsResult<Self> {
        unsafe {
            let alg = open_alg(BCRYPT_RSA_ALGORITHM, 0)?;
            let mut key: BCRYPT_KEY_HANDLE = ptr::null_mut();
            let status = BCryptGenerateKeyPair(alg, &mut key, bits, 0);
            if status != STATUS_SUCCESS {
                BCryptCloseAlgorithmProvider(alg, 0);
                return Err(nt_err("BCryptGenerateKeyPair", status));
            }
            let status = BCryptFinalizeKeyPair(key, 0);
            if status != STATUS_SUCCESS {
                BCryptDestroyKey(key);
                BCryptCloseAlgorithmProvider(alg, 0);
                return Err(nt_err("BCryptFinalizeKeyPair", status));
            }
            Ok(Self { alg, key })
        }
    }

    /// Imports a legacy CAPI RSA private key blob into a CNG key handle.
    unsafe fn import_capi(capi_blob: &[u8]) -> TdsResult<Self> {
        let alg = unsafe { open_alg(BCRYPT_RSA_ALGORITHM, 0) }?;
        let mut key: BCRYPT_KEY_HANDLE = ptr::null_mut();
        let status = unsafe {
            BCryptImportKeyPair(
                alg,
                ptr::null_mut(),
                LEGACY_RSAPRIVATE_BLOB,
                &mut key,
                capi_blob.as_ptr(),
                capi_blob.len() as u32,
                0,
            )
        };
        if status != STATUS_SUCCESS {
            unsafe { BCryptCloseAlgorithmProvider(alg, 0) };
            return Err(nt_err("BCryptImportKeyPair", status));
        }
        Ok(Self { alg, key })
    }

    /// Serializes the private key to a PKCS#8 PEM document.
    pub(crate) fn to_pkcs8_pem(&self) -> TdsResult<Vec<u8>> {
        unsafe {
            // Export the legacy CAPI blob, re-encode it as PKCS#1 DER, then wrap
            // that in a PKCS#8 PrivateKeyInfo and PEM-armor it.
            let capi_blob = self.export_capi()?;
            let pkcs1_der = encode_object(PKCS_RSA_PRIVATE_KEY, capi_blob.as_ptr().cast())?;

            // ASN.1 NULL parameters for the rsaEncryption algorithm identifier.
            let mut null_params = [0x05u8, 0x00u8];
            let algorithm = CRYPT_ALGORITHM_IDENTIFIER {
                pszObjId: szOID_RSA_RSA as *mut u8,
                Parameters: CRYPT_INTEGER_BLOB {
                    cbData: null_params.len() as u32,
                    pbData: null_params.as_mut_ptr(),
                },
            };
            let mut pkcs1_owned = pkcs1_der;
            let info = CRYPT_PRIVATE_KEY_INFO {
                Version: 0,
                Algorithm: algorithm,
                PrivateKey: CRYPT_INTEGER_BLOB {
                    cbData: pkcs1_owned.len() as u32,
                    pbData: pkcs1_owned.as_mut_ptr(),
                },
                pAttributes: ptr::null_mut(),
            };
            let pkcs8_der = encode_object(
                PKCS_PRIVATE_KEY_INFO,
                (&info as *const CRYPT_PRIVATE_KEY_INFO).cast(),
            )?;
            der_to_pem(&pkcs8_der, "PRIVATE KEY")
        }
    }

    /// Exports the key as a legacy CAPI RSA private blob.
    unsafe fn export_capi(&self) -> TdsResult<Vec<u8>> {
        let mut needed = 0u32;
        nt_check("BCryptExportKey", unsafe {
            BCryptExportKey(
                self.key,
                ptr::null_mut(),
                LEGACY_RSAPRIVATE_BLOB,
                ptr::null_mut(),
                0,
                &mut needed,
                0,
            )
        })?;
        let mut buf = vec![0u8; needed as usize];
        let mut written = 0u32;
        nt_check("BCryptExportKey", unsafe {
            BCryptExportKey(
                self.key,
                ptr::null_mut(),
                LEGACY_RSAPRIVATE_BLOB,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut written,
                0,
            )
        })?;
        buf.truncate(written as usize);
        Ok(buf)
    }

    /// RSA-OAEP encryption using SHA-1 for both the OAEP hash and the MGF1 mask.
    pub(crate) fn oaep_sha1_encrypt(&self, plaintext: &[u8]) -> TdsResult<Vec<u8>> {
        unsafe {
            let padding = BCRYPT_OAEP_PADDING_INFO {
                pszAlgId: BCRYPT_SHA1_ALGORITHM,
                pbLabel: ptr::null_mut(),
                cbLabel: 0,
            };
            let padding_ptr = (&padding as *const BCRYPT_OAEP_PADDING_INFO).cast::<c_void>();
            let mut needed = 0u32;
            nt_check(
                "BCryptEncrypt(RSA-OAEP)",
                BCryptEncrypt(
                    self.key,
                    plaintext.as_ptr(),
                    plaintext.len() as u32,
                    padding_ptr,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    0,
                    &mut needed,
                    BCRYPT_PAD_OAEP,
                ),
            )?;
            let mut out = vec![0u8; needed as usize];
            let mut written = 0u32;
            nt_check(
                "BCryptEncrypt(RSA-OAEP)",
                BCryptEncrypt(
                    self.key,
                    plaintext.as_ptr(),
                    plaintext.len() as u32,
                    padding_ptr,
                    ptr::null_mut(),
                    0,
                    out.as_mut_ptr(),
                    out.len() as u32,
                    &mut written,
                    BCRYPT_PAD_OAEP,
                ),
            )?;
            out.truncate(written as usize);
            Ok(out)
        }
    }

    /// RSA-OAEP decryption (SHA-1 OAEP hash and MGF1 mask).
    pub(crate) fn oaep_sha1_decrypt(&self, ciphertext: &[u8]) -> TdsResult<Vec<u8>> {
        unsafe {
            let padding = BCRYPT_OAEP_PADDING_INFO {
                pszAlgId: BCRYPT_SHA1_ALGORITHM,
                pbLabel: ptr::null_mut(),
                cbLabel: 0,
            };
            let padding_ptr = (&padding as *const BCRYPT_OAEP_PADDING_INFO).cast::<c_void>();
            let mut needed = 0u32;
            nt_check(
                "BCryptDecrypt(RSA-OAEP)",
                BCryptDecrypt(
                    self.key,
                    ciphertext.as_ptr(),
                    ciphertext.len() as u32,
                    padding_ptr,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    0,
                    &mut needed,
                    BCRYPT_PAD_OAEP,
                ),
            )?;
            let mut out = vec![0u8; needed as usize];
            let mut written = 0u32;
            nt_check(
                "BCryptDecrypt(RSA-OAEP)",
                BCryptDecrypt(
                    self.key,
                    ciphertext.as_ptr(),
                    ciphertext.len() as u32,
                    padding_ptr,
                    ptr::null_mut(),
                    0,
                    out.as_mut_ptr(),
                    out.len() as u32,
                    &mut written,
                    BCRYPT_PAD_OAEP,
                ),
            )?;
            out.truncate(written as usize);
            Ok(out)
        }
    }

    /// Signs `data` with RSASSA-PKCS1v1.5 over SHA-256.
    pub(crate) fn pkcs1_sha256_sign(&self, data: &[u8]) -> TdsResult<Vec<u8>> {
        unsafe {
            let digest = sha256(data)?;
            let padding = BCRYPT_PKCS1_PADDING_INFO {
                pszAlgId: BCRYPT_SHA256_ALGORITHM,
            };
            let padding_ptr = (&padding as *const BCRYPT_PKCS1_PADDING_INFO).cast::<c_void>();
            let mut needed = 0u32;
            nt_check(
                "BCryptSignHash",
                BCryptSignHash(
                    self.key,
                    padding_ptr,
                    digest.as_ptr(),
                    digest.len() as u32,
                    ptr::null_mut(),
                    0,
                    &mut needed,
                    BCRYPT_PAD_PKCS1,
                ),
            )?;
            let mut sig = vec![0u8; needed as usize];
            let mut written = 0u32;
            nt_check(
                "BCryptSignHash",
                BCryptSignHash(
                    self.key,
                    padding_ptr,
                    digest.as_ptr(),
                    digest.len() as u32,
                    sig.as_mut_ptr(),
                    sig.len() as u32,
                    &mut written,
                    BCRYPT_PAD_PKCS1,
                ),
            )?;
            sig.truncate(written as usize);
            Ok(sig)
        }
    }

    /// Verifies an RSASSA-PKCS1v1.5 / SHA-256 signature over `data`.
    pub(crate) fn pkcs1_sha256_verify(&self, data: &[u8], signature: &[u8]) -> TdsResult<bool> {
        unsafe {
            let digest = sha256(data)?;
            let padding = BCRYPT_PKCS1_PADDING_INFO {
                pszAlgId: BCRYPT_SHA256_ALGORITHM,
            };
            let padding_ptr = (&padding as *const BCRYPT_PKCS1_PADDING_INFO).cast::<c_void>();
            let status = BCryptVerifySignature(
                self.key,
                padding_ptr,
                digest.as_ptr(),
                digest.len() as u32,
                signature.as_ptr(),
                signature.len() as u32,
                BCRYPT_PAD_PKCS1,
            );
            match status {
                STATUS_SUCCESS => Ok(true),
                STATUS_INVALID_SIGNATURE => Ok(false),
                other => Err(nt_err("BCryptVerifySignature", other)),
            }
        }
    }
}

/// Decodes a PEM document (any label) into its DER bytes.
unsafe fn pem_to_der(pem: &[u8]) -> TdsResult<Vec<u8>> {
    let mut needed = 0u32;
    if unsafe {
        CryptStringToBinaryA(
            pem.as_ptr(),
            pem.len() as u32,
            CRYPT_STRING_BASE64HEADER,
            ptr::null_mut(),
            &mut needed,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(win32_err("CryptStringToBinaryA"));
    }
    let mut der = vec![0u8; needed as usize];
    if unsafe {
        CryptStringToBinaryA(
            pem.as_ptr(),
            pem.len() as u32,
            CRYPT_STRING_BASE64HEADER,
            der.as_mut_ptr(),
            &mut needed,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(win32_err("CryptStringToBinaryA"));
    }
    der.truncate(needed as usize);
    Ok(der)
}

/// Base64-armors DER bytes into a PEM document with the given label.
unsafe fn der_to_pem(der: &[u8], label: &str) -> TdsResult<Vec<u8>> {
    let mut needed = 0u32;
    if unsafe {
        CryptBinaryToStringA(
            der.as_ptr(),
            der.len() as u32,
            CRYPT_STRING_BASE64,
            ptr::null_mut(),
            &mut needed,
        )
    } == 0
    {
        return Err(win32_err("CryptBinaryToStringA"));
    }
    let mut buf = vec![0u8; needed as usize];
    if unsafe {
        CryptBinaryToStringA(
            der.as_ptr(),
            der.len() as u32,
            CRYPT_STRING_BASE64,
            buf.as_mut_ptr(),
            &mut needed,
        )
    } == 0
    {
        return Err(win32_err("CryptBinaryToStringA"));
    }
    // `needed` now excludes the trailing NUL; the body already has CRLF breaks.
    let body = String::from_utf8_lossy(&buf[..needed as usize]);
    Ok(format!("-----BEGIN {label}-----\r\n{body}-----END {label}-----\r\n").into_bytes())
}

/// CryptoAPI `CryptDecodeObjectEx` wrapped with the two-call size pattern. The
/// returned buffer is self-contained (referenced data is stored inline).
unsafe fn decode_object(struct_type: windows_sys::core::PCSTR, der: &[u8]) -> TdsResult<Vec<u8>> {
    let mut needed = 0u32;
    if unsafe {
        CryptDecodeObjectEx(
            ASN_ENCODING,
            struct_type,
            der.as_ptr(),
            der.len() as u32,
            0,
            ptr::null(),
            ptr::null_mut(),
            &mut needed,
        )
    } == 0
    {
        return Err(win32_err("CryptDecodeObjectEx"));
    }
    let mut buf = vec![0u8; needed as usize];
    if unsafe {
        CryptDecodeObjectEx(
            ASN_ENCODING,
            struct_type,
            der.as_ptr(),
            der.len() as u32,
            0,
            ptr::null(),
            buf.as_mut_ptr().cast::<c_void>(),
            &mut needed,
        )
    } == 0
    {
        return Err(win32_err("CryptDecodeObjectEx"));
    }
    buf.truncate(needed as usize);
    Ok(buf)
}

/// CryptoAPI `CryptEncodeObjectEx` wrapped with the two-call size pattern.
unsafe fn encode_object(
    struct_type: windows_sys::core::PCSTR,
    structure: *const c_void,
) -> TdsResult<Vec<u8>> {
    let mut needed = 0u32;
    if unsafe {
        CryptEncodeObjectEx(
            ASN_ENCODING,
            struct_type,
            structure,
            0,
            ptr::null(),
            ptr::null_mut(),
            &mut needed,
        )
    } == 0
    {
        return Err(win32_err("CryptEncodeObjectEx"));
    }
    let mut buf = vec![0u8; needed as usize];
    if unsafe {
        CryptEncodeObjectEx(
            ASN_ENCODING,
            struct_type,
            structure,
            0,
            ptr::null(),
            buf.as_mut_ptr().cast::<c_void>(),
            &mut needed,
        )
    } == 0
    {
        return Err(win32_err("CryptEncodeObjectEx"));
    }
    buf.truncate(needed as usize);
    Ok(buf)
}
