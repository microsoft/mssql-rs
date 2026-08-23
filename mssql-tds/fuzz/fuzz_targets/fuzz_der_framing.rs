// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzz target for the hand-rolled DER/PEM framing helpers used by the Apple
//! Always Encrypted crypto backend.
//!
//! These helpers parse attacker-influenceable key material (PEM/DER RSA private
//! keys), so this exercises the DER TLV reader and the PEM/Base64 framing for
//! panics, slice-index overflows, and mis-sized reads. The helpers perform no
//! cryptography — this only fuzzes the byte-framing parsers.
//!
//! Run with: cargo +nightly fuzz run fuzz_der_framing

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    mssql_tds::fuzz_support::fuzz_der_framing(data);
});
