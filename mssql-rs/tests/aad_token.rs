// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! AAD/Entra ID federated auth integration test (issue #17).
//!
//! Gated on `ACCESS_TOKEN` (a JWT for `https://database.windows.net/.default`,
//! matching the `mssql-py-core` access-token tests) plus the shared live-test
//! `DB_HOST`, `DB_PORT`, and `DB_DATABASE` pointing at an AAD-enabled SQL
//! Server (Azure SQL DB or Managed Instance).

pub mod common;

#[path = "common/dotenv_test.rs"]
mod dotenv_test;

use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};

fn token_config() -> Option<Config> {
    common::load_env();
    let Ok(token) = std::env::var("ACCESS_TOKEN") else {
        eprintln!("ACCESS_TOKEN not set, skipping");
        return None;
    };
    let host = std::env::var("DB_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("DB_PORT")
        .map(|port| port.parse().expect("DB_PORT must be a valid port"))
        .unwrap_or(1433);

    let mut cfg = Config::new();
    cfg.host(host)
        .port(port)
        .database(common::database())
        .encryption(EncryptionLevel::Required)
        .authentication(AuthMethod::aad_token(token));
    common::apply_certificate_settings(&mut cfg);
    Some(cfg)
}

#[test]
fn token_config_loads_dotenv_without_sql_auth() {
    if dotenv_test::in_child() {
        assert!(std::env::var_os("SQL_PASSWORD").is_none());
        let cfg = token_config().expect("token from .env");
        assert_eq!(cfg.get_addr(), "dotenv.invalid:4321");
        let ctx = cfg.to_client_context();
        assert_eq!(ctx.database, "dotenv_database");
        let token = std::env::var("ACCESS_TOKEN").expect("token from .env");
        assert_eq!(ctx.access_token.as_deref(), Some(token.as_str()));
        assert!(ctx.encryption_options.trust_server_certificate);
    } else {
        let token = uuid::Uuid::new_v4();
        dotenv_test::run(
            "token_config_loads_dotenv_without_sql_auth",
            &format!(
                "ACCESS_TOKEN={token}\nDB_HOST=dotenv.invalid\nDB_PORT=4321\n\
                 DB_DATABASE=dotenv_database\nTRUST_SERVER_CERTIFICATE=true\n"
            ),
            &[],
        );
    }
}

#[test]
fn token_config_uses_exported_token_with_dotenv_endpoint() {
    if dotenv_test::in_child() {
        let cfg = token_config().expect("exported token");
        assert_eq!(cfg.get_addr(), "dotenv.invalid:4321");
        assert_eq!(cfg.to_client_context().database, "dotenv_database");
    } else {
        let token = uuid::Uuid::new_v4().to_string();
        dotenv_test::run(
            "token_config_uses_exported_token_with_dotenv_endpoint",
            "DB_HOST=dotenv.invalid\nDB_PORT=4321\nDB_DATABASE=dotenv_database\n",
            &[("ACCESS_TOKEN", &token)],
        );
    }
}

#[test]
fn token_config_skips_when_token_is_missing() {
    if dotenv_test::in_child() {
        assert!(token_config().is_none());
    } else {
        dotenv_test::run(
            "token_config_skips_when_token_is_missing",
            "DB_HOST=dotenv.invalid\n",
            &[],
        );
    }
}

#[tokio::test]
async fn aad_token_login() {
    let Some(cfg) = token_config() else {
        return;
    };

    let mut client = Client::connect(&cfg).await.expect("AAD login failed");
    let row = client
        .query("SELECT SUSER_SNAME()", &[])
        .await
        .expect("query succeeds: SELECT SUSER_SNAME()")
        .into_first_result();
    let sname = row
        .first()
        .expect("expected row at index 0")
        .get::<&str, _>(0usize);
    assert!(sname.is_some(), "SUSER_SNAME() returned NULL");
    eprintln!("AAD login as: {sname:?}");
}
