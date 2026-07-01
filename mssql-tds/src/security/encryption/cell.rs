// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cell decryption and denormalization for Always Encrypted (read path).
//!
//! When a column is encrypted, SQL Server sends each cell as an opaque cipher
//! blob (a `varbinary`) instead of the column's real type. To produce the
//! plaintext value the driver must:
//!
//! 1. Decrypt the blob with the column's encryption key using the
//!    [`AeadAes256CbcHmacSha256`] cipher, yielding the *normalized* plaintext
//!    bytes (the form SQL Server encrypts).
//! 2. *Denormalize* those bytes back into a typed [`ColumnValues`] using the
//!    column's base type information.
//!
//! The normalization rules (version 1) are defined by SQL Server and matched
//! against the reference drivers (the JDBC driver's `dtv.denormalizedValue`):
//!
//! * Integer types (`bit`/`tinyint`/`smallint`/`int`/`bigint`) are normalized to
//!   an 8-byte little-endian `bigint`.
//! * `real`/`float` keep their 4- or 8-byte IEEE form.
//! * `smallmoney`/`money` are normalized to the 8-byte `money` form (high dword
//!   then low dword); `smallmoney` uses the low dword.
//! * `decimal`/`numeric` are a sign byte followed by the little-endian magnitude.
//! * `binary`/`varbinary` and `uniqueidentifier` keep their raw bytes.
//! * Character types keep their encoded bytes (UTF-16LE for the `n` types, the
//!   collation code page otherwise).

// Consumed by the ROW/NBCROW decode path in the next Always Encrypted phase.
#![allow(dead_code)]

use uuid::Uuid;

use super::aead_aes_256_cbc_hmac_sha256::AeadAes256CbcHmacSha256;
use crate::core::TdsResult;
use crate::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime,
};
use crate::datatypes::decoder::DecimalParts;
use crate::datatypes::sql_string::{EncodingType, SqlString};
use crate::datatypes::sqldatatypes::{TdsDataType, TypeInfo, TypeInfoVariant, is_unicode_type};
use crate::datatypes::sqltypes::SqlType;
use crate::error::Error;
use crate::query::metadata::CryptoMetadata;
use crate::security::encryption::ColumnEncryptionType;

/// Cipher algorithm id for `AEAD_AES_256_CBC_HMAC_SHA256`.
///
/// This is the value SQL Server uses to identify the algorithm both in the
/// COLMETADATA crypto metadata and in the `sp_describe_parameter_encryption`
/// result set (`TdsEnums.AEAD_AES_256_CBC_HMAC_SHA256` in the other drivers).
const AEAD_AES_256_CBC_HMAC_SHA256_ALGORITHM_ID: u8 = 0x02;
/// The only cell-normalization rule version defined by SQL Server today.
const SUPPORTED_NORMALIZATION_VERSION: u8 = 0x01;

/// Decrypts an encrypted cell blob and denormalizes it into a typed value.
///
/// `plaintext_cek` is the decrypted column encryption key (see
/// [`crate::security::keystore::decrypt_cek`]); `cipher_blob` is the raw cell
/// value read from the ROW token.
///
/// # Errors
///
/// Returns [`Error::ColumnEncryptionError`] if the cipher algorithm is
/// unsupported, decryption fails, or the plaintext cannot be denormalized into
/// the column's base type.
pub(crate) fn decrypt_cell(
    crypto_metadata: &CryptoMetadata,
    plaintext_cek: &[u8],
    cipher_blob: &[u8],
) -> TdsResult<ColumnValues> {
    if crypto_metadata.cipher_algorithm_id != AEAD_AES_256_CBC_HMAC_SHA256_ALGORITHM_ID {
        return Err(Error::ColumnEncryptionError(format!(
            "Unsupported cell cipher algorithm id {:#04x} (expected \
             {AEAD_AES_256_CBC_HMAC_SHA256_ALGORITHM_ID:#04x})",
            crypto_metadata.cipher_algorithm_id
        )));
    }

    let cipher = AeadAes256CbcHmacSha256::new(plaintext_cek)?;
    let plaintext = cipher.decrypt(cipher_blob)?;

    denormalize(
        &plaintext,
        crypto_metadata.base_data_type,
        &crypto_metadata.base_type_info,
        crypto_metadata.normalization_rule_version,
    )
}

