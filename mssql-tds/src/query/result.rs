// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::datatypes::column_values::ColumnValues;
use crate::query::metadata::ColumnMetadata;
use crate::token::tokenitems::ReturnValueStatus;
use crate::token::tokens::ReturnValueToken;

/// A value returned from a stored procedure or prepared statement — either an
/// `OUTPUT` parameter or a UDF return value.
///
/// Collected automatically as the TDS token stream is read. Use
/// [`TdsClient::get_return_values()`](crate::connection::tds_client::TdsClient::get_return_values) or
/// [`TdsClient::retrieve_output_params()`](crate::connection::tds_client::TdsClient::retrieve_output_params)
/// to access them after the result set is consumed.
#[derive(Debug, Clone)]
pub struct ReturnValue {
    /// Zero-based ordinal position of the parameter in the RPC call.
    pub param_ordinal: u16,
    /// Parameter name as declared in the stored procedure (may be empty for
    /// positional parameters).
    pub param_name: String,
    /// The returned value, decoded into the appropriate [`ColumnValues`]
    /// variant.
    pub value: ColumnValues,
    /// Type metadata (data type, precision, scale, collation, etc.).
    pub column_metadata: Box<ColumnMetadata>,
    /// Whether this is an `OUTPUT` parameter or a UDF return value.
    pub status: ReturnValueStatus,
}

impl From<ReturnValueToken> for ReturnValue {
    fn from(token: ReturnValueToken) -> Self {
        ReturnValue {
            param_ordinal: token.param_ordinal,
            param_name: token.param_name,
            value: token.value,
            column_metadata: token.column_metadata,
            status: token.status,
        }
    }
}
