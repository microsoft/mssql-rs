// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use adbc_core::error::Result;
use adbc_core::options::OptionValue;
use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::core::EncryptionSetting;

use crate::error::{invalid, unsupported};

/// TDS datasource (for example `tcp:localhost,1433`).
pub const DATA_SOURCE: &str = "mssql.data_source";
/// Existing database to connect to. The driver never creates databases.
pub const DATABASE: &str = "mssql.database";
pub const ACCESS_TOKEN: &str = "mssql.access_token";
pub const AUTHENTICATION: &str = "mssql.authentication";
pub const ENCRYPT: &str = "mssql.encrypt";
pub const TRUST_CERTIFICATE: &str = "mssql.trust_server_certificate";
pub const CONNECT_TIMEOUT: &str = "mssql.connect_timeout_seconds";
pub const QUERY_TIMEOUT: &str = "mssql.query_timeout_seconds";
pub const BATCH_ROWS: &str = "mssql.batch_rows";
pub const BATCH_BYTES: &str = "mssql.batch_bytes";

pub(crate) fn string(value: OptionValue) -> Result<String> {
    match value {
        OptionValue::String(value) => Ok(value),
        _ => Err(invalid("expected a string option")),
    }
}

pub(crate) fn integer(value: OptionValue, maximum: usize) -> Result<usize> {
    let value = match value {
        OptionValue::Int(value) => value,
        OptionValue::String(value) => value.parse().map_err(|_| invalid("expected an integer"))?,
        _ => return Err(invalid("expected an integer option")),
    };
    usize::try_from(value)
        .ok()
        .filter(|value| *value <= maximum)
        .ok_or_else(|| invalid("integer option is out of range"))
}

#[derive(Clone)]
pub(crate) struct Config {
    pub(crate) context: ClientContext,
    pub(crate) timeout: u32,
    pub(crate) batch_rows: usize,
    pub(crate) batch_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        let mut context = ClientContext::default();
        context.user_agent.set_library_name("MS-ADBC".into());
        context
            .user_agent
            .set_driver_version(env!("CARGO_PKG_VERSION").into());
        Self {
            context,
            timeout: 30,
            batch_rows: 4096,
            batch_bytes: 8 * 1024 * 1024,
        }
    }
}

impl Config {
    pub(crate) fn set(&mut self, key: &str, value: OptionValue) -> Result<()> {
        match key {
            "uri" => {
                let uri = string(value)?;
                let uri = url::Url::parse(&uri).map_err(|_| invalid("invalid SQL Server URI"))?;
                if uri.scheme() != "mssql"
                    || !uri.username().is_empty()
                    || uri.password().is_some()
                    || uri.query().is_some()
                    || uri.fragment().is_some()
                {
                    return Err(invalid(
                        "use mssql://host[:port]/database with separate credential options",
                    ));
                }
                let host = uri
                    .host_str()
                    .ok_or_else(|| invalid("URI requires a host"))?;
                let database =
                    percent_encoding::percent_decode_str(uri.path().trim_start_matches('/'))
                        .decode_utf8()
                        .map_err(|_| invalid("URI database is not UTF-8"))?;
                if database.contains('\0') {
                    return Err(invalid("URI database contains NUL"));
                }
                self.context.data_source = format!("tcp:{host},{}", uri.port().unwrap_or(1433));
                self.context.database = database.into_owned();
            }
            "username" => self.context.user_name = string(value)?,
            "password" => self.context.password = string(value)?,
            DATA_SOURCE => self.context.data_source = string(value)?,
            DATABASE => self.context.database = string(value)?,
            ACCESS_TOKEN => self.context.access_token = Some(string(value)?),
            AUTHENTICATION => {
                self.context.tds_authentication_method = match string(value)?.as_str() {
                    "sql" => TdsAuthenticationMethod::Password,
                    "integrated" => TdsAuthenticationMethod::SSPI,
                    _ => return Err(unsupported("authentication mode")),
                };
            }
            ENCRYPT => {
                self.context.encryption_options.mode = match string(value)?.as_str() {
                    "strict" => EncryptionSetting::Strict,
                    "true" => EncryptionSetting::On,
                    "false" => EncryptionSetting::PreferOff,
                    _ => return Err(invalid("encrypt must be strict, true, or false")),
                };
            }
            TRUST_CERTIFICATE => {
                self.context.encryption_options.trust_server_certificate = bool::try_from(value)?
            }
            CONNECT_TIMEOUT => {
                self.context.connect_timeout = integer(value, u32::MAX as usize)? as u32
            }
            QUERY_TIMEOUT => self.timeout = integer(value, u32::MAX as usize)? as u32,
            BATCH_ROWS => {
                let rows = integer(value, 1_000_000)?;
                if rows == 0 {
                    return Err(invalid("batch_rows must be positive"));
                }
                self.batch_rows = rows;
            }
            BATCH_BYTES => {
                let bytes = integer(value, 1024 * 1024 * 1024)?;
                if bytes == 0 {
                    return Err(invalid("batch_bytes must be positive"));
                }
                self.batch_bytes = bytes;
            }
            _ => return Err(unsupported(format!("option {key}"))),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_keeps_credentials_out_of_connection_strings() {
        let mut config = Config::default();
        config
            .set("uri", "mssql://localhost:1444/drivers".into())
            .unwrap();
        assert_eq!(config.context.data_source, "tcp:localhost,1444");
        assert_eq!(config.context.database, "drivers");
        assert!(
            config
                .set("uri", "mssql://user:secret@localhost/db".into())
                .is_err()
        );
    }

    #[test]
    fn invalid_options_do_not_change_batch_limits() {
        let mut config = Config::default();
        assert!(config.set(BATCH_ROWS, 0_i64.into()).is_err());
        assert!(config.set(BATCH_BYTES, (-1_i64).into()).is_err());
        assert_eq!(config.batch_rows, 4096);
        assert_eq!(config.batch_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn uri_decodes_database_and_preserves_ipv6_brackets() {
        let mut config = Config::default();
        config
            .set("uri", "mssql://[::1]:1434/my%20database".into())
            .unwrap();
        assert_eq!(config.context.data_source, "tcp:[::1],1434");
        assert_eq!(config.context.database, "my database");
        assert!(config.set("uri", "mssql://localhost/%ff".into()).is_err());
    }
}
