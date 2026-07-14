// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Minimal PEM / DER framing helpers for the Apple crypto backend.
//!
//! Apple's `SecKey` imports and exports RSA private keys as raw PKCS#1
//! (`RSAPrivateKey`) DER, while the rest of the stack (and the other backends)
//! speak PEM and accept PKCS#8. These helpers bridge the two: strip/apply PEM
//! armor, Base64 encode/decode, and convert between PKCS#1 and PKCS#8 DER.
//!
//! This is byte-level serialization only — there is no cryptography here — so it
//! lives in safe, portable Rust and is unit-tested on every platform (the Apple
//! `SecKey` calls that consume the output can only be exercised on macOS).

// Under `fuzzing` only the framing parsers are exercised; the encode-side helpers
// (only used by the Apple backend / tests) are then unused.
#![cfg_attr(fuzzing, allow(dead_code))]
// These helpers are only wired into the macOS backend; on other targets they are
// still compiled under `cfg(test)` so their round-trips stay covered.
#![allow(dead_code)]

use crate::core::TdsResult;
use crate::error::Error;

/// DER tag for `SEQUENCE` (constructed).
const TAG_SEQUENCE: u8 = 0x30;
/// DER tag for `INTEGER`.
const TAG_INTEGER: u8 = 0x02;
/// DER tag for `OCTET STRING`.
const TAG_OCTET_STRING: u8 = 0x04;

/// `AlgorithmIdentifier` for `rsaEncryption` (OID 1.2.840.113549.1.1.1) with the
/// required explicit `NULL` parameters, pre-encoded as DER.
const RSA_ENCRYPTION_ALG_ID: [u8; 15] = [
    0x30, 0x0d, // SEQUENCE, 13 bytes
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, // OID rsaEncryption
    0x05, 0x00, // NULL
];

fn der_err(msg: &str) -> Error {
    Error::ColumnEncryptionError(format!("DER parsing failed: {msg}"))
}

/// Encodes a DER definite-length prefix for a value of `len` bytes.
fn encode_len(len: usize) -> Vec<u8> {
    if len < 0x80 {
        return vec![len as u8];
    }
    let mut bytes = Vec::new();
    let mut value = len;
    while value > 0 {
        bytes.insert(0, (value & 0xff) as u8);
        value >>= 8;
    }
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(0x80 | bytes.len() as u8);
    out.extend_from_slice(&bytes);
    out
}

/// Encodes a single DER TLV (tag, length, value).
fn encode_tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 4);
    out.push(tag);
    out.extend_from_slice(&encode_len(value.len()));
    out.extend_from_slice(value);
    out
}

/// A parsed DER TLV: its tag, its content slice, and the offset just past it.
struct Tlv<'a> {
    tag: u8,
    content: &'a [u8],
    next: usize,
}

/// Reads one DER TLV starting at `pos` within `buf`.
fn read_tlv(buf: &[u8], pos: usize) -> TdsResult<Tlv<'_>> {
    if pos >= buf.len() {
        return Err(der_err("unexpected end of input reading tag"));
    }
    let tag = buf[pos];
    let mut idx = pos + 1;
    if idx >= buf.len() {
        return Err(der_err("unexpected end of input reading length"));
    }
    let first = buf[idx];
    idx += 1;
    let len = if first < 0x80 {
        first as usize
    } else {
        let num_bytes = (first & 0x7f) as usize;
        if num_bytes == 0 || num_bytes > core::mem::size_of::<usize>() {
            return Err(der_err("unsupported DER length encoding"));
        }
        let mut value = 0usize;
        for _ in 0..num_bytes {
            if idx >= buf.len() {
                return Err(der_err("unexpected end of input in long-form length"));
            }
            value = (value << 8) | buf[idx] as usize;
            idx += 1;
        }
        value
    };
    let end = idx
        .checked_add(len)
        .ok_or_else(|| der_err("DER length overflow"))?;
    if end > buf.len() {
        return Err(der_err("DER value extends past end of input"));
    }
    Ok(Tlv {
        tag,
        content: &buf[idx..end],
        next: end,
    })
}

/// Returns the raw PKCS#1 `RSAPrivateKey` DER for an RSA private key supplied as
/// either PKCS#1 or PKCS#8 DER.
///
/// PKCS#1 `RSAPrivateKey` is `SEQUENCE { INTEGER version, INTEGER n, ... }` while
/// PKCS#8 `PrivateKeyInfo` is `SEQUENCE { INTEGER version, SEQUENCE algorithm,
/// OCTET STRING privateKey }`. They are told apart by the tag of the element
/// following the version `INTEGER`.
pub(crate) fn rsa_private_key_to_pkcs1(der: &[u8]) -> TdsResult<Vec<u8>> {
    let outer = read_tlv(der, 0)?;
    if outer.tag != TAG_SEQUENCE {
        return Err(der_err("expected outer SEQUENCE"));
    }
    let body = outer.content;
    let version = read_tlv(body, 0)?;
    if version.tag != TAG_INTEGER {
        return Err(der_err("expected version INTEGER"));
    }
    let second = read_tlv(body, version.next)?;
    match second.tag {
        // PKCS#1: the version is followed by the modulus INTEGER, so the input is
        // already an RSAPrivateKey.
        TAG_INTEGER => Ok(der.to_vec()),
        // PKCS#8: the version is followed by the algorithm SEQUENCE; the third
        // element is the OCTET STRING wrapping the PKCS#1 key.
        TAG_SEQUENCE => {
            let private_key = read_tlv(body, second.next)?;
            if private_key.tag != TAG_OCTET_STRING {
                return Err(der_err("expected privateKey OCTET STRING"));
            }
            Ok(private_key.content.to_vec())
        }
        _ => Err(der_err("unrecognized RSA private key structure")),
    }
}

