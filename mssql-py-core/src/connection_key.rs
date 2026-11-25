// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Private SQL-authentication preparation shared by keying and pooled physical opening.

use std::{fmt, sync::OnceLock};

use mssql_tds::{
    connection::client_context::{ClientContext, IPAddressPreference, TdsAuthenticationMethod},
    core::EncryptionSetting,
    message::login_options::ApplicationIntent,
    security::{fill_random, hmac_sha256},
};
use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
    types::{PyBool, PyDict, PyString},
};

use crate::{
    connection::PyCoreConnection,
    pool::connection::{AuthBinding, ConnectSpec, PoolKey},
};

// These are the outputs of Python's existing connection-string adaptation, not
// a second connection-string grammar. Keep unknown fields fail-closed.
const STRING_OPTIONS: &[&str] = &[
    "server",
    "user_name",
    "password",
    "database",
    "application_name",
    "authentication",
    "trusted_connection",
    "encryption",
    "trust_server_certificate",
    "host_name_in_certificate",
    "server_certificate",
    "server_spn",
    "multi_subnet_failover",
    "application_intent",
    "ip_address_preference",
];
const INTEGER_OPTIONS: &[&str] = &[
    "packet_size",
    "connect_retry_count",
    "connect_retry_interval",
    "keep_alive",
    "keep_alive_interval",
    "connect_timeout",
];

static PROCESS_KEY: OnceLock<Result<[u8; 32], &'static str>> = OnceLock::new();

/// Key and physical-open specification derived together from one frozen context.
pub(crate) struct PreparedSqlConnection {
    key: PoolKey,
    spec: ConnectSpec,
}

impl fmt::Debug for PreparedSqlConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedSqlConnection([REDACTED])")
    }
}

impl PreparedSqlConnection {
    pub(crate) fn into_parts(self) -> (PoolKey, ConnectSpec) {
        (self.key, self.spec)
    }
}

fn invalid_options() -> PyErr {
    PyValueError::new_err("Invalid or unsupported SQL-authentication connection options")
}

/// Consumes Python-adapted options, never customer-supplied pool fingerprints.
pub(crate) fn prepare_sql_connection(
    options: &Bound<'_, PyDict>,
) -> PyResult<PreparedSqlConnection> {
    let options = validate_options(options)?;
    let mut context =
        PyCoreConnection::dict_to_client_context(&options).map_err(|_| invalid_options())?;
    if context.tds_authentication_method != TdsAuthenticationMethod::Password {
        return Err(invalid_options());
    }
    context
        .prepare_for_connect()
        .map_err(|_| invalid_options())?;
    let secret = PROCESS_KEY
        .get_or_init(|| {
            let mut secret = [0; 32];
            fill_random(&mut secret)
                .map(|()| secret)
                .map_err(|_| "Cannot initialize the private connection fingerprint key")
        })
        .as_ref()
        .map_err(|message| PyRuntimeError::new_err(*message))?;
    fingerprint(context, secret)
}

fn validate_options<'py>(options: &Bound<'py, PyDict>) -> PyResult<Bound<'py, PyDict>> {
    for (key, value) in options.iter() {
        let key = key.extract::<String>().map_err(|_| invalid_options())?;
        if STRING_OPTIONS.contains(&key.as_str()) {
            if !value.is_instance_of::<PyString>() {
                return Err(invalid_options());
            }
        } else if INTEGER_OPTIONS.contains(&key.as_str()) {
            if value.is_instance_of::<PyBool>() || value.extract::<u32>().is_err() {
                return Err(invalid_options());
            }
        } else {
            return Err(invalid_options());
        }
    }
    let server = options
        .get_item("server")?
        .ok_or_else(invalid_options)?
        .extract::<String>()
        .map_err(|_| invalid_options())?;
    if server.is_empty() {
        return Err(invalid_options());
    }
    for field in STRING_OPTIONS {
        if let Some(value) = options.get_item(*field)? {
            let value = value.extract::<String>().map_err(|_| invalid_options())?;
            if value.contains('\0') {
                return Err(invalid_options());
            }
        }
    }
    for (field, minimum, maximum) in [
        ("connect_retry_count", 0, 255),
        ("connect_retry_interval", 1, 60),
    ] {
        if let Some(value) = options.get_item(field)? {
            let value = value.extract::<u32>()?;
            if !(minimum..=maximum).contains(&value) {
                return Err(invalid_options());
            }
        }
    }

    let normalized = options.copy()?;
    // Rust ODBC treats zero as unspecified and otherwise clamps packet size.
    let packet_size = options
        .get_item("packet_size")?
        .map(|value| value.extract::<u32>())
        .transpose()?
        .filter(|value| *value != 0)
        .map(|value| value.clamp(512, 32768))
        .unwrap_or_else(|| u32::from(ClientContext::default().packet_size));
    normalized.set_item("packet_size", packet_size)?;

    if let Some(value) = options.get_item("ip_address_preference")? {
        let value = value.extract::<String>()?;
        if !["ipv4first", "ipv6first", "useplatformdefault"]
            .iter()
            .any(|accepted| value.eq_ignore_ascii_case(accepted))
        {
            normalized.set_item("ip_address_preference", "IPv4First")?;
        }
    }
    if let Some(value) = options.get_item("encryption")? {
        let value = value.extract::<String>()?;
        if !["yes", "mandatory", "no", "optional", "strict"]
            .iter()
            .any(|accepted| value.eq_ignore_ascii_case(accepted))
        {
            return Err(invalid_options());
        }
    }
    Ok(normalized)
}

