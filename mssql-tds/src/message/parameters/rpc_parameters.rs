// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitflags::bitflags;

use crate::datatypes::column_values::DEFAULT_VARTIME_SCALE;
use crate::datatypes::encoder::SqlValueEncoder;
use crate::datatypes::sql_tvp::TvpTypeName;
use crate::datatypes::sqltypes::SqlType;
use crate::{
    core::TdsResult,
    datatypes::sqldatatypes::TdsDataType,
    error::Error,
    io::packet_writer::{PacketWriter, TdsPacketWriter},
    token::tokens::SqlCollation,
};

/// Maximum byte length sent inline (non-PLP) for a BIGVARBINARY value; longer
/// ciphertexts use PLP chunked encoding. Mirrors JDBC
/// `DataTypes.SHORT_VARTYPE_MAX_BYTES`.
const SHORT_VARTYPE_MAX_BYTES: usize = 8000;

/// PLP length sentinel written as the BIGVARBINARY max length for values that
/// exceed [`SHORT_VARTYPE_MAX_BYTES`]. Mirrors JDBC `DataTypes.SQL_USHORTVARMAXLEN`.
const SQL_USHORTVARMAXLEN: u16 = 0xFFFF;

bitflags! {
    /// TDS RPC parameter status flags.
    ///
    /// Controls how the server interprets each parameter value. Use
    /// [`BY_REF_VALUE`](Self::BY_REF_VALUE) for output parameters.
    #[derive(Debug, Clone, Copy)]
    pub struct StatusFlags: u8 {
        /// No flags set.
        const NONE = 0b0000_0000;
        /// Parameter is passed by reference (output parameter).
        const BY_REF_VALUE = 0b0000_0001;
        /// Use the parameter's default value.
        const DEFAULT_VALUE = 0b0000_0010;
        /// Reserved by the TDS protocol.
        const RESERVED_BIT_1 = 0b0000_0100;
        /// Parameter value is encrypted.
        const ENCRYPTED = 0b0000_1000;
        /// Reserved by the TDS protocol.
        const RESERVED_BIT_4 = 0b0001_0000;
    }
}

/// Cipher metadata for an encrypted RPC parameter.
///
/// Written as the `CryptoMetaData` block following an encrypted parameter's
/// value in an RPC request (MS-TDS 2.2.6.6). These fields are populated from
/// the results of `sp_describe_parameter_encryption`. Mirrors JDBC
/// `writeCryptoMetaData`.
#[derive(Debug, Clone)]
pub(crate) struct RpcEncryptionMetadata {
    /// Cipher algorithm id (`0x01` = AEAD_AES_256_CBC_HMAC_SHA256).
    pub(crate) cipher_algorithm_id: u8,
    /// Encryption type (`1` = deterministic, `2` = randomized).
    pub(crate) encryption_type: u8,
    /// Database id of the column encryption key.
    pub(crate) database_id: i32,
    /// Column encryption key id.
    pub(crate) cek_id: i32,
    /// Column encryption key version.
    pub(crate) cek_version: i32,
    /// Column encryption key metadata version (8 bytes).
    pub(crate) cek_md_version: [u8; 8],
    /// Normalization rule version (currently `1`).
    pub(crate) normalization_rule_version: u8,
}

/// An encrypted RPC parameter value: the ciphertext (or `None` for an encrypted
/// NULL) plus the cipher metadata the server needs to decrypt it.
#[derive(Debug, Clone)]
pub(crate) struct EncryptedRpcValue {
    /// Ciphertext bytes, or `None` for a NULL value.
    pub(crate) ciphertext: Option<Vec<u8>>,
    /// Cipher metadata describing how the value was encrypted.
    pub(crate) metadata: RpcEncryptionMetadata,
}

