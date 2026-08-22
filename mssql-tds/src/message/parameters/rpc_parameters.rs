// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitflags::bitflags;

use crate::datatypes::column_values::DEFAULT_VARTIME_SCALE;
use crate::datatypes::encoder::SqlValueEncoder;
use crate::datatypes::sqltypes::SqlType;
use crate::{
    core::TdsResult,
    datatypes::sqldatatypes::{TdsDataType, VectorBaseType},
    error::Error,
    io::packet_writer::{PacketWriter, TdsPacketWriter},
    token::tokens::SqlCollation,
};

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
}

impl RpcParameter {
    /// Creates a new RPC parameter.
    pub fn new(name: Option<String>, options: StatusFlags, value: SqlType) -> Self {
        Self {
            name,
            options,
            value,
        }
    }

    /// Get the SQL type name from a SqlType value for use in parameter declarations.
    /// This is used to build the parameter list string for sp_executesql and sp_prepare.
    #[cfg(fuzzing)]
    pub fn get_sql_name(value: &SqlType) -> String {
        Self::get_sql_name_impl(value)
    }

    #[cfg(not(fuzzing))]
    pub(crate) fn get_sql_name(value: &SqlType) -> String {
        Self::get_sql_name_impl(value)
    }

    fn get_sql_name_impl(value: &SqlType) -> String {
        // For nullable types, we need to check the actual datatype to derive the name.
        let tds_type = TdsDataType::from(value);
        let type_name = tds_type.get_meta_type_name();

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
            // `vector(N)` implies the float32 base type; float16 must be spelled out.
            SqlType::Vector(_, dims, base_type) => match base_type {
                VectorBaseType::Float32 => dims.to_string(),
                VectorBaseType::Float16 => format!("{dims}, float16"),
            },
            _ => "".to_string(),
        };

        if len_in_metadata.is_empty() {
            type_name.to_string()
        } else {
            format!("{type_name}({len_in_metadata})").to_string()
        }
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

        // Write the options byte.
        packet_writer.write_byte_async(self.options.bits()).await?;

        encoder
            .encode_sqlvalue(packet_writer, &self.value, db_collation)
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
pub fn build_parameter_list_string(named_params: &Vec<RpcParameter>, params_list: &mut String) {
    build_parameter_list_string_impl(named_params, params_list)
}

#[cfg(not(fuzzing))]
pub(crate) fn build_parameter_list_string(
    named_params: &Vec<RpcParameter>,
    params_list: &mut String,
) {
    build_parameter_list_string_impl(named_params, params_list)
}

fn build_parameter_list_string_impl(named_params: &Vec<RpcParameter>, params_list: &mut String) {
    let mut first_param = true;
    for param in named_params {
        if let Some(param_name) = &param.name {
            // TODO: while persisting types with length, we need to compute the length and
            // add the length after the type name. e.g. Nvarchar(200), varchar(100) etc.
            let param_type_name = RpcParameter::get_sql_name(&param.value);
            if first_param {
                first_param = false;
            } else {
                params_list.push_str(", ");
            }
            params_list.push_str(&format!("{param_name} {param_type_name} "));
        }
    }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::datatypes::sqltypes::SqlType;
    use crate::message::parameters::rpc_parameters::RpcParameter;

    #[test]
    fn test_get_sql_names() {
        let sql_type = SqlType::NVarchar(None, 50);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "nvarchar(50)".to_string());

        let sql_type = SqlType::VarBinary(None, 100);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "varbinary(100)".to_string());

        let sql_type = SqlType::Time(None);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "time(7)".to_string());

        let sql_type = SqlType::DateTimeOffset(None);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "datetimeoffset(7)".to_string());

        let sql_type = SqlType::DateTime2(None);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "datetime2(7)".to_string());

        let sql_type = SqlType::NVarcharMax(None);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "nvarchar(MAX)".to_string());

        let sql_type = SqlType::VarcharMax(None);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "varchar(MAX)".to_string());

        let sql_type = SqlType::NVarchar(None, 4000);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "nvarchar(4000)".to_string());

        let sql_type = SqlType::Varchar(None, 4000);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "varchar(4000)".to_string());

        let sql_type = SqlType::VarBinary(None, 4000);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "varbinary(4000)".to_string());

        let sql_type = SqlType::VarBinaryMax(None);
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "varbinary(MAX)".to_string());

        let sql_type = SqlType::Vector(
            None,
            3,
            crate::datatypes::sqldatatypes::VectorBaseType::Float32,
        );
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "vector(3)".to_string());

        let sql_type = SqlType::Vector(
            None,
            3,
            crate::datatypes::sqldatatypes::VectorBaseType::Float16,
        );
        let rpc_param = RpcParameter::get_sql_name(&sql_type);
        assert_eq!(rpc_param, "vector(3, float16)".to_string());
    }
}
