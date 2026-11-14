// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Live-test configuration using the repository's shared test environment:
//! `DB_HOST`, `DB_PORT`, `DB_USERNAME`, `SQL_PASSWORD` (or `/tmp/password`),
//! `DB_DATABASE`, `TRUST_SERVER_CERTIFICATE`, and `CERT_HOST_NAME`, optionally
//! loaded from a `.env` file. Live tests skip when no password is available.
//!
//! Test crates declare this as `pub mod common;` so helpers unused by a given
//! test binary are not reported as dead code.

use std::env;

use mssql_rs::{AuthMethod, Config};

pub fn load_env() {
    if let Err(err) = dotenv::dotenv() {
        assert!(err.not_found(), "invalid .env file: {err}");
    }
}

pub fn password() -> Option<String> {
    load_env();
    env::var("SQL_PASSWORD").ok().or_else(|| {
        std::fs::read_to_string("/tmp/password")
            .ok()
            .map(|password| password.trim().to_string())
    })
}

pub fn user_name() -> String {
    env::var("DB_USERNAME").unwrap_or_else(|_| "sa".into())
}

pub fn database() -> String {
    env::var("DB_DATABASE").unwrap_or_else(|_| "master".into())
}

/// Host, port, database, and SQL authentication without TLS trust settings,
/// for tests that control certificate validation themselves.
pub fn base_config() -> Option<Config> {
    let password = password()?;
    let port = env::var("DB_PORT")
        .map(|port| port.parse().expect("DB_PORT must be a valid port"))
        .unwrap_or(1433);
    let mut config = Config::new();
    config
        .host(env::var("DB_HOST").unwrap_or_else(|_| "localhost".into()))
        .port(port)
        .database(database())
        .authentication(AuthMethod::sql_server(user_name(), password));
    Some(config)
}

pub fn trust_server_certificate() -> bool {
    env::var("TRUST_SERVER_CERTIFICATE")
        .map(|value| value.parse().unwrap_or(false))
        .unwrap_or(false)
}

pub fn apply_certificate_settings(config: &mut Config) {
    if trust_server_certificate() {
        config.trust_cert();
    }
    if let Ok(host_name) = env::var("CERT_HOST_NAME") {
        config.host_name_in_certificate(host_name);
    }
}

pub fn live_config() -> Option<Config> {
    let mut config = base_config()?;
    apply_certificate_settings(&mut config);
    Some(config)
}

/// Exits the test process successfully when no password is configured.
pub fn live_config_or_exit() -> Config {
    live_config().unwrap_or_else(|| {
        eprintln!("SQL_PASSWORD not set and /tmp/password unreadable; skipping live test");
        std::process::exit(0);
    })
}
