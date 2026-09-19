// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{
    core::TdsResult, datatypes::sqltypes::SqlType, io::packet_writer::PacketWriter,
    message::parameters::rpc_parameters::RpcTypeMetadata, token::tokens::SqlCollation,
};

pub(crate) trait SqlValueEncoder {
    async fn encode_sqlvalue(
        &self,
        packet_writer: &mut PacketWriter<'_>,
        sql_value: &SqlType,
        db_collation: &SqlCollation,
        type_metadata: Option<RpcTypeMetadata>,
    ) -> TdsResult<()>;
}

pub struct GenericEncoder {}

impl GenericEncoder {
    pub fn new() -> Self {
        Self {}
    }
}

impl SqlValueEncoder for GenericEncoder {
    async fn encode_sqlvalue(
        &self,
        packet_writer: &mut PacketWriter<'_>,
        sql_value: &SqlType,
        db_collation: &SqlCollation,
        type_metadata: Option<RpcTypeMetadata>,
    ) -> TdsResult<()> {
        sql_value
            .serialize(packet_writer, db_collation, type_metadata)
            .await?;
        Ok(())
    }
}