/// Wraps a PKCS#1 `RSAPrivateKey` DER in a PKCS#8 `PrivateKeyInfo` SEQUENCE.
pub(crate) fn pkcs1_to_pkcs8(pkcs1: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    // version INTEGER 0
    body.extend_from_slice(&[TAG_INTEGER, 0x01, 0x00]);
    // privateKeyAlgorithm
    body.extend_from_slice(&RSA_ENCRYPTION_ALG_ID);
    // privateKey OCTET STRING { RSAPrivateKey }
    body.extend_from_slice(&encode_tlv(TAG_OCTET_STRING, pkcs1));
    encode_tlv(TAG_SEQUENCE, &body)
}

/// Decodes a PEM document into its DER payload, ignoring the armor label.
pub(crate) fn pem_to_der(pem: &[u8]) -> TdsResult<Vec<u8>> {
    let text = core::str::from_utf8(pem)
        .map_err(|_| Error::ColumnEncryptionError("PEM is not valid UTF-8".to_string()))?;
    let mut base64_body = String::new();
    let mut in_body = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-----BEGIN") {
            in_body = true;
            continue;
        }
        if trimmed.starts_with("-----END") {
            in_body = false;
            continue;
        }
        if in_body {
            base64_body.push_str(trimmed);
        }
    }
    if base64_body.is_empty() {
        return Err(Error::ColumnEncryptionError(
            "PEM contains no Base64 body".to_string(),
        ));
    }
    base64_decode(&base64_body)
}

/// Encodes DER bytes as a PEM document with the given armor `label`.
pub(crate) fn der_to_pem(der: &[u8], label: &str) -> Vec<u8> {
    let encoded = base64_encode(der);
    let mut out = String::new();
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        // `base64_encode` only emits ASCII, so each chunk is valid UTF-8.
        out.push_str(core::str::from_utf8(chunk).expect("base64 output is ASCII"));
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out.into_bytes()
}

/// Standard Base64 alphabet (RFC 4648).
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `input` as standard Base64 with padding.
fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(BASE64_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(BASE64_ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Maps a Base64 character to its 6-bit value, or `None` for padding/invalid.
fn base64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decodes standard Base64, ignoring `=` padding. Whitespace must already be
/// stripped by the caller.
fn base64_decode(input: &str) -> TdsResult<Vec<u8>> {
    let mut bits = 0u32;
    let mut bit_count = 0u32;
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for &c in input.as_bytes() {
        if c == b'=' {
            break;
        }
        let value = base64_value(c)
            .ok_or_else(|| Error::ColumnEncryptionError("invalid Base64 character".to_string()))?;
        bits = (bits << 6) | value as u32;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push((bits >> bit_count) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_all_lengths() {
        for len in 0..32usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 1) as u8).collect();
            let encoded = base64_encode(&data);
            let decoded = base64_decode(&encoded).expect("decode");
            assert_eq!(decoded, data, "round trip failed for len {len}");
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn encode_len_short_and_long_form() {
        assert_eq!(encode_len(0), vec![0x00]);
        assert_eq!(encode_len(127), vec![0x7f]);
        assert_eq!(encode_len(128), vec![0x81, 0x80]);
        assert_eq!(encode_len(256), vec![0x82, 0x01, 0x00]);
    }

    #[test]
    fn pkcs1_pkcs8_round_trip() {
        // A stand-in "RSAPrivateKey": a SEQUENCE of two small INTEGERs. The
        // conversion only manipulates framing, so the exact contents are
        // irrelevant as long as they form a valid TLV.
        let fake_pkcs1 = encode_tlv(
            TAG_SEQUENCE,
            &[
                TAG_INTEGER,
                0x01,
                0x00, // version 0
                TAG_INTEGER,
                0x01,
                0x2a, // "modulus" 42
            ],
        );
        let pkcs8 = pkcs1_to_pkcs8(&fake_pkcs1);
        let recovered = rsa_private_key_to_pkcs1(&pkcs8).expect("unwrap pkcs8");
        assert_eq!(recovered, fake_pkcs1);

        // A genuine PKCS#1 input is returned unchanged.
        let passthrough = rsa_private_key_to_pkcs1(&fake_pkcs1).expect("passthrough pkcs1");
        assert_eq!(passthrough, fake_pkcs1);
    }

    #[test]
    fn pem_der_round_trip() {
        let der: Vec<u8> = (0..200u32).map(|i| (i % 256) as u8).collect();
        let pem = der_to_pem(&der, "PRIVATE KEY");
        let pem_text = core::str::from_utf8(&pem).unwrap();
        assert!(pem_text.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        assert!(pem_text.trim_end().ends_with("-----END PRIVATE KEY-----"));
        // No Base64 line exceeds 64 characters.
        for line in pem_text.lines().filter(|l| !l.starts_with("-----")) {
            assert!(line.len() <= 64);
        }
        let decoded = pem_to_der(&pem).expect("pem_to_der");
        assert_eq!(decoded, der);
    }

    #[test]
    fn pem_to_der_rejects_empty_body() {
        let pem = b"-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n";
        assert!(pem_to_der(pem).is_err());
    }

    #[test]
    fn read_tlv_rejects_truncated_value() {
        // SEQUENCE claiming 5 bytes but only 2 present.
        let bad = [TAG_SEQUENCE, 0x05, 0x01, 0x02];
        assert!(read_tlv(&bad, 0).is_err());
    }
}