/// A single parameter in a TDS RPC request.
///
/// Construct with [`RpcParameter::new`], supplying an optional name, status
/// flags, and a [`SqlType`] value. Named parameters (e.g. `Some("@id".into())`)
/// are matched by name on the server; positional parameters (`None`) are
/// matched by ordinal.
#[derive(Debug, Clone)]
pub struct RpcParameter {
    /// The name of the parameter, if applicable. For positional
    /// parameters, this will be `None`.
    pub(crate) name: Option<String>,

    /// Options for the parameter. This is a bitmask
    /// represents whether the parameter is input, output, or both, as well as the encryption setting.
    options: StatusFlags,

    /// The data type and value of the parameter.
    ///  This is used to determine how to serialize the value.
    value: SqlType,

    /// When present, the parameter is sent encrypted (Always Encrypted): the
    /// ciphertext is serialized as a BIGVARBINARY with the ENCRYPTED status flag
    /// and a trailing CryptoMetaData block, bypassing the plaintext `value`.
    encrypted: Option<EncryptedRpcValue>,
}

impl RpcParameter {
    /// Creates a new RPC parameter.
    pub fn new(name: Option<String>, options: StatusFlags, value: SqlType) -> Self {
        Self {
            name,
            options,
            value,
            encrypted: None,
        }
    }

    /// Get the SQL type name from a SqlType value for use in parameter declarations.
    /// This is used to build the parameter list string for sp_executesql and sp_prepare.
    ///
    /// Returns [`Error::ImplementationError`] if the `SqlType` maps to a [`TdsDataType`]
    /// variant that has no SQL declaration name (see [`TdsDataType::get_meta_type_name`]).
    #[cfg(fuzzing)]
    pub fn get_sql_name(value: &SqlType) -> TdsResult<String> {
        Self::get_sql_name_impl(value)
    }

    #[cfg(not(fuzzing))]
    pub(crate) fn get_sql_name(value: &SqlType) -> TdsResult<String> {
        Self::get_sql_name_impl(value)
    }

    fn get_sql_name_impl(value: &SqlType) -> TdsResult<String> {
        // Table-valued parameters are declared by their schema-qualified table
        // type name with the mandatory `READONLY` suffix, not via a base TDS
        // type name (which `get_meta_type_name` would reject for `SqlTable`).
        if let SqlType::Table(type_name, _) = value {
            return Ok(Self::format_tvp_sql_name(type_name));
        }

        // For nullable types, we need to check the actual datatype to derive the name.
        let tds_type = TdsDataType::from(value);
        let type_name = tds_type.get_meta_type_name()?;

        let len_in_metadata = match value {
            SqlType::NVarcharMax(_) | SqlType::VarBinaryMax(_) | SqlType::VarcharMax(_) => {
                "MAX".to_string()
            }
            SqlType::Varchar(_, len) | SqlType::VarBinary(_, len) | SqlType::NVarchar(_, len) => {
                // The user may have specified an large length length.
                // But we will send it across without tampering and let the server handle it.
                // We want to send the length as a string based on the intention of API usage, so
                // that the intention of the user is translated. The same params will also be used by server
                // for prepared statements. Hence we shouldn't try to be intelligent here.
                if (*len > 8000
                    && matches!(value, SqlType::Varchar(_, _) | SqlType::VarBinary(_, _)))
                    || (*len > 4000 && matches!(value, SqlType::NVarchar(_, _)))
                {
                    "MAX".to_string()
                } else {
                    len.to_string()
                }
            }
            SqlType::Binary(_, len) => {
                // For binary types, we need to send the length.
                len.to_string()
            }
            SqlType::Char(_, len) | SqlType::NChar(_, len) => {
                // For Char and NChar, send the declared length as `char(N)` / `nchar(N)`.
                len.to_string()
            }
            SqlType::Time(time) => {
                // For time, we need to send the scale as the length.
                match time {
                    // If the time is not specified, we assume the default scale.
                    // This is a common case for time types.
                    Some(time) => time.get_scale().to_string(),
                    _ => DEFAULT_VARTIME_SCALE.to_string(), // Default scale for Time
                }
            }
            SqlType::DateTime2(datetime2) => {
                // For DateTime2, we need to send the scale as the length.
                match datetime2 {
                    Some(val) => val.time.get_scale().to_string(),
                    None => DEFAULT_VARTIME_SCALE.to_string(), // Default scale for DateTime2
                }
            }
            SqlType::DateTimeOffset(datetimeoffset) => {
                // For DateTimeoffset, we need to send the scale as the length.
                match datetimeoffset {
                    Some(val) => val.datetime2.time.get_scale().to_string(),
                    None => DEFAULT_VARTIME_SCALE.to_string(), // Default scale for DateTimeOffset
                }
            }
            SqlType::Decimal(value) | SqlType::Numeric(value) => {
                // For Decimal and Numeric, we need to send the precision and scale as the length.
                // The format is "precision,scale".
                match value {
                    Some(parts) => {
                        format!("{},{}", parts.precision, parts.scale)
                    }
                    None => "18, 10".to_string(), // Default precision and scale
                }
            }
            SqlType::Vector(_, dims, _) => dims.to_string(),
            _ => "".to_string(),
        };

        if len_in_metadata.is_empty() {
            Ok(type_name.to_string())
        } else {
            Ok(format!("{type_name}({len_in_metadata})"))
        }
    }