/// Converts normalized plaintext bytes back into a typed [`ColumnValues`].
pub(crate) fn denormalize(
    plaintext: &[u8],
    base_data_type: TdsDataType,
    base_type_info: &TypeInfo,
    normalization_rule_version: u8,
) -> TdsResult<ColumnValues> {
    if normalization_rule_version != SUPPORTED_NORMALIZATION_VERSION {
        return Err(Error::ColumnEncryptionError(format!(
            "Unsupported Always Encrypted normalization rule version {normalization_rule_version} \
             (expected {SUPPORTED_NORMALIZATION_VERSION})"
        )));
    }

    match base_data_type {
        // Integer family: normalized to an 8-byte little-endian bigint.
        TdsDataType::Bit | TdsDataType::BitN => Ok(ColumnValues::Bit(read_i64(plaintext)? != 0)),
        TdsDataType::Int1 => Ok(ColumnValues::TinyInt(read_i64(plaintext)? as u8)),
        TdsDataType::Int2 => Ok(ColumnValues::SmallInt(read_i64(plaintext)? as i16)),
        TdsDataType::Int4 => Ok(ColumnValues::Int(read_i64(plaintext)? as i32)),
        TdsDataType::Int8 => Ok(ColumnValues::BigInt(read_i64(plaintext)?)),
        TdsDataType::IntN => {
            let value = read_i64(plaintext)?;
            match base_type_info.length {
                1 => Ok(ColumnValues::TinyInt(value as u8)),
                2 => Ok(ColumnValues::SmallInt(value as i16)),
                4 => Ok(ColumnValues::Int(value as i32)),
                8 => Ok(ColumnValues::BigInt(value)),
                other => Err(Error::ColumnEncryptionError(format!(
                    "Invalid IntN base length {other} for encrypted column"
                ))),
            }
        }

        // Floating point: keeps its 4- or 8-byte IEEE form.
        TdsDataType::Flt4 => Ok(ColumnValues::Real(read_f32(plaintext)?)),
        TdsDataType::Flt8 => Ok(ColumnValues::Float(read_f64(plaintext)?)),
        TdsDataType::FltN => match plaintext.len() {
            4 => Ok(ColumnValues::Real(read_f32(plaintext)?)),
            8 => Ok(ColumnValues::Float(read_f64(plaintext)?)),
            other => Err(Error::ColumnEncryptionError(format!(
                "Invalid FltN plaintext length {other} (expected 4 or 8)"
            ))),
        },

        // Money: normalized to the 8-byte money form (high dword then low dword).
        TdsDataType::Money => Ok(ColumnValues::Money(read_money(plaintext)?)),
        TdsDataType::Money4 => Ok(ColumnValues::SmallMoney(read_small_money(plaintext)?)),
        TdsDataType::MoneyN => match base_type_info.length {
            4 => Ok(ColumnValues::SmallMoney(read_small_money(plaintext)?)),
            8 => Ok(ColumnValues::Money(read_money(plaintext)?)),
            other => Err(Error::ColumnEncryptionError(format!(
                "Invalid MoneyN base length {other} for encrypted column"
            ))),
        },

        // Decimal/numeric: sign byte followed by the little-endian magnitude.
        TdsDataType::Decimal | TdsDataType::DecimalN => Ok(ColumnValues::Decimal(read_decimal(
            plaintext,
            base_type_info,
        )?)),
        TdsDataType::Numeric | TdsDataType::NumericN => Ok(ColumnValues::Numeric(read_decimal(
            plaintext,
            base_type_info,
        )?)),

        // Binary: raw bytes.
        TdsDataType::Binary
        | TdsDataType::VarBinary
        | TdsDataType::BigBinary
        | TdsDataType::BigVarBinary
        | TdsDataType::Image => Ok(ColumnValues::Bytes(plaintext.to_vec())),

        // uniqueidentifier: 16 little-endian bytes.
        TdsDataType::Guid => {
            let uuid = Uuid::from_slice_le(plaintext).map_err(|e| {
                Error::ColumnEncryptionError(format!("Invalid uniqueidentifier plaintext: {e}"))
            })?;
            Ok(ColumnValues::Uuid(uuid))
        }

        // Character types: keep the encoded bytes; the encoding follows the base type.
        TdsDataType::Char
        | TdsDataType::VarChar
        | TdsDataType::BigChar
        | TdsDataType::BigVarChar
        | TdsDataType::Text
        | TdsDataType::NChar
        | TdsDataType::NVarChar
        | TdsDataType::NText => {
            let encoding = string_encoding(base_data_type, base_type_info)?;
            Ok(ColumnValues::String(SqlString::new(
                plaintext.to_vec(),
                encoding,
            )))
        }

        // Temporal types: the normalized form matches the TDS row value layout
        // (without the length prefix). Scale comes from the base TYPE_INFO.
        TdsDataType::DateN => Ok(ColumnValues::Date(read_date(plaintext)?)),
        TdsDataType::TimeN => {
            let scale = temporal_scale(base_type_info)?;
            Ok(ColumnValues::Time(read_time(plaintext, scale)?))
        }
        TdsDataType::DateTime2N => {
            let scale = temporal_scale(base_type_info)?;
            Ok(ColumnValues::DateTime2(read_datetime2(plaintext, scale)?))
        }
        TdsDataType::DateTimeOffsetN => {
            let scale = temporal_scale(base_type_info)?;
            Ok(ColumnValues::DateTimeOffset(read_datetime_offset(
                plaintext, scale,
            )?))
        }
        TdsDataType::DateTim4 => Ok(ColumnValues::SmallDateTime(read_small_datetime(plaintext)?)),
        TdsDataType::DateTime => Ok(ColumnValues::DateTime(read_datetime(plaintext)?)),
        TdsDataType::DateTimeN => match base_type_info.length {
            4 => Ok(ColumnValues::SmallDateTime(read_small_datetime(plaintext)?)),
            8 => Ok(ColumnValues::DateTime(read_datetime(plaintext)?)),
            other => Err(Error::ColumnEncryptionError(format!(
                "Invalid DateTimeN base length {other} for encrypted column"
            ))),
        },

        // Anything else is not yet denormalized.
        other => Err(Error::ColumnEncryptionError(format!(
            "Denormalization is not yet implemented for encrypted base type {other:?}"
        ))),
    }
}

/// Reads an exact 8-byte little-endian `i64` (the normalized integer form).
fn read_i64(bytes: &[u8]) -> TdsResult<i64> {
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| length_error("8-byte integer", bytes.len()))?;
    Ok(i64::from_le_bytes(array))
}

/// Reads an exact 4-byte little-endian `f32`.
fn read_f32(bytes: &[u8]) -> TdsResult<f32> {
    let array: [u8; 4] = bytes
        .try_into()
        .map_err(|_| length_error("4-byte real", bytes.len()))?;
    Ok(f32::from_le_bytes(array))
}

/// Reads an exact 8-byte little-endian `f64`.
fn read_f64(bytes: &[u8]) -> TdsResult<f64> {
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| length_error("8-byte float", bytes.len()))?;
    Ok(f64::from_le_bytes(array))
}

/// Reads a little-endian `i32` from a 4-byte slice.
fn read_i32_at(bytes: &[u8], offset: usize) -> TdsResult<i32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| length_error("4-byte money component", bytes.len()))?;
    let array: [u8; 4] = slice.try_into().expect("slice is exactly 4 bytes");
    Ok(i32::from_le_bytes(array))
}

/// Reads the 8-byte normalized `money` form (high dword then low dword).
fn read_money(bytes: &[u8]) -> TdsResult<SqlMoney> {
    if bytes.len() != 8 {
        return Err(length_error("8-byte money", bytes.len()));
    }
    Ok(SqlMoney {
        msb_part: read_i32_at(bytes, 0)?,
        lsb_part: read_i32_at(bytes, 4)?,
    })
}

/// Reads a normalized `smallmoney` (the low dword of the 8-byte money form).
fn read_small_money(bytes: &[u8]) -> TdsResult<SqlSmallMoney> {
    if bytes.len() != 8 {
        return Err(length_error("8-byte smallmoney", bytes.len()));
    }
    Ok(SqlSmallMoney {
        int_val: read_i32_at(bytes, 4)?,
    })
}

