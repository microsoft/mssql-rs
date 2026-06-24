// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parsing of the two result sets returned by `sp_describe_parameter_encryption`.
//!
//! When Always Encrypted is enabled and a parameterized statement is executed,
//! the driver first asks the server, via `sp_describe_parameter_encryption`,
//! which parameters require encryption and how. The procedure returns two
//! result sets:
//!
//! * **Result set 1** — one row per *encrypted CEK value*. Rows that share a
//!   `KeyOrdinal` describe the same column encryption key (CEK) wrapped by
//!   different column master keys (CMKs) for rotation. These rows are
//!   accumulated into [`CekTableEntry`] values keyed by ordinal.
//! * **Result set 2** — one row per parameter, describing whether (and how) the
//!   parameter must be encrypted and which CEK ordinal to use.
//!
//! The column order of both result sets is fixed by the server contract and
//! mirrors the JDBC driver's `DescribeParameterEncryptionResultSet1/2`
//! enumerations (`AE.java`). If the server ever changes the order, transparent
//! parameter encryption breaks for every driver, so the indices below are
//! treated as a stable wire contract.

#![allow(dead_code)] // Fully consumed by the parameter-encryption execute path (Phase E pt3).

use std::collections::HashMap;

use crate::core::TdsResult;
use crate::datatypes::column_values::ColumnValues;
use crate::error::Error;
use crate::query::metadata::{CekTableEntry, EncryptedCekValue};

// Result set 1 column indices (0-based). Mirrors JDBC
// `DescribeParameterEncryptionResultSet1` (which is 1-based).
const RS1_KEY_ORDINAL: usize = 0;
const RS1_DB_ID: usize = 1;
const RS1_KEY_ID: usize = 2;
const RS1_KEY_VERSION: usize = 3;
const RS1_KEY_MD_VERSION: usize = 4;
const RS1_ENCRYPTED_KEY: usize = 5;
const RS1_PROVIDER_NAME: usize = 6;
const RS1_KEY_PATH: usize = 7;
const RS1_KEY_ENCRYPTION_ALGORITHM: usize = 8;

// Result set 2 column indices (0-based). Mirrors JDBC
// `DescribeParameterEncryptionResultSet2`.
const RS2_PARAMETER_ORDINAL: usize = 0;
const RS2_PARAMETER_NAME: usize = 1;
const RS2_COLUMN_ENCRYPTION_ALGORITHM: usize = 2;
const RS2_COLUMN_ENCRYPTION_TYPE: usize = 3;
const RS2_COLUMN_ENCRYPTION_KEY_ORDINAL: usize = 4;
const RS2_NORMALIZATION_RULE_VERSION: usize = 5;

/// Encryption type byte for a parameter that the server does not require to be
/// encrypted.
pub(crate) const ENCRYPTION_TYPE_PLAINTEXT: u8 = 0;

/// Per-parameter encryption description parsed from result set 2.
#[derive(Debug, Clone)]
pub(crate) struct ParameterEncryptionInfo {
    /// 1-based ordinal of the parameter within the statement.
    #[allow(dead_code)]
    pub parameter_ordinal: i32,
    /// Parameter name including the leading `@` (e.g. `@p1`).
    pub parameter_name: String,
    /// Cipher algorithm id (`1` = AEAD_AES_256_CBC_HMAC_SHA256).
    pub cipher_algorithm_id: u8,
    /// Encryption type (`0` = plaintext, `1` = deterministic, `2` = randomized).
    pub encryption_type: u8,
    /// Ordinal into the result-set-1 CEK table identifying the wrapping key.
    pub cek_ordinal: i32,
    /// Normalization rule version byte.
    pub normalization_rule_version: u8,
}

impl ParameterEncryptionInfo {
    /// Returns `true` if the server requires this parameter to be encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.encryption_type != ENCRYPTION_TYPE_PLAINTEXT
    }
}

/// Combined result of an `sp_describe_parameter_encryption` call.
#[derive(Debug, Default)]
pub(crate) struct DescribeParameterEncryptionResult {
    /// CEK entries keyed by their result-set-1 key ordinal.
    pub cek_entries: HashMap<i32, CekTableEntry>,
    /// Per-parameter encryption info, in result-set-2 order.
    pub parameters: Vec<ParameterEncryptionInfo>,
}

impl DescribeParameterEncryptionResult {
    /// Creates an empty result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks up the CEK entry for a parameter's ordinal.
    pub fn cek_entry_for(&self, cek_ordinal: i32) -> Option<&CekTableEntry> {
        self.cek_entries.get(&cek_ordinal)
    }
}

