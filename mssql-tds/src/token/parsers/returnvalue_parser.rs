// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;

use super::super::tokenitems::ReturnValueStatus;
use super::super::tokens::{ReturnValueToken, Tokens};
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{
    datatypes::{
        decoder::SqlTypeDecode,
        sqldatatypes::{TdsDataType, read_type_info},
    },
    io::token_stream::ParserContext,
    query::metadata::ColumnMetadata,
};

pub(crate) struct ReturnValueTokenParser<T>
where
    T: SqlTypeDecode,
{
    decoder: T,
}

impl<T: SqlTypeDecode + Default> Default for ReturnValueTokenParser<T> {
    fn default() -> Self {
        Self {
            decoder: T::default(),
        }
    }
}

#[async_trait]
impl<T: SqlTypeDecode + Sync, P: TdsPacketReader + Send + Sync> TokenParser<P>
    for ReturnValueTokenParser<T>
{
    async fn parse(&self, reader: &mut P, _context: &ParserContext) -> TdsResult<Tokens> {
        let param_ordinal = reader.read_uint16().await?;
        let param_name_length = reader.read_byte().await?;
        let byte_length = (param_name_length as usize).checked_mul(2).ok_or_else(|| {
            crate::error::Error::ProtocolError(format!(
                "Parameter name length overflow: {param_name_length}"
            ))
        })?;
        let param_name = reader.read_unicode_with_byte_length(byte_length).await?;
        let status_byte = reader.read_byte().await?;
        let status = ReturnValueStatus::from(status_byte);
        let user_type = reader.read_uint32().await?;
        let flags = reader.read_uint16().await?;
        let tds_type = reader.read_byte().await?;
        let type_info = read_type_info(reader, TdsDataType::try_from(tds_type)?).await?;

        // Per [MS-TDS] an encrypted RETURNVALUE is laid out as
        // `TYPE_INFO || CryptoMetaData || Value`, where `TYPE_INFO` is the
        // encrypted wire type (`varbinary`) and `CryptoMetaData` carries the
        // base (plaintext) type plus cipher parameters. RETURNVALUE carries no
        // CEK table, so `parse_crypto_metadata` is called with
        // `has_cek_table = false`. The value on the wire is the ciphertext; it
        // is decoded below as `varbinary` and decrypted by the connection, which
        // holds the parameter's column encryption key (an encrypted output
        // parameter reuses the CEK resolved when the matching input parameter
        // was encrypted).
        const FLAG_ENCRYPTED: u16 = 0x0800;
        let crypto_metadata = if flags & FLAG_ENCRYPTED != 0 {
            Some(super::colmetadata_parser::parse_crypto_metadata(reader, false).await?)
        } else {
            None
        };

        let column_metadata = ColumnMetadata {
            user_type,
            flags,
            data_type: TdsDataType::try_from(tds_type)?,
            type_info,
            column_name: param_name.clone(),
            multi_part_name: None,
            crypto_metadata,
        };
        // For an encrypted parameter this decodes the ciphertext as `varbinary`
        // (the wire type); the connection then decrypts it into the real value.
        // For a plaintext parameter it decodes the value directly.
        let value = self.decoder.decode(reader, &column_metadata).await?;

        Ok(Tokens::from(ReturnValueToken {
            param_ordinal,
            param_name,
            value,
            column_metadata: Box::new(column_metadata),
            status,
        }))
    }
}