/// Reads the normalized decimal/numeric form: a sign byte (`1` = positive)
/// followed by the little-endian magnitude in 32-bit groups.
fn read_decimal(bytes: &[u8], base_type_info: &TypeInfo) -> TdsResult<DecimalParts> {
    let (precision, scale) = match base_type_info.type_info_variant {
        TypeInfoVariant::VarLenPrecisionScale(_, _, precision, scale) => (precision, scale),
        ref other => {
            return Err(Error::ColumnEncryptionError(format!(
                "Invalid base type info for decimal/numeric: {other:?}"
            )));
        }
    };

    let (sign, magnitude) = bytes
        .split_first()
        .ok_or_else(|| length_error("decimal sign byte", bytes.len()))?;

    if magnitude.len() % 4 != 0 {
        return Err(Error::ColumnEncryptionError(format!(
            "Invalid decimal magnitude length {} (must be a multiple of 4)",
            magnitude.len()
        )));
    }

    let int_parts = magnitude
        .chunks_exact(4)
        .map(|chunk| i32::from_le_bytes(chunk.try_into().expect("chunk is exactly 4 bytes")))
        .collect();

    Ok(DecimalParts {
        is_positive: *sign == 1,
        scale,
        precision,
        int_parts,
    })
}

/// Determines the encoding for an encrypted character column from its base type.
fn string_encoding(
    base_data_type: TdsDataType,
    base_type_info: &TypeInfo,
) -> TdsResult<EncodingType> {
    if is_unicode_type(base_data_type) {
        return Ok(EncodingType::Utf16);
    }

    let collation = match base_type_info.type_info_variant {
        TypeInfoVariant::VarLenString(_, _, collation) => collation,
        TypeInfoVariant::PartialLen(_, _, collation, _, _) => collation,
        _ => None,
    };

    match collation {
        Some(collation) if collation.utf8() => Ok(EncodingType::Utf8),
        Some(collation) => Ok(EncodingType::LcidBased(collation)),
        None => Err(Error::ColumnEncryptionError(
            "Encrypted character column is missing collation information".to_string(),
        )),
    }
}

/// Builds a length-mismatch error.
fn length_error(expected: &str, actual: usize) -> Error {
    Error::ColumnEncryptionError(format!(
        "Invalid normalized plaintext length for {expected}: got {actual} bytes"
    ))
}

/// Extracts the fractional-seconds scale from a temporal column's base type.
fn temporal_scale(base_type_info: &TypeInfo) -> TdsResult<u8> {
    match base_type_info.type_info_variant {
        TypeInfoVariant::VarLenScale(_, scale) => Ok(scale),
        ref other => Err(Error::ColumnEncryptionError(format!(
            "Encrypted temporal column is missing scale information: {other:?}"
        ))),
    }
}

/// Reads a little-endian unsigned integer (1..=8 bytes) into a `u64`.
fn read_uint_le(bytes: &[u8], width: usize, what: &str) -> TdsResult<u64> {
    if bytes.len() != width {
        return Err(length_error(what, bytes.len()));
    }
    let mut value: u64 = 0;
    for (index, byte) in bytes.iter().enumerate() {
        value |= (*byte as u64) << (8 * index);
    }
    Ok(value)
}

/// Reads a `date`: a 3-byte little-endian day count since 0001-01-01.
fn read_date(bytes: &[u8]) -> TdsResult<SqlDate> {
    let days = read_uint_le(bytes, 3, "3-byte date")? as u32;
    Ok(SqlDate::unchecked_create(days))
}

/// Reads a `time(scale)`: a 3/4/5-byte little-endian scaled value, converted to
/// 100-nanosecond units (matching the row decoder).
fn read_time(bytes: &[u8], scale: u8) -> TdsResult<SqlTime> {
    let scaled_value = match bytes.len() {
        3 => read_uint_le(bytes, 3, "time")?,
        4 => read_uint_le(bytes, 4, "time")?,
        5 => read_uint_le(bytes, 5, "time")?,
        other => return Err(length_error("3/4/5-byte time", other)),
    };
    let multiplier = if scale <= 7 {
        10u64.pow(u32::from(7 - scale))
    } else {
        1
    };
    Ok(SqlTime {
        time_nanoseconds: scaled_value * multiplier,
        scale,
    })
}

/// Reads a `datetime2(scale)`: the time component followed by a 3-byte date.
fn read_datetime2(bytes: &[u8], scale: u8) -> TdsResult<SqlDateTime2> {
    let date_start = bytes
        .len()
        .checked_sub(3)
        .ok_or_else(|| length_error("datetime2 (time + 3-byte date)", bytes.len()))?;
    let time = read_time(&bytes[..date_start], scale)?;
    let date = read_date(&bytes[date_start..])?;
    Ok(SqlDateTime2 {
        days: date.get_days(),
        time,
    })
}

/// Reads a `datetimeoffset(scale)`: a `datetime2` followed by a 2-byte signed
/// UTC offset in minutes.
fn read_datetime_offset(bytes: &[u8], scale: u8) -> TdsResult<SqlDateTimeOffset> {
    let offset_start = bytes
        .len()
        .checked_sub(2)
        .ok_or_else(|| length_error("datetimeoffset (datetime2 + 2-byte offset)", bytes.len()))?;
    let datetime2 = read_datetime2(&bytes[..offset_start], scale)?;
    let offset = i16::from_le_bytes(
        bytes[offset_start..]
            .try_into()
            .expect("slice is exactly 2 bytes"),
    );
    Ok(SqlDateTimeOffset { datetime2, offset })
}

/// Reads a legacy `smalldatetime`: 2-byte day count and 2-byte minute count.
fn read_small_datetime(bytes: &[u8]) -> TdsResult<SqlSmallDateTime> {
    if bytes.len() != 4 {
        return Err(length_error("4-byte smalldatetime", bytes.len()));
    }
    Ok(SqlSmallDateTime {
        days: read_uint_le(&bytes[0..2], 2, "smalldatetime days")? as u16,
        time: read_uint_le(&bytes[2..4], 2, "smalldatetime minutes")? as u16,
    })
}

/// Reads a legacy `datetime`: a 4-byte signed day count and 4-byte tick count.
fn read_datetime(bytes: &[u8]) -> TdsResult<SqlDateTime> {
    if bytes.len() != 8 {
        return Err(length_error("8-byte datetime", bytes.len()));
    }
    let days = i32::from_le_bytes(bytes[0..4].try_into().expect("slice is exactly 4 bytes"));
    let time = u32::from_le_bytes(bytes[4..8].try_into().expect("slice is exactly 4 bytes"));
    Ok(SqlDateTime { days, time })
}