    /// Formats a table-valued parameter's declaration name for `sp_executesql`,
    /// e.g. `[dbo].[MyType] READONLY`.
    ///
    /// The schema defaults to `dbo` when unspecified (SQL Server's default
    /// schema). The catalog/database part is intentionally omitted: SQL Server
    /// forbids cross-database TVP types in parameter declarations. The
    /// `READONLY` suffix is mandatory for TVP parameters.
    fn format_tvp_sql_name(type_name: &TvpTypeName) -> String {
        let schema = type_name.schema_name.as_deref().unwrap_or("dbo");
        format!("[{schema}].[{}] READONLY", type_name.type_name)
    }

    /// Serializes the RPC parameter into the provided `PacketWriter`.
    /// The `encoder` is used to encode the parameter value based on its data type.
    /// The `db_collation` is used for string types to determine the collation.
    /// The `is_positional` flag indicates whether the parameter is positional or named.
    pub(crate) async fn serialize<T: SqlValueEncoder>(
        &self,
        packet_writer: &mut PacketWriter<'_>,
        db_collation: &SqlCollation,
        is_positional: bool,
        encoder: &T,
    ) -> TdsResult<()> {
        // If the parameter is positional, then we dont need to write the name.
        if is_positional {
            // Indicates that the parameter name is 0 length, since this is
            // a positional parameter.
            packet_writer.write_byte_async(0).await?;
        } else {
            match self.name {
                Some(ref name) => {
                    if name.len() > 0xFF {
                        return Err(Error::UsageError(
                            "Parameter name is too long. Maximum length is 255 characters."
                                .to_string(),
                        ));
                    }
                    let name_length = name.len() as u8;
                    // We can only send byte length.
                    packet_writer.write_byte_async(name_length).await?;
                    packet_writer.write_string_unicode_async(name).await?;
                }
                None => {
                    // Since this is not a positional parameter,
                    // we expect that a name is provided.
                    // If it is not provided, then the called made a mistake.
                    // Since this is a programming error, we can panic.
                    unreachable!(
                        "Parameter name is None for a non-positional parameter. Unexpected implemetation path"
                    );
                }
            }
        }

        // Encrypted parameters bypass the normal value encoder: the ciphertext
        // is sent as a BIGVARBINARY with the ENCRYPTED status flag and a
        // trailing CryptoMetaData block (Always Encrypted).
        if let Some(encrypted) = &self.encrypted {
            Self::write_encrypted(packet_writer, self.options, encrypted).await?;
            return Ok(());
        }

        // Write the options byte.
        packet_writer.write_byte_async(self.options.bits()).await?;

        encoder
            .encode_sqlvalue(packet_writer, &self.value, db_collation)
            .await?;
        Ok(())
    }