/// Reads a signed 32-bit integer from a result-set cell, accepting any of the
/// integer column types the server may use.
fn read_i32(row: &[ColumnValues], index: usize) -> TdsResult<i32> {
    match row.get(index) {
        Some(ColumnValues::Int(v)) => Ok(*v),
        Some(ColumnValues::TinyInt(v)) => Ok(*v as i32),
        Some(ColumnValues::SmallInt(v)) => Ok(*v as i32),
        Some(ColumnValues::BigInt(v)) => Ok(*v as i32),
        other => Err(Error::ColumnEncryptionError(format!(
            "sp_describe_parameter_encryption: expected integer at column {index}, got {other:?}"
        ))),
    }
}

/// Reads a `u8` from an integer result-set cell.
fn read_u8(row: &[ColumnValues], index: usize) -> TdsResult<u8> {
    Ok(read_i32(row, index)? as u8)
}

/// Reads a binary cell as a byte vector.
fn read_bytes(row: &[ColumnValues], index: usize) -> TdsResult<Vec<u8>> {
    match row.get(index) {
        Some(ColumnValues::Bytes(v)) => Ok(v.clone()),
        other => Err(Error::ColumnEncryptionError(format!(
            "sp_describe_parameter_encryption: expected binary at column {index}, got {other:?}"
        ))),
    }
}

/// Reads a string cell.
fn read_string(row: &[ColumnValues], index: usize) -> TdsResult<String> {
    match row.get(index) {
        Some(ColumnValues::String(s)) => Ok(s.to_utf8_string()),
        other => Err(Error::ColumnEncryptionError(format!(
            "sp_describe_parameter_encryption: expected string at column {index}, got {other:?}"
        ))),
    }
}

/// Reads the 8-byte CEK metadata version.
fn read_md_version(row: &[ColumnValues], index: usize) -> TdsResult<[u8; 8]> {
    let bytes = read_bytes(row, index)?;
    bytes.as_slice().try_into().map_err(|_| {
        Error::ColumnEncryptionError(format!(
            "sp_describe_parameter_encryption: CEK metadata version must be 8 bytes, got {}",
            bytes.len()
        ))
    })
}

/// Accumulates one result-set-1 row into the CEK table, creating a new entry the
/// first time a key ordinal is seen and appending its encrypted CEK value
/// otherwise.
pub(crate) fn accumulate_cek_entry(
    cek_entries: &mut HashMap<i32, CekTableEntry>,
    row: &[ColumnValues],
) -> TdsResult<()> {
    let ordinal = read_i32(row, RS1_KEY_ORDINAL)?;
    let encrypted_value = EncryptedCekValue {
        encrypted_key: read_bytes(row, RS1_ENCRYPTED_KEY)?,
        key_store_name: read_string(row, RS1_PROVIDER_NAME)?,
        key_path: read_string(row, RS1_KEY_PATH)?,
        algorithm_name: read_string(row, RS1_KEY_ENCRYPTION_ALGORITHM)?,
    };

    let entry = cek_entries.entry(ordinal).or_insert_with(|| CekTableEntry {
        database_id: 0,
        cek_id: 0,
        cek_version: 0,
        cek_md_version: [0u8; 8],
        encrypted_cek_values: Vec::new(),
    });

    // The identity fields are identical across rows that share an ordinal; set
    // them from the first row that creates the entry.
    if entry.encrypted_cek_values.is_empty() {
        entry.database_id = read_i32(row, RS1_DB_ID)?;
        entry.cek_id = read_i32(row, RS1_KEY_ID)?;
        entry.cek_version = read_i32(row, RS1_KEY_VERSION)?;
        entry.cek_md_version = read_md_version(row, RS1_KEY_MD_VERSION)?;
    }
    entry.encrypted_cek_values.push(encrypted_value);
    Ok(())
}