/// Encrypts a plaintext parameter value for Always Encrypted.
///
/// Normalizes `value` into the canonical plaintext byte form, then encrypts it
/// with the column encryption key using `AEAD_AES_256_CBC_HMAC_SHA256`.
/// Returns `Ok(None)` when the parameter value is SQL `NULL` (a NULL parameter
/// is sent with no ciphertext).
///
/// # Errors
///
/// Returns [`Error::ColumnEncryptionError`] if the cipher algorithm, encryption
/// type, or normalization rule version is unsupported, if the value's type is
/// not normalizable, or if encryption fails.
pub(crate) fn encrypt_parameter(
    value: &SqlType,
    plaintext_cek: &[u8],
    cipher_algorithm_id: u8,
    encryption_type: u8,
    normalization_rule_version: u8,
) -> TdsResult<Option<Vec<u8>>> {
    if cipher_algorithm_id != AEAD_AES_256_CBC_HMAC_SHA256_ALGORITHM_ID {
        return Err(Error::ColumnEncryptionError(format!(
            "Unsupported parameter cipher algorithm id {cipher_algorithm_id:#04x} (expected \
             {AEAD_AES_256_CBC_HMAC_SHA256_ALGORITHM_ID:#04x})"
        )));
    }
    if normalization_rule_version != SUPPORTED_NORMALIZATION_VERSION {
        return Err(Error::ColumnEncryptionError(format!(
            "Unsupported Always Encrypted normalization rule version {normalization_rule_version} \
             (expected {SUPPORTED_NORMALIZATION_VERSION})"
        )));
    }

    let column_encryption_type = match encryption_type {
        1 => ColumnEncryptionType::Deterministic,
        2 => ColumnEncryptionType::Randomized,
        other => {
            return Err(Error::ColumnEncryptionError(format!(
                "Unsupported Always Encrypted encryption type {other} (expected 1 or 2)"
            )));
        }
    };

    let Some(normalized) = normalize(value)? else {
        return Ok(None);
    };

    let cipher = AeadAes256CbcHmacSha256::new(plaintext_cek)?;
    Ok(Some(cipher.encrypt(&normalized, column_encryption_type)?))
}

/// Normalizes a plaintext parameter value into the canonical byte form that
/// SQL Server encrypts (the inverse of [`denormalize`]).
///
/// Returns `Ok(None)` when the value is SQL `NULL`.
pub(crate) fn normalize(value: &SqlType) -> TdsResult<Option<Vec<u8>>> {
    let bytes = match value {
        // Integer family: an 8-byte little-endian bigint.
        SqlType::Bit(v) => v.map(|b| i64::from(b).to_le_bytes().to_vec()),
        SqlType::TinyInt(v) => v.map(|n| i64::from(n).to_le_bytes().to_vec()),
        SqlType::SmallInt(v) => v.map(|n| i64::from(n).to_le_bytes().to_vec()),
        SqlType::Int(v) => v.map(|n| i64::from(n).to_le_bytes().to_vec()),
        SqlType::BigInt(v) => v.map(|n| n.to_le_bytes().to_vec()),

        // Floating point: native IEEE form.
        SqlType::Real(v) => v.map(|f| f.to_le_bytes().to_vec()),
        SqlType::Float(v) => v.map(|f| f.to_le_bytes().to_vec()),

        // Money: the 8-byte money form (high dword then low dword).
        SqlType::Money(v) => v.as_ref().map(normalize_money),
        SqlType::SmallMoney(v) => v.as_ref().map(normalize_small_money),

        // Decimal/numeric: sign byte followed by the little-endian magnitude.
        SqlType::Decimal(v) | SqlType::Numeric(v) => v.as_ref().map(normalize_decimal),

        // uniqueidentifier: 16 little-endian bytes.
        SqlType::Uuid(v) => v.map(|u| u.to_bytes_le().to_vec()),

        // Binary: raw bytes.
        SqlType::Binary(v, _) | SqlType::VarBinary(v, _) | SqlType::VarBinaryMax(v) => v.clone(),

        // Character types: the already-encoded bytes (UTF-16LE for `n` types,
        // the collation code page otherwise).
        SqlType::NVarchar(v, _)
        | SqlType::NVarcharMax(v)
        | SqlType::NChar(v, _)
        | SqlType::NText(v)
        | SqlType::Varchar(v, _)
        | SqlType::VarcharMax(v)
        | SqlType::Char(v, _)
        | SqlType::Text(v) => v.as_ref().map(|s| s.bytes.clone()),

        // Temporal types: the row-value layout (without the length prefix).
        SqlType::Date(v) => v.as_ref().map(normalize_date),
        SqlType::Time(v) => return v.as_ref().map(normalize_time).transpose(),
        SqlType::DateTime2(v) => return v.as_ref().map(normalize_datetime2).transpose(),
        SqlType::DateTimeOffset(v) => {
            return v.as_ref().map(normalize_datetime_offset).transpose();
        }
        SqlType::SmallDateTime(v) => v.as_ref().map(normalize_small_datetime),
        SqlType::DateTime(v) => v.as_ref().map(normalize_datetime),

        other => {
            return Err(Error::ColumnEncryptionError(format!(
                "Always Encrypted normalization is not implemented for parameter type {other:?}"
            )));
        }
    };
    Ok(bytes)
}

/// Normalizes and encrypts a plaintext bulk-copy cell value for an encrypted
/// destination column. This is the bulk-copy counterpart to
/// [`encrypt_parameter`]: bulk copy works with [`ColumnValues`] rather than the
/// RPC [`SqlType`] representation.
///
/// Returns `Ok(None)` when the value is SQL `NULL`.
pub(crate) fn encrypt_cell_value(
    value: &ColumnValues,
    plaintext_cek: &[u8],
    cipher_algorithm_id: u8,
    encryption_type: u8,
    normalization_rule_version: u8,
) -> TdsResult<Option<Vec<u8>>> {
    if cipher_algorithm_id != AEAD_AES_256_CBC_HMAC_SHA256_ALGORITHM_ID {
        return Err(Error::ColumnEncryptionError(format!(
            "Unsupported cell cipher algorithm id {cipher_algorithm_id:#04x} (expected \
             {AEAD_AES_256_CBC_HMAC_SHA256_ALGORITHM_ID:#04x})"
        )));
    }
    if normalization_rule_version != SUPPORTED_NORMALIZATION_VERSION {
        return Err(Error::ColumnEncryptionError(format!(
            "Unsupported Always Encrypted normalization rule version {normalization_rule_version} \
             (expected {SUPPORTED_NORMALIZATION_VERSION})"
        )));
    }

    let column_encryption_type = match encryption_type {
        1 => ColumnEncryptionType::Deterministic,
        2 => ColumnEncryptionType::Randomized,
        other => {
            return Err(Error::ColumnEncryptionError(format!(
                "Unsupported Always Encrypted encryption type {other} (expected 1 or 2)"
            )));
        }
    };

    let Some(normalized) = normalize_column_value(value)? else {
        return Ok(None);
    };

    let cipher = AeadAes256CbcHmacSha256::new(plaintext_cek)?;
    Ok(Some(cipher.encrypt(&normalized, column_encryption_type)?))
}