    /// Marks this parameter as encrypted, supplying the ciphertext (or `None`
    /// for an encrypted NULL) and the cipher metadata. When set, [`serialize`]
    /// writes the value as a BIGVARBINARY with the ENCRYPTED status flag and a
    /// trailing CryptoMetaData block instead of encoding the plaintext value.
    #[allow(dead_code)]
    pub(crate) fn set_encrypted(
        &mut self,
        ciphertext: Option<Vec<u8>>,
        metadata: RpcEncryptionMetadata,
    ) {
        self.encrypted = Some(EncryptedRpcValue {
            ciphertext,
            metadata,
        });
    }

    /// Returns the parameter's plaintext value. Used by the parameter-encryption
    /// path to normalize and encrypt the value before sending.
    #[allow(dead_code)]
    pub(crate) fn value(&self) -> &SqlType {
        &self.value
    }

    /// Serializes the parameter's value in its encrypted form: the ENCRYPTED
    /// status flag, a BIGVARBINARY TYPE_INFO carrying the ciphertext, and the
    /// trailing CryptoMetaData block. Encrypted values carry no collation.
    /// Mirrors JDBC `writeEncryptedRPCByteArray` + `writeCryptoMetaData`
    /// (MS-TDS 2.2.6.6).
    async fn write_encrypted(
        packet_writer: &mut PacketWriter<'_>,
        options: StatusFlags,
        encrypted: &EncryptedRpcValue,
    ) -> TdsResult<()> {
        // Always mark the parameter encrypted, preserving any output flag.
        let status = options | StatusFlags::ENCRYPTED;
        packet_writer.write_byte_async(status.bits()).await?;

        // The ciphertext is transmitted as BIGVARBINARY.
        packet_writer
            .write_byte_async(TdsDataType::BigVarBinary as u8)
            .await?;

        Self::write_encrypted_value(packet_writer, encrypted.ciphertext.as_deref()).await?;
        Self::write_crypto_metadata(packet_writer, &encrypted.metadata).await?;
        Ok(())
    }

    /// Writes the BIGVARBINARY max-length, actual-length, and value bytes for an
    /// encrypted parameter. Short values (<= 8000 bytes) are written inline;
    /// longer values use PLP chunked encoding. A `None` value is written as a
    /// NULL (actual length `-1`).
    async fn write_encrypted_value(
        packet_writer: &mut PacketWriter<'_>,
        value: Option<&[u8]>,
    ) -> TdsResult<()> {
        let len = value.map_or(0, <[u8]>::len);
        let is_short = len <= SHORT_VARTYPE_MAX_BYTES;

        // Declared max length: 8000 for short values, PLP sentinel otherwise.
        if is_short {
            packet_writer
                .write_u16_async(SHORT_VARTYPE_MAX_BYTES as u16)
                .await?;
        } else {
            packet_writer.write_u16_async(SQL_USHORTVARMAXLEN).await?;
        }

        match value {
            // NULL value: actual length of -1.
            None => packet_writer.write_i16_async(-1).await?,
            Some(bytes) if is_short => {
                packet_writer.write_u16_async(len as u16).await?;
                if len > 0 {
                    packet_writer.write_async(bytes).await?;
                }
            }
            Some(bytes) => {
                // PLP: 8-byte total length, then a single length-prefixed chunk,
                // then the PLP terminator (4 zero bytes).
                packet_writer.write_u64_async(len as u64).await?;
                packet_writer.write_u32_async(len as u32).await?;
                packet_writer.write_async(bytes).await?;
                packet_writer.write_u32_async(0).await?;
            }
        }
        Ok(())
    }

