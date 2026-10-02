// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum ConnectionStringError {
    MissingDataSource,
    InvalidProperty(String),
    InvalidValue,
    UnsupportedProperty(String),
}

impl fmt::Display for ConnectionStringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDataSource => f.write_str("Data Source is required"),
            Self::InvalidProperty(property) => write!(f, "invalid connection property: {property}"),
            Self::InvalidValue => f.write_str("invalid connection string value"),
            Self::UnsupportedProperty(property) => {
                write!(f, "unsupported connection property: {property}")
            }
        }
    }
}

impl std::error::Error for ConnectionStringError {}

#[derive(Debug, PartialEq)]
pub struct ConnectionOptions {
    pub data_source: String,
    pub initial_catalog: String,
    pub user_id: String,
    pub password: String,
    pub encrypt: EncryptionSetting,
    pub trust_server_certificate: bool,
}

impl ConnectionOptions {
    pub fn parse(connection_string: &str) -> Result<Self, ConnectionStringError> {
        let mut options = Self {
            data_source: String::new(),
            initial_catalog: String::new(),
            user_id: String::new(),
            password: String::new(),
            encrypt: EncryptionSetting::Strict,
            trust_server_certificate: false,
        };

        for property in split_properties(connection_string)? {
            if property.trim().is_empty() {
                continue;
            }

            let (key, raw_value) = property
                .split_once('=')
                .ok_or_else(|| ConnectionStringError::InvalidProperty(property.to_owned()))?;
            let key = key.trim();
            let value = unquote(raw_value.trim())?;

            match key.to_ascii_lowercase().as_str() {
                "provider" => {}
                "data source" | "server" | "address" | "addr" | "network address" => {
                    options.data_source = value
                }
                "initial catalog" | "database" => options.initial_catalog = value,
                "user id" | "uid" => options.user_id = value,
                "password" | "pwd" => options.password = value,
                "encrypt" => {
                    options.encrypt = match value.to_ascii_lowercase().as_str() {
                        "true" | "yes" | "mandatory" => EncryptionSetting::Required,
                        "false" | "no" | "optional" => EncryptionSetting::PreferOff,
                        "strict" => EncryptionSetting::Strict,
                        _ => return Err(ConnectionStringError::InvalidValue),
                    }
                }
                "trustservercertificate" | "trust server certificate" => {
                    options.trust_server_certificate = parse_bool(&value)?;
                }
                _ => {
                    return Err(ConnectionStringError::UnsupportedProperty(key.to_owned()));
                }
            }
        }

        if options.data_source.is_empty() {
            return Err(ConnectionStringError::MissingDataSource);
        }

        Ok(options)
    }

    pub(crate) fn encryption_options(&self) -> EncryptionOptions {
        EncryptionOptions {
            mode: self.encrypt,
            trust_server_certificate: self.trust_server_certificate,
            host_name_in_cert: None,
            server_certificate: None,
        }
    }
}

fn parse_bool(value: &str) -> Result<bool, ConnectionStringError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(ConnectionStringError::InvalidValue),
    }
}

fn split_properties(connection_string: &str) -> Result<Vec<&str>, ConnectionStringError> {
    let bytes = connection_string.as_bytes();
    let mut properties = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut quote = None;

    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(end) if byte == end && bytes.get(index + 1) == Some(&end) => {
                index += 1;
            }
            Some(end) if byte == end => quote = None,
            Some(_) => {}
            None if matches!(byte, b'\'' | b'"' | b'{') => {
                quote = Some(if byte == b'{' { b'}' } else { byte });
            }
            None if byte == b';' => {
                properties.push(&connection_string[start..index]);
                start = index + 1;
            }
            None => {}
        }
        index += 1;
    }

    if quote.is_some() {
        return Err(ConnectionStringError::InvalidValue);
    }
    properties.push(&connection_string[start..]);
    Ok(properties)
}

fn unquote(value: &str) -> Result<String, ConnectionStringError> {
    let Some(first) = value.as_bytes().first().copied() else {
        return Ok(String::new());
    };
    let end = match first {
        b'\'' => b'\'',
        b'"' => b'"',
        b'{' => b'}',
        _ => return Ok(value.to_owned()),
    };
    if value.as_bytes().last() != Some(&end) || value.len() < 2 {
        return Err(ConnectionStringError::InvalidValue);
    }

    let inner = &value[1..value.len() - 1];
    let escaped_end = [end, end];
    let replacement = [end];
    let mut result = Vec::with_capacity(inner.len());
    let bytes = inner.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == end {
            if bytes.get(index + 1) != Some(&end) {
                return Err(ConnectionStringError::InvalidValue);
            }
            result.extend_from_slice(&replacement);
            index += escaped_end.len();
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(result).map_err(|_| ConnectionStringError::InvalidValue)
}