/// Normalizes a plaintext bulk-copy cell value into the canonical byte form that
/// SQL Server encrypts. This is the bulk-copy counterpart to [`normalize`]
/// (which operates on RPC [`SqlType`] values) and the inverse of the
/// denormalization performed by [`denormalize`].
///
/// Returns `Ok(None)` when the value is SQL `NULL`.
pub(crate) fn normalize_column_value(value: &ColumnValues) -> TdsResult<Option<Vec<u8>>> {
    let bytes = match value {
        ColumnValues::Null => None,

        // Integer family: an 8-byte little-endian bigint.
        ColumnValues::Bit(v) => Some(i64::from(*v).to_le_bytes().to_vec()),
        ColumnValues::TinyInt(v) => Some(i64::from(*v).to_le_bytes().to_vec()),
        ColumnValues::SmallInt(v) => Some(i64::from(*v).to_le_bytes().to_vec()),
        ColumnValues::Int(v) => Some(i64::from(*v).to_le_bytes().to_vec()),
        ColumnValues::BigInt(v) => Some(v.to_le_bytes().to_vec()),

        // Floating point: native IEEE form.
        ColumnValues::Real(v) => Some(v.to_le_bytes().to_vec()),
        ColumnValues::Float(v) => Some(v.to_le_bytes().to_vec()),

        // Money: the 8-byte money form (high dword then low dword).
        ColumnValues::Money(v) => Some(normalize_money(v)),
        ColumnValues::SmallMoney(v) => Some(normalize_small_money(v)),

        // Decimal/numeric: sign byte followed by the little-endian magnitude.
        ColumnValues::Decimal(v) | ColumnValues::Numeric(v) => Some(normalize_decimal(v)),

        // uniqueidentifier: 16 little-endian bytes.
        ColumnValues::Uuid(v) => Some(v.to_bytes_le().to_vec()),

        // Binary: raw bytes.
        ColumnValues::Bytes(v) => Some(v.clone()),

        // Character types: the already-encoded bytes (UTF-16LE for `n` types,
        // the collation code page otherwise).
        ColumnValues::String(s) => Some(s.bytes.clone()),

        // Temporal types: the row-value layout (without the length prefix).
        ColumnValues::Date(v) => Some(normalize_date(v)),
        ColumnValues::Time(v) => Some(normalize_time(v)?),
        ColumnValues::DateTime2(v) => Some(normalize_datetime2(v)?),
        ColumnValues::DateTimeOffset(v) => Some(normalize_datetime_offset(v)?),
        ColumnValues::SmallDateTime(v) => Some(normalize_small_datetime(v)),
        ColumnValues::DateTime(v) => Some(normalize_datetime(v)),

        other => {
            return Err(Error::ColumnEncryptionError(format!(
                "Always Encrypted normalization is not implemented for bulk-copy value {other:?}"
            )));
        }
    };
    Ok(bytes)
}

/// Normalizes a `money` value to the 8-byte form (high dword then low dword).
fn normalize_money(m: &SqlMoney) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&m.msb_part.to_le_bytes());
    out.extend_from_slice(&m.lsb_part.to_le_bytes());
    out
}

/// Normalizes a `smallmoney` value to the 8-byte money form (value in low dword).
fn normalize_small_money(m: &SqlSmallMoney) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&m.int_val.to_le_bytes());
    out
}

/// Normalizes a `decimal`/`numeric` value: a sign byte (`1` = positive) followed
/// by the little-endian magnitude in 32-bit groups.
fn normalize_decimal(d: &DecimalParts) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + d.int_parts.len() * 4);
    out.push(u8::from(d.is_positive));
    for part in &d.int_parts {
        out.extend_from_slice(&part.to_le_bytes());
    }
    out
}

/// Returns the on-wire byte width of a `time(scale)` value.
fn time_byte_width(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

/// Normalizes a `date` value to a 3-byte little-endian day count.
fn normalize_date(d: &SqlDate) -> Vec<u8> {
    d.get_days().to_le_bytes()[..3].to_vec()
}

/// Normalizes a `time(scale)` value to its 3/4/5-byte scaled form.
fn normalize_time(t: &SqlTime) -> TdsResult<Vec<u8>> {
    let scale = t.scale;
    if scale > 7 {
        return Err(Error::ColumnEncryptionError(format!(
            "Invalid time scale {scale} (expected 0..=7)"
        )));
    }
    let multiplier = 10u64.pow(u32::from(7 - scale));
    let scaled = t.time_nanoseconds / multiplier;
    Ok(scaled.to_le_bytes()[..time_byte_width(scale)].to_vec())
}

/// Normalizes a `datetime2(scale)` value: the time component followed by the
/// 3-byte date.
fn normalize_datetime2(dt: &SqlDateTime2) -> TdsResult<Vec<u8>> {
    let mut out = normalize_time(&dt.time)?;
    out.extend_from_slice(&dt.days.to_le_bytes()[..3]);
    Ok(out)
}

/// Normalizes a `datetimeoffset(scale)` value: the `datetime2` form followed by
/// a 2-byte signed UTC offset in minutes.
fn normalize_datetime_offset(dto: &SqlDateTimeOffset) -> TdsResult<Vec<u8>> {
    let mut out = normalize_datetime2(&dto.datetime2)?;
    out.extend_from_slice(&dto.offset.to_le_bytes());
    Ok(out)
}

/// Normalizes a `smalldatetime` value: 2-byte day count and 2-byte minute count.
fn normalize_small_datetime(sdt: &SqlSmallDateTime) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    out.extend_from_slice(&sdt.days.to_le_bytes());
    out.extend_from_slice(&sdt.time.to_le_bytes());
    out
}