fn fingerprint(context: ClientContext, secret: &[u8; 32]) -> PyResult<PreparedSqlConnection> {
    let encryption = match context.encryption_options.mode {
        EncryptionSetting::PreferOff => "optional",
        EncryptionSetting::On | EncryptionSetting::Required => "mandatory",
        EncryptionSetting::Strict => "strict",
    };
    let intent = match context.application_intent {
        ApplicationIntent::ReadOnly => "read-only",
        ApplicationIntent::ReadWrite => "read-write",
    };
    let ip_preference = match context.ipaddress_preference {
        IPAddressPreference::IPv4First => "ipv4-first",
        IPAddressPreference::IPv6First => "ipv6-first",
        IPAddressPreference::UsePlatformDefault => "platform-default",
    };
    let policy = context
        .multi_subnet_failover_policy()
        .ok_or_else(invalid_options)?;
    let certificate = context
        .encryption_options
        .server_certificate
        .as_ref()
        .map(|path| path.to_str().ok_or_else(invalid_options))
        .transpose()?;
    let mut descriptor = serde_json::to_vec(&(
        "mssql-python/async-pool/v1",
        (
            "endpoint",
            &context.data_source,
            &context.database_instance,
            policy.enabled,
            &context.failover_partner,
            context.ssrp_timeout_ms,
        ),
        (
            "login",
            "sql-password",
            &context.user_name,
            &context.password,
            &context.database,
            &context.application_name,
            intent,
            &context.language,
            &context.workstation_id,
            &context.server_spn,
            &context.login_server_name,
        ),
        (
            "tls",
            encryption,
            context.encryption_options.trust_server_certificate,
            &context.encryption_options.host_name_in_cert,
            certificate,
        ),
        (
            "transport",
            context.packet_size,
            ip_preference,
            context.keep_alive_in_ms,
            context.keep_alive_interval_in_ms,
            context.connect_timeout,
            context.login_timeout,
        ),
        (
            "recovery",
            context.connect_retry_count,
            context.connect_retry_interval,
        ),
        (
            "driver",
            &context.library_name,
            context.encode_driver_version(),
            &context.user_agent.library_name,
            &context.user_agent.driver_version,
            &context.user_agent.runtime,
        ),
    ))
    .map_err(|_| invalid_options())?;
    let key = hmac_sha256(secret, &descriptor);
    descriptor.fill(0);
    let key = key.map_err(|_| PyRuntimeError::new_err("Connection fingerprint failed"))?;

    let mut auth_descriptor = serde_json::to_vec(&(
        "mssql-python/sql-auth/v1",
        &context.user_name,
        &context.password,
    ))
    .map_err(|_| invalid_options())?;
    let binding = hmac_sha256(secret, &auth_descriptor);
    auth_descriptor.fill(0);
    let binding =
        binding.map_err(|_| PyRuntimeError::new_err("Authentication fingerprint failed"))?;
    let key = PoolKey::new(Box::new(key))
        .map_err(|_| PyRuntimeError::new_err("Invalid connection fingerprint"))?;
    let binding = AuthBinding::new(Box::new(binding))
        .map_err(|_| PyRuntimeError::new_err("Invalid authentication fingerprint"))?;
    Ok(PreparedSqlConnection {
        key,
        spec: ConnectSpec::new(context, binding),
    })
}

#[cfg(test)]
mod tests;