    /// Writes the CryptoMetaData block for an encrypted parameter. Mirrors JDBC
    /// `writeCryptoMetaData` (MS-TDS 2.2.6.6).
    async fn write_crypto_metadata(
        packet_writer: &mut PacketWriter<'_>,
        metadata: &RpcEncryptionMetadata,
    ) -> TdsResult<()> {
        packet_writer
            .write_byte_async(metadata.cipher_algorithm_id)
            .await?;
        packet_writer
            .write_byte_async(metadata.encryption_type)
            .await?;
        packet_writer.write_i32_async(metadata.database_id).await?;
        packet_writer.write_i32_async(metadata.cek_id).await?;
        packet_writer.write_i32_async(metadata.cek_version).await?;
        packet_writer.write_async(&metadata.cek_md_version).await?;
        packet_writer
            .write_byte_async(metadata.normalization_rule_version)
            .await?;
        Ok(())
    }

    /// Access to the value field for fuzzing
    #[cfg(fuzzing)]
    pub fn get_value(&self) -> &SqlType {
        &self.value
    }
}

/// Builds a comma-separated list of parameter names and types for the RPC call.
/// This is used to construct the parameter declaration string for sp_executesql.
#[cfg(fuzzing)]
pub fn build_parameter_list_string(
    named_params: &Vec<RpcParameter>,
    params_list: &mut String,
) -> TdsResult<()> {
    build_parameter_list_string_impl(named_params, params_list)
}

#[cfg(not(fuzzing))]
pub(crate) fn build_parameter_list_string(
    named_params: &Vec<RpcParameter>,
    params_list: &mut String,
) -> TdsResult<()> {
    build_parameter_list_string_impl(named_params, params_list)
}

fn build_parameter_list_string_impl(
    named_params: &Vec<RpcParameter>,
    params_list: &mut String,
) -> TdsResult<()> {
    let mut first_param = true;
    for param in named_params {
        if let Some(param_name) = &param.name {
            // TODO: while persisting types with length, we need to compute the length and
            // add the length after the type name. e.g. Nvarchar(200), varchar(100) etc.
            let param_type_name = RpcParameter::get_sql_name(&param.value)?;
            if first_param {
                first_param = false;
            } else {
                params_list.push_str(", ");
            }
            params_list.push_str(&format!("{param_name} {param_type_name} "));
        }
    }
    Ok(())
}