/// Normalizes a legacy `datetime` value: 4-byte signed day count and 4-byte
/// tick count.
fn normalize_datetime(dt: &SqlDateTime) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&dt.days.to_le_bytes());
    out.extend_from_slice(&dt.time.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::sqldatatypes::{FixedLengthTypes, TypeInfoVariant, VariableLengthTypes};
    use crate::security::encryption::ColumnEncryptionType;

    /// A fixed 32-byte CEK used to round-trip through the cipher.
    const CEK: [u8; 32] = [
        0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38,
        0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
        0x48, 0x49,
    ];

    fn type_info(tds_type: TdsDataType, length: usize, variant: TypeInfoVariant) -> TypeInfo {
        TypeInfo {
            tds_type,
            length,
            type_info_variant: variant,
        }
    }

    fn crypto(base_data_type: TdsDataType, base_type_info: TypeInfo) -> CryptoMetadata {
        CryptoMetadata {
            cek_table_ordinal: 0,
            base_data_type,
            base_type_info,
            cipher_algorithm_id: AEAD_AES_256_CBC_HMAC_SHA256_ALGORITHM_ID,
            cipher_algorithm_name: None,
            encryption_type: 1,
            normalization_rule_version: 1,
        }
    }

    /// Encrypts `normalized` so it can be fed back through [`decrypt_cell`].
    fn encrypt(normalized: &[u8]) -> Vec<u8> {
        AeadAes256CbcHmacSha256::new(&CEK)
            .unwrap()
            .encrypt(normalized, ColumnEncryptionType::Randomized)
            .unwrap()
    }

    #[test]
    fn denormalize_int_family() {
        // int normalized to 8-byte bigint.
        let info = type_info(
            TdsDataType::IntN,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let value = denormalize(&1234i64.to_le_bytes(), TdsDataType::IntN, &info, 1).unwrap();
        assert_eq!(value, ColumnValues::Int(1234));

        // tinyint
        let info = type_info(
            TdsDataType::IntN,
            1,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let value = denormalize(&200i64.to_le_bytes(), TdsDataType::IntN, &info, 1).unwrap();
        assert_eq!(value, ColumnValues::TinyInt(200));

        // bigint via fixed Int8
        let info = type_info(
            TdsDataType::Int8,
            8,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int8),
        );
        let value = denormalize(&(-5i64).to_le_bytes(), TdsDataType::Int8, &info, 1).unwrap();
        assert_eq!(value, ColumnValues::BigInt(-5));
    }

    #[test]
    fn denormalize_bit() {
        let info = type_info(
            TdsDataType::BitN,
            1,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Bit),
        );
        assert_eq!(
            denormalize(&1i64.to_le_bytes(), TdsDataType::BitN, &info, 1).unwrap(),
            ColumnValues::Bit(true)
        );
        assert_eq!(
            denormalize(&0i64.to_le_bytes(), TdsDataType::BitN, &info, 1).unwrap(),
            ColumnValues::Bit(false)
        );
    }

    #[test]
    fn denormalize_float() {
        let info = type_info(
            TdsDataType::FltN,
            8,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Flt8),
        );
        let value = denormalize(&1.5f64.to_le_bytes(), TdsDataType::FltN, &info, 1).unwrap();
        assert_eq!(value, ColumnValues::Float(1.5));

        let info = type_info(
            TdsDataType::FltN,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Flt4),
        );
        let value = denormalize(&2.5f32.to_le_bytes(), TdsDataType::FltN, &info, 1).unwrap();
        assert_eq!(value, ColumnValues::Real(2.5));
    }

    #[test]
    fn denormalize_money_forms() {
        // money: high dword then low dword. Represent 1.2345 (123450 / 10000).
        let amount: i64 = 12345;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((amount >> 32) as i32).to_le_bytes());
        bytes.extend_from_slice(&((amount & 0xFFFF_FFFF) as i32).to_le_bytes());
        let info = type_info(
            TdsDataType::MoneyN,
            8,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Money),
        );
        let value = denormalize(&bytes, TdsDataType::MoneyN, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::Money(SqlMoney {
                msb_part: 0,
                lsb_part: 12345,
            })
        );

        // smallmoney: low dword at offset 4.
        let info = type_info(
            TdsDataType::MoneyN,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Money4),
        );
        let value = denormalize(&bytes, TdsDataType::MoneyN, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::SmallMoney(SqlSmallMoney { int_val: 12345 })
        );
    }

    #[test]
    fn denormalize_decimal() {
        // value 12345 with scale 2 => 123.45, precision 5.
        let info = type_info(
            TdsDataType::DecimalN,
            5,
            TypeInfoVariant::VarLenPrecisionScale(VariableLengthTypes::DecimalN, 17, 5, 2),
        );
        let mut bytes = vec![1u8]; // positive sign
        bytes.extend_from_slice(&12345i32.to_le_bytes());
        let value = denormalize(&bytes, TdsDataType::DecimalN, &info, 1).unwrap();
        match value {
            ColumnValues::Decimal(parts) => {
                assert!(parts.is_positive);
                assert_eq!(parts.scale, 2);
                assert_eq!(parts.precision, 5);
                assert_eq!(parts.int_parts, vec![12345]);
            }
            other => panic!("expected Decimal, got {other:?}"),
        }
    }

    #[test]
    fn denormalize_binary_and_guid() {
        let info = type_info(
            TdsDataType::BigVarBinary,
            8000,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let value = denormalize(&[1, 2, 3, 4], TdsDataType::BigVarBinary, &info, 1).unwrap();
        assert_eq!(value, ColumnValues::Bytes(vec![1, 2, 3, 4]));

        let raw = [
            0x10u8, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
            0x1e, 0x1f,
        ];
        let info = type_info(
            TdsDataType::Guid,
            16,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let value = denormalize(&raw, TdsDataType::Guid, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::Uuid(Uuid::from_slice_le(&raw).unwrap())
        );
    }

    #[test]
    fn denormalize_unicode_string() {
        let info = type_info(
            TdsDataType::NVarChar,
            65535,
            TypeInfoVariant::VarLenString(VariableLengthTypes::NVarChar, 20, None),
        );
        let utf16: Vec<u8> = "Hi".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let value = denormalize(&utf16, TdsDataType::NVarChar, &info, 1).unwrap();
        match value {
            ColumnValues::String(s) => assert_eq!(s.to_utf8_string(), "Hi"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn denormalize_rejects_unknown_normalization_version() {
        let info = type_info(
            TdsDataType::Int4,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let error = denormalize(&0i64.to_le_bytes(), TdsDataType::Int4, &info, 2).unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    #[test]
    fn decrypt_cell_round_trips_through_cipher() {
        let info = type_info(
            TdsDataType::IntN,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let crypto_metadata = crypto(TdsDataType::IntN, info);
        let blob = encrypt(&987654i64.to_le_bytes());

        let value = decrypt_cell(&crypto_metadata, &CEK, &blob).unwrap();
        assert_eq!(value, ColumnValues::Int(987654));
    }

    #[test]
    fn decrypt_cell_rejects_unknown_algorithm() {
        let info = type_info(
            TdsDataType::Int4,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let mut crypto_metadata = crypto(TdsDataType::Int4, info);
        crypto_metadata.cipher_algorithm_id = 0x00;

        let error = decrypt_cell(&crypto_metadata, &CEK, &[0u8; 64]).unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    /// Builds a temporal base type carrying the given scale.
    fn temporal_type_info(
        tds_type: TdsDataType,
        variant_type: VariableLengthTypes,
        scale: u8,
    ) -> TypeInfo {
        type_info(
            tds_type,
            0,
            TypeInfoVariant::VarLenScale(variant_type, scale),
        )
    }

    #[test]
    fn denormalize_date() {
        // 2024-01-01 is day 739_251 since 0001-01-01.
        let days: u32 = 739_251;
        let bytes = &days.to_le_bytes()[0..3];
        let info = type_info(
            TdsDataType::DateN,
            3,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let value = denormalize(bytes, TdsDataType::DateN, &info, 1).unwrap();
        match value {
            ColumnValues::Date(date) => assert_eq!(date.get_days(), days),
            other => panic!("expected Date, got {other:?}"),
        }
    }

    #[test]
    fn denormalize_time() {
        // 01:02:03.0000000 = 3723 seconds = 37_230_000_000 100ns units.
        // Scale 7 keeps the 100ns value directly (5-byte scaled value).
        let scaled: u64 = 37_230_000_000;
        let bytes = &scaled.to_le_bytes()[0..5];
        let info = temporal_type_info(TdsDataType::TimeN, VariableLengthTypes::TimeN, 7);
        let value = denormalize(bytes, TdsDataType::TimeN, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::Time(SqlTime {
                time_nanoseconds: 37_230_000_000,
                scale: 7,
            })
        );
    }

    #[test]
    fn denormalize_time_scaled() {
        // Scale 0 stores seconds in 3 bytes; denormalize multiplies by 10^7.
        let seconds: u32 = 3723;
        let bytes = &seconds.to_le_bytes()[0..3];
        let info = temporal_type_info(TdsDataType::TimeN, VariableLengthTypes::TimeN, 0);
        let value = denormalize(bytes, TdsDataType::TimeN, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::Time(SqlTime {
                time_nanoseconds: 37_230_000_000,
                scale: 0,
            })
        );
    }

    #[test]
    fn denormalize_datetime2() {
        let days: u32 = 739_251;
        let scaled: u64 = 37_230_000_000; // scale 7, 100ns units
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&scaled.to_le_bytes()[0..5]);
        bytes.extend_from_slice(&days.to_le_bytes()[0..3]);
        let info = temporal_type_info(TdsDataType::DateTime2N, VariableLengthTypes::DateTime2N, 7);
        let value = denormalize(&bytes, TdsDataType::DateTime2N, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::DateTime2(SqlDateTime2 {
                days,
                time: SqlTime {
                    time_nanoseconds: 37_230_000_000,
                    scale: 7,
                },
            })
        );
    }

    #[test]
    fn denormalize_datetime_offset() {
        let days: u32 = 739_251;
        let scaled: u64 = 37_230_000_000;
        let offset: i16 = -330; // UTC-05:30
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&scaled.to_le_bytes()[0..5]);
        bytes.extend_from_slice(&days.to_le_bytes()[0..3]);
        bytes.extend_from_slice(&offset.to_le_bytes());
        let info = temporal_type_info(
            TdsDataType::DateTimeOffsetN,
            VariableLengthTypes::DateTimeOffsetN,
            7,
        );
        let value = denormalize(&bytes, TdsDataType::DateTimeOffsetN, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                datetime2: SqlDateTime2 {
                    days,
                    time: SqlTime {
                        time_nanoseconds: 37_230_000_000,
                        scale: 7,
                    },
                },
                offset,
            })
        );
    }

    #[test]
    fn denormalize_legacy_datetime() {
        // datetime: 4-byte signed days + 4-byte ticks.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&45_000i32.to_le_bytes());
        bytes.extend_from_slice(&1_080_000u32.to_le_bytes());
        let info = type_info(
            TdsDataType::DateTimeN,
            8,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let value = denormalize(&bytes, TdsDataType::DateTimeN, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::DateTime(SqlDateTime {
                days: 45_000,
                time: 1_080_000,
            })
        );

        // smalldatetime: 2-byte days + 2-byte minutes.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&45_000u16.to_le_bytes());
        bytes.extend_from_slice(&720u16.to_le_bytes());
        let info = type_info(
            TdsDataType::DateTimeN,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let value = denormalize(&bytes, TdsDataType::DateTimeN, &info, 1).unwrap();
        assert_eq!(
            value,
            ColumnValues::SmallDateTime(SqlSmallDateTime {
                days: 45_000,
                time: 720,
            })
        );
    }

    #[test]
    fn normalize_integer_is_8_byte_bigint() {
        assert_eq!(
            normalize(&SqlType::Int(Some(1234))).unwrap().unwrap(),
            1234i64.to_le_bytes().to_vec()
        );
        assert_eq!(
            normalize(&SqlType::TinyInt(Some(200))).unwrap().unwrap(),
            200i64.to_le_bytes().to_vec()
        );
        assert_eq!(
            normalize(&SqlType::Bit(Some(true))).unwrap().unwrap(),
            1i64.to_le_bytes().to_vec()
        );
    }

    #[test]
    fn normalize_null_returns_none() {
        assert!(normalize(&SqlType::Int(None)).unwrap().is_none());
        assert!(normalize(&SqlType::NVarcharMax(None)).unwrap().is_none());
    }

    #[test]
    fn normalize_string_keeps_encoded_bytes() {
        let s = SqlString::from_utf8_string("hi".to_string());
        let expected = s.bytes.clone();
        let normalized = normalize(&SqlType::NVarchar(Some(s), 10)).unwrap().unwrap();
        assert_eq!(normalized, expected);
    }

    #[test]
    fn normalize_money_layout() {
        let m = SqlMoney {
            msb_part: 1,
            lsb_part: 2,
        };
        let mut expected = 1i32.to_le_bytes().to_vec();
        expected.extend_from_slice(&2i32.to_le_bytes());
        assert_eq!(
            normalize(&SqlType::Money(Some(m))).unwrap().unwrap(),
            expected
        );
    }

    #[test]
    fn normalize_decimal_layout() {
        let d = DecimalParts {
            is_positive: false,
            scale: 2,
            precision: 10,
            int_parts: vec![1, 2],
        };
        let mut expected = vec![0u8];
        expected.extend_from_slice(&1i32.to_le_bytes());
        expected.extend_from_slice(&2i32.to_le_bytes());
        assert_eq!(
            normalize(&SqlType::Decimal(Some(d))).unwrap().unwrap(),
            expected
        );
    }

    #[test]
    fn encrypt_parameter_null_returns_none() {
        assert!(
            encrypt_parameter(&SqlType::Int(None), &CEK, 2, 1, 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn encrypt_parameter_rejects_unknown_algorithm() {
        let error = encrypt_parameter(&SqlType::Int(Some(1)), &CEK, 0, 1, 1).unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    #[test]
    fn encrypt_parameter_rejects_unknown_encryption_type() {
        let error = encrypt_parameter(&SqlType::Int(Some(1)), &CEK, 2, 9, 1).unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    #[test]
    fn encrypt_parameter_roundtrip_int() {
        let cipher = encrypt_parameter(&SqlType::Int(Some(987654)), &CEK, 2, 2, 1)
            .unwrap()
            .unwrap();
        let info = type_info(
            TdsDataType::IntN,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let mut meta = crypto(TdsDataType::IntN, info);
        meta.encryption_type = 2;
        assert_eq!(
            decrypt_cell(&meta, &CEK, &cipher).unwrap(),
            ColumnValues::Int(987654)
        );
    }

    #[test]
    fn encrypt_parameter_roundtrip_binary() {
        let cipher = encrypt_parameter(&SqlType::VarBinary(Some(vec![9, 8, 7]), 10), &CEK, 2, 2, 1)
            .unwrap()
            .unwrap();
        let info = type_info(
            TdsDataType::BigVarBinary,
            10,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let mut meta = crypto(TdsDataType::BigVarBinary, info);
        meta.encryption_type = 2;
        assert_eq!(
            decrypt_cell(&meta, &CEK, &cipher).unwrap(),
            ColumnValues::Bytes(vec![9, 8, 7])
        );
    }

    #[test]
    fn encrypt_parameter_roundtrip_guid() {
        let value = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let cipher = encrypt_parameter(&SqlType::Uuid(Some(value)), &CEK, 2, 1, 1)
            .unwrap()
            .unwrap();
        let info = type_info(
            TdsDataType::Guid,
            16,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let meta = crypto(TdsDataType::Guid, info);
        assert_eq!(
            decrypt_cell(&meta, &CEK, &cipher).unwrap(),
            ColumnValues::Uuid(value)
        );
    }

    #[test]
    fn encrypt_parameter_roundtrip_date() {
        let date = SqlDate::unchecked_create(739_251);
        let cipher = encrypt_parameter(&SqlType::Date(Some(date)), &CEK, 2, 2, 1)
            .unwrap()
            .unwrap();
        let info = type_info(
            TdsDataType::DateN,
            3,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let mut meta = crypto(TdsDataType::DateN, info);
        meta.encryption_type = 2;
        assert_eq!(
            decrypt_cell(&meta, &CEK, &cipher).unwrap(),
            ColumnValues::Date(SqlDate::unchecked_create(739_251))
        );
    }

    #[test]
    fn normalize_column_value_null_returns_none() {
        assert!(
            normalize_column_value(&ColumnValues::Null)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn normalize_column_value_integer_is_8_byte_bigint() {
        assert_eq!(
            normalize_column_value(&ColumnValues::Int(1234)).unwrap(),
            Some(1234i64.to_le_bytes().to_vec())
        );
        assert_eq!(
            normalize_column_value(&ColumnValues::TinyInt(200)).unwrap(),
            Some(200i64.to_le_bytes().to_vec())
        );
        assert_eq!(
            normalize_column_value(&ColumnValues::Bit(true)).unwrap(),
            Some(1i64.to_le_bytes().to_vec())
        );
    }

    #[test]
    fn encrypt_cell_value_null_returns_none() {
        assert!(
            encrypt_cell_value(&ColumnValues::Null, &CEK, 2, 1, 1)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn encrypt_cell_value_rejects_unknown_algorithm() {
        let error = encrypt_cell_value(&ColumnValues::Int(1), &CEK, 0, 1, 1).unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    #[test]
    fn encrypt_cell_value_rejects_unknown_encryption_type() {
        let error = encrypt_cell_value(&ColumnValues::Int(1), &CEK, 2, 9, 1).unwrap_err();
        assert!(matches!(error, Error::ColumnEncryptionError(_)));
    }

    #[test]
    fn encrypt_cell_value_roundtrip_int() {
        let cipher = encrypt_cell_value(&ColumnValues::Int(987654), &CEK, 2, 2, 1)
            .unwrap()
            .unwrap();
        let info = type_info(
            TdsDataType::IntN,
            4,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let mut meta = crypto(TdsDataType::IntN, info);
        meta.encryption_type = 2;
        assert_eq!(
            decrypt_cell(&meta, &CEK, &cipher).unwrap(),
            ColumnValues::Int(987654)
        );
    }

    #[test]
    fn encrypt_cell_value_roundtrip_binary() {
        let cipher = encrypt_cell_value(&ColumnValues::Bytes(vec![9, 8, 7]), &CEK, 2, 2, 1)
            .unwrap()
            .unwrap();
        let info = type_info(
            TdsDataType::BigVarBinary,
            10,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let mut meta = crypto(TdsDataType::BigVarBinary, info);
        meta.encryption_type = 2;
        assert_eq!(
            decrypt_cell(&meta, &CEK, &cipher).unwrap(),
            ColumnValues::Bytes(vec![9, 8, 7])
        );
    }

    #[test]
    fn encrypt_cell_value_roundtrip_guid() {
        let value = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let cipher = encrypt_cell_value(&ColumnValues::Uuid(value), &CEK, 2, 1, 1)
            .unwrap()
            .unwrap();
        let info = type_info(
            TdsDataType::Guid,
            16,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let meta = crypto(TdsDataType::Guid, info);
        assert_eq!(
            decrypt_cell(&meta, &CEK, &cipher).unwrap(),
            ColumnValues::Uuid(value)
        );
    }

    #[test]
    fn encrypt_cell_value_roundtrip_date() {
        let date = SqlDate::unchecked_create(739_251);
        let cipher = encrypt_cell_value(&ColumnValues::Date(date), &CEK, 2, 2, 1)
            .unwrap()
            .unwrap();
        let info = type_info(
            TdsDataType::DateN,
            3,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        let mut meta = crypto(TdsDataType::DateN, info);
        meta.encryption_type = 2;
        assert_eq!(
            decrypt_cell(&meta, &CEK, &cipher).unwrap(),
            ColumnValues::Date(SqlDate::unchecked_create(739_251))
        );
    }
}