/// Parses one result-set-2 row into a [`ParameterEncryptionInfo`].
pub(crate) fn parse_parameter_info(row: &[ColumnValues]) -> TdsResult<ParameterEncryptionInfo> {
    Ok(ParameterEncryptionInfo {
        parameter_ordinal: read_i32(row, RS2_PARAMETER_ORDINAL)?,
        parameter_name: read_string(row, RS2_PARAMETER_NAME)?,
        cipher_algorithm_id: read_u8(row, RS2_COLUMN_ENCRYPTION_ALGORITHM)?,
        encryption_type: read_u8(row, RS2_COLUMN_ENCRYPTION_TYPE)?,
        cek_ordinal: read_i32(row, RS2_COLUMN_ENCRYPTION_KEY_ORDINAL)?,
        normalization_rule_version: read_u8(row, RS2_NORMALIZATION_RULE_VERSION)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::sql_string::SqlString;

    fn sql(s: &str) -> ColumnValues {
        ColumnValues::String(SqlString::from_utf8_string(s.to_string()))
    }

    fn rs1_row(ordinal: i32, provider: &str, encrypted: Vec<u8>) -> Vec<ColumnValues> {
        vec![
            ColumnValues::Int(ordinal),                        // KeyOrdinal
            ColumnValues::Int(7),                              // DbId
            ColumnValues::Int(11),                             // KeyId
            ColumnValues::Int(3),                              // KeyVersion
            ColumnValues::Bytes(vec![1, 2, 3, 4, 5, 6, 7, 8]), // KeyMdVersion
            ColumnValues::Bytes(encrypted),                    // EncryptedKey
            sql(provider),                                     // ProviderName
            sql("https://vault/key"),                          // KeyPath
            sql("RSA_OAEP"),                                   // KeyEncryptionAlgorithm
        ]
    }

    fn rs2_row(
        ordinal: i32,
        name: &str,
        algo: u8,
        enc_type: u8,
        cek_ordinal: i32,
        norm: u8,
    ) -> Vec<ColumnValues> {
        vec![
            ColumnValues::Int(ordinal),
            sql(name),
            ColumnValues::TinyInt(algo),
            ColumnValues::TinyInt(enc_type),
            ColumnValues::Int(cek_ordinal),
            ColumnValues::TinyInt(norm),
        ]
    }

    #[test]
    fn accumulates_single_cek_entry() {
        let mut entries = HashMap::new();
        accumulate_cek_entry(&mut entries, &rs1_row(0, "AZURE_KEY_VAULT", vec![9, 9, 9])).unwrap();

        assert_eq!(entries.len(), 1);
        let entry = &entries[&0];
        assert_eq!(entry.database_id, 7);
        assert_eq!(entry.cek_id, 11);
        assert_eq!(entry.cek_version, 3);
        assert_eq!(entry.cek_md_version, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(entry.encrypted_cek_values.len(), 1);
        assert_eq!(
            entry.encrypted_cek_values[0].key_store_name,
            "AZURE_KEY_VAULT"
        );
        assert_eq!(entry.encrypted_cek_values[0].key_path, "https://vault/key");
        assert_eq!(entry.encrypted_cek_values[0].algorithm_name, "RSA_OAEP");
        assert_eq!(entry.encrypted_cek_values[0].encrypted_key, vec![9, 9, 9]);
    }

    #[test]
    fn accumulates_multiple_values_for_same_ordinal() {
        let mut entries = HashMap::new();
        accumulate_cek_entry(&mut entries, &rs1_row(0, "PROVIDER_A", vec![1])).unwrap();
        accumulate_cek_entry(&mut entries, &rs1_row(0, "PROVIDER_B", vec![2])).unwrap();

        assert_eq!(entries.len(), 1);
        let entry = &entries[&0];
        assert_eq!(entry.encrypted_cek_values.len(), 2);
        assert_eq!(entry.encrypted_cek_values[0].key_store_name, "PROVIDER_A");
        assert_eq!(entry.encrypted_cek_values[1].key_store_name, "PROVIDER_B");
    }

    #[test]
    fn accumulates_distinct_ordinals() {
        let mut entries = HashMap::new();
        accumulate_cek_entry(&mut entries, &rs1_row(0, "P0", vec![0])).unwrap();
        accumulate_cek_entry(&mut entries, &rs1_row(1, "P1", vec![1])).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains_key(&0));
        assert!(entries.contains_key(&1));
    }

    #[test]
    fn parses_encrypted_parameter() {
        let info = parse_parameter_info(&rs2_row(1, "@p1", 1, 1, 0, 1)).unwrap();
        assert_eq!(info.parameter_ordinal, 1);
        assert_eq!(info.parameter_name, "@p1");
        assert_eq!(info.cipher_algorithm_id, 1);
        assert_eq!(info.encryption_type, 1);
        assert_eq!(info.cek_ordinal, 0);
        assert_eq!(info.normalization_rule_version, 1);
        assert!(info.is_encrypted());
    }

    #[test]
    fn parses_plaintext_parameter() {
        let info = parse_parameter_info(&rs2_row(2, "@p2", 0, 0, 0, 0)).unwrap();
        assert!(!info.is_encrypted());
    }

    #[test]
    fn rejects_wrong_md_version_length() {
        let mut entries = HashMap::new();
        let mut row = rs1_row(0, "P", vec![1]);
        row[RS1_KEY_MD_VERSION] = ColumnValues::Bytes(vec![1, 2, 3]);
        assert!(accumulate_cek_entry(&mut entries, &row).is_err());
    }

    #[test]
    fn rejects_non_integer_ordinal() {
        let row = rs2_row(1, "@p1", 1, 1, 0, 1);
        let mut bad = row.clone();
        bad[RS2_COLUMN_ENCRYPTION_KEY_ORDINAL] = sql("not-an-int");
        assert!(parse_parameter_info(&bad).is_err());
    }
}