impl From<&SqlType> for TdsDataType {
    fn from(value: &SqlType) -> TdsDataType {
        match value {
            SqlType::Bit(_) => TdsDataType::Bit,
            SqlType::TinyInt(_) => TdsDataType::Int1,
            SqlType::SmallInt(_) => TdsDataType::Int2,
            SqlType::Int(_) => TdsDataType::Int4,
            SqlType::BigInt(_) => TdsDataType::Int8,
            SqlType::Real(_) => TdsDataType::Flt4,
            SqlType::Float(_) => TdsDataType::Flt8,
            SqlType::Decimal(_) => TdsDataType::DecimalN,
            SqlType::Numeric(_) => TdsDataType::NumericN,
            SqlType::NVarchar(_, _) => TdsDataType::NVarChar,
            SqlType::VarBinary(_, _) => TdsDataType::BigVarBinary,
            SqlType::Binary(_, _) => TdsDataType::BigBinary,
            SqlType::Char(_, _) => TdsDataType::Char,
            SqlType::NChar(_, _) => TdsDataType::NChar,
            SqlType::Text(_) => TdsDataType::Text,
            SqlType::NText(_) => TdsDataType::NText,
            SqlType::Json(_) => TdsDataType::Json,
            SqlType::Money(_) => TdsDataType::Money,
            SqlType::SmallMoney(_) => TdsDataType::Money4,
            SqlType::Time(_) => TdsDataType::TimeN,
            SqlType::DateTime2(_) => TdsDataType::DateTime2N,
            SqlType::DateTimeOffset(_) => TdsDataType::DateTimeOffsetN,
            SqlType::SmallDateTime(_) => TdsDataType::DateTim4,
            SqlType::NVarcharMax(_) => TdsDataType::NVarChar,
            SqlType::Varchar(_, _) => TdsDataType::VarChar,
            SqlType::VarcharMax(_) => TdsDataType::VarChar,
            SqlType::VarBinaryMax(_) => TdsDataType::VarBinary,
            SqlType::Xml(_) => TdsDataType::Xml,
            SqlType::Uuid(_) => TdsDataType::Guid,
            SqlType::DateTime(_) => TdsDataType::DateTime,
            SqlType::Date(_) => TdsDataType::DateN,
            SqlType::Vector(_, _, _) => TdsDataType::Vector,
            SqlType::Variant(_) => TdsDataType::SsVariant,
            SqlType::Table(_, _) => TdsDataType::SqlTable,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::datatypes::sqltypes::SqlType;
    use crate::error::Error;
    use crate::message::parameters::rpc_parameters::RpcParameter;
    use crate::message::parameters::rpc_parameters::{
        EncryptedRpcValue, RpcEncryptionMetadata, StatusFlags,
    };

    use crate::datatypes::encoder::GenericEncoder;
    use crate::io::packet_writer::PacketWriter;
    use crate::io::packet_writer::tests::MockNetworkWriter;
    use crate::message::messages::PacketType;
    use crate::token::tokens::SqlCollation;
    use futures::executor::block_on;

    /// Returns the RPC payload bytes written to the packet writer, stripping the
    /// 8-byte packet header.
    fn payload(writer: &PacketWriter) -> Vec<u8> {
        writer.get_payload().into_inner()[8..].to_vec()
    }

    /// Sample cipher metadata for encrypted-parameter serialization tests.
    fn sample_metadata() -> RpcEncryptionMetadata {
        RpcEncryptionMetadata {
            cipher_algorithm_id: 1,
            encryption_type: 1,
            database_id: 7,
            cek_id: 11,
            cek_version: 3,
            cek_md_version: [1, 2, 3, 4, 5, 6, 7, 8],
            normalization_rule_version: 1,
        }
    }

    /// The CryptoMetaData block bytes for [`sample_metadata`].
    fn sample_metadata_bytes() -> Vec<u8> {
        let mut b = vec![0x01, 0x01]; // cipher_algorithm_id, encryption_type
        b.extend_from_slice(&7i32.to_le_bytes()); // database_id
        b.extend_from_slice(&11i32.to_le_bytes()); // cek_id
        b.extend_from_slice(&3i32.to_le_bytes()); // cek_version
        b.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // cek_md_version
        b.push(0x01); // normalization_rule_version
        b
    }

    fn serialize_param(param: &RpcParameter) -> Vec<u8> {
        // Use a packet size large enough that even the PLP test stays within a
        // single packet, so the payload is contiguous (no interspersed headers).
        let mut mock = MockNetworkWriter::new(16384);
        let mut w = PacketWriter::new(PacketType::RpcRequest, &mut mock, None, None);
        let collation = SqlCollation::default();
        let encoder = GenericEncoder {};
        block_on(param.serialize(&mut w, &collation, false, &encoder)).unwrap();
        payload(&w)
    }

    /// A short encrypted value is serialized as a BIGVARBINARY with the
    /// ENCRYPTED status flag, an inline length-prefixed value, and the trailing
    /// CryptoMetaData block.
    #[test]
    fn serialize_encrypted_short_value() {
        let mut param = RpcParameter::new(
            Some("@p".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(5)),
        );
        param.set_encrypted(Some(vec![0xAA, 0xBB, 0xCC, 0xDD]), sample_metadata());

        let mut expected = vec![0x02, 0x40, 0x00, 0x70, 0x00]; // name: len 2, "@p" UTF-16LE
        expected.push(0x08); // status: ENCRYPTED
        expected.push(0xA5); // type: BIGVARBINARY
        expected.extend_from_slice(&8000u16.to_le_bytes()); // max length
        expected.extend_from_slice(&4u16.to_le_bytes()); // actual length
        expected.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // ciphertext
        expected.extend_from_slice(&sample_metadata_bytes());

        assert_eq!(serialize_param(&param), expected);
    }

    /// An encrypted NULL value writes an actual length of -1 and no value bytes,
    /// still followed by the CryptoMetaData block.
    #[test]
    fn serialize_encrypted_null_value() {
        let mut param = RpcParameter::new(
            Some("@p".to_string()),
            StatusFlags::NONE,
            SqlType::Int(None),
        );
        param.set_encrypted(None, sample_metadata());

        let mut expected = vec![0x02, 0x40, 0x00, 0x70, 0x00];
        expected.push(0x08); // status: ENCRYPTED
        expected.push(0xA5); // type: BIGVARBINARY
        expected.extend_from_slice(&8000u16.to_le_bytes()); // max length
        expected.extend_from_slice(&(-1i16).to_le_bytes()); // NULL actual length
        expected.extend_from_slice(&sample_metadata_bytes());

        assert_eq!(serialize_param(&param), expected);
    }

    /// A value longer than 8000 bytes uses PLP chunked encoding: a PLP max-length
    /// sentinel, an 8-byte total length, a single length-prefixed chunk, and the
    /// PLP terminator, then the CryptoMetaData block.
    #[test]
    fn serialize_encrypted_plp_value() {
        let ciphertext = vec![0x55u8; 8001];
        let mut param = RpcParameter::new(
            Some("@p".to_string()),
            StatusFlags::NONE,
            SqlType::VarBinaryMax(None),
        );
        param.set_encrypted(Some(ciphertext.clone()), sample_metadata());

        let mut expected = vec![0x02, 0x40, 0x00, 0x70, 0x00];
        expected.push(0x08); // status: ENCRYPTED
        expected.push(0xA5); // type: BIGVARBINARY
        expected.extend_from_slice(&0xFFFFu16.to_le_bytes()); // PLP max-length sentinel
        expected.extend_from_slice(&8001u64.to_le_bytes()); // total length
        expected.extend_from_slice(&8001u32.to_le_bytes()); // chunk length
        expected.extend_from_slice(&ciphertext); // chunk data
        expected.extend_from_slice(&0u32.to_le_bytes()); // PLP terminator
        expected.extend_from_slice(&sample_metadata_bytes());

        assert_eq!(serialize_param(&param), expected);
    }

    /// The ENCRYPTED status flag is forced on even when the parameter's options
    /// did not include it, preserving the BY_REF (output) flag.
    #[test]
    fn serialize_encrypted_preserves_output_flag() {
        let mut param = RpcParameter::new(
            Some("@p".to_string()),
            StatusFlags::BY_REF_VALUE,
            SqlType::Int(Some(5)),
        );
        param.set_encrypted(Some(vec![0x01]), sample_metadata());

        let bytes = serialize_param(&param);
        // Status byte follows the 5-byte name prefix.
        assert_eq!(
            bytes[5],
            StatusFlags::BY_REF_VALUE.bits() | StatusFlags::ENCRYPTED.bits()
        );
    }

    /// `EncryptedRpcValue` round-trips through `set_encrypted`.
    #[test]
    fn set_encrypted_stores_value() {
        let mut param = RpcParameter::new(
            Some("@p".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(5)),
        );
        param.set_encrypted(Some(vec![9, 9]), sample_metadata());
        let stored: &EncryptedRpcValue = param.encrypted.as_ref().expect("encrypted set");
        assert_eq!(stored.ciphertext.as_deref(), Some(&[9u8, 9][..]));
        assert_eq!(stored.metadata.cek_id, 11);
    }

    #[test]
    fn test_get_sql_names() {
        let decimal =
            crate::datatypes::decoder::DecimalParts::from_i64(12345, 18, 5).expect("decimal parts");
        let cases: Vec<(SqlType, &str)> = vec![
            (SqlType::NVarchar(None, 50), "nvarchar(50)"),
            (SqlType::VarBinary(None, 100), "varbinary(100)"),
            (SqlType::Time(None), "time(7)"),
            (SqlType::DateTimeOffset(None), "datetimeoffset(7)"),
            (SqlType::DateTime2(None), "datetime2(7)"),
            (SqlType::NVarcharMax(None), "nvarchar(MAX)"),
            (SqlType::VarcharMax(None), "varchar(MAX)"),
            (SqlType::NVarchar(None, 4000), "nvarchar(4000)"),
            (SqlType::Varchar(None, 4000), "varchar(4000)"),
            (SqlType::VarBinary(None, 4000), "varbinary(4000)"),
            (SqlType::VarBinaryMax(None), "varbinary(MAX)"),
            (
                SqlType::Vector(
                    None,
                    3,
                    crate::datatypes::sqldatatypes::VectorBaseType::Float32,
                ),
                "vector(3)",
            ),
            // GH #45: SqlType::Numeric must not error when generating the RPC parameter
            // declaration. Covers both the value-present and value-absent paths.
            (SqlType::Numeric(Some(decimal)), "numeric(18,5)"),
            (SqlType::Numeric(None), "numeric(18, 10)"),
            // Sibling fix: SqlType::Char / SqlType::NChar must produce `char(N)` / `nchar(N)`.
            (SqlType::Char(None, 10), "char(10)"),
            (SqlType::NChar(None, 25), "nchar(25)"),
            // sql_variant declares as `sql_variant` with no length suffix.
            (
                SqlType::Variant(Box::new(SqlType::Int(Some(1)))),
                "sql_variant",
            ),
        ];
        for (sql_type, expected) in cases {
            let rpc_param = RpcParameter::get_sql_name(&sql_type)
                .unwrap_or_else(|e| panic!("get_sql_name failed for {sql_type:?}: {e}"));
            assert_eq!(rpc_param, expected, "case: {sql_type:?}");
        }
    }

    /// `get_sql_name` must surface `Error::ImplementationError` when the underlying
    /// `TdsDataType` has no SQL declaration name, rather than panicking. There is no
    /// `SqlType` that currently routes to such a variant, so this is exercised by
    /// constructing the `TdsDataType` directly.
    #[test]
    fn test_get_sql_name_propagates_implementation_error() {
        use crate::datatypes::sqldatatypes::TdsDataType;
        let err = TdsDataType::IntN.get_meta_type_name().expect_err(
            "TdsDataType::IntN should have no SQL declaration name; \
             update test if you added a mapping.",
        );
        assert!(matches!(err, Error::ImplementationError(_)));
    }

    /// Table-valued parameters are declared by their schema-qualified table type
    /// name with the mandatory `READONLY` suffix; the schema defaults to `dbo`.
    #[test]
    fn test_get_sql_name_tvp() {
        use crate::datatypes::sql_tvp::TvpTypeName;

        let schema_qualified = SqlType::Table(
            TvpTypeName::new(Some("sales".to_string()), "OrderList".to_string()),
            None,
        );
        assert_eq!(
            RpcParameter::get_sql_name(&schema_qualified).unwrap(),
            "[sales].[OrderList] READONLY"
        );

        let default_schema = SqlType::Table(TvpTypeName::new(None, "OrderList".to_string()), None);
        assert_eq!(
            RpcParameter::get_sql_name(&default_schema).unwrap(),
            "[dbo].[OrderList] READONLY"
        );
    }

    /// A `SqlType::Table` maps to the `SqlTable` TDS wire type.
    #[test]
    fn test_tds_data_type_from_table() {
        use crate::datatypes::sql_tvp::TvpTypeName;
        use crate::datatypes::sqldatatypes::TdsDataType;

        let value = SqlType::Table(TvpTypeName::new(None, "OrderList".to_string()), None);
        assert_eq!(TdsDataType::from(&value), TdsDataType::SqlTable);
    }
}
