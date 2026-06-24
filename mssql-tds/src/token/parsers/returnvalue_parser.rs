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

        // TODO: Crypto metadata
        let column_metadata = ColumnMetadata {
            user_type,
            flags,
            data_type: TdsDataType::try_from(tds_type)?,
            type_info,
            column_name: param_name.clone(),
            multi_part_name: None,
            crypto_metadata: None,
        };
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
