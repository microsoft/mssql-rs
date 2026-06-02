// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SQLSTATE constants used by ODBC API implementations.

pub(crate) const SQLSTATE_HY024: [u8; 5] = *b"HY024";
pub(crate) const SQLSTATE_HY092: [u8; 5] = *b"HY092";
pub(crate) const SQLSTATE_HY000: [u8; 5] = *b"HY000";
pub(crate) const SQLSTATE_HY009: [u8; 5] = *b"HY009";
pub(crate) const SQLSTATE_HY010: [u8; 5] = *b"HY010";
pub(crate) const SQLSTATE_HY110: [u8; 5] = *b"HY110";
pub(crate) const SQLSTATE_01004: [u8; 5] = *b"01004";
pub(crate) const SQLSTATE_01S00: [u8; 5] = *b"01S00";
pub(crate) const SQLSTATE_08001: [u8; 5] = *b"08001";
pub(crate) const SQLSTATE_08003: [u8; 5] = *b"08003";
