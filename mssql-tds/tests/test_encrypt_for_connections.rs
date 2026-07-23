// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for all encryption settings.
//!
//! These tests verify that connections work correctly with all supported
//! encryption modes: PreferOff, On, Required, and Strict.

#[cfg(test)]
mod common;

#[cfg(test)]
mod encryption_tests {
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting, TdsResult};

    use crate::common::{build_tcp_datasource_explicit, init_tracing, trust_server_certificate};
    use std::env;

    /// Create a client context with the specified encryption setting
    fn create_context_with_encryption(encryption_setting: EncryptionSetting) -> ClientContext {
        dotenv::dotenv().ok();
        let mut context = ClientContext::default();
        context.user_name =
            env::var("DB_USERNAME").expect("DB_USERNAME environment variable not set");
        context.password = env::var("SQL_PASSWORD")
            .or_else(|_| {
                std::fs::read_to_string("/tmp/password")
                    .map(|s| s.trim().to_string())
                    .map_err(|_| std::env::VarError::NotPresent)
            })
            .expect(
                "SQL_PASSWORD environment variable not set and /tmp/password could not be read",
            );
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: encryption_setting,
            trust_server_certificate: trust_server_certificate(),
            host_name_in_cert: env::var("CERT_HOST_NAME").ok(),
            server_certificate: None,
        };
        context
    }

    /// Create a client with the specified encryption setting
    async fn create_client_with_encryption(
        datasource: &str,
        encryption_setting: EncryptionSetting,
    ) -> TdsResult<TdsClient> {
        let context = create_context_with_encryption(encryption_setting);
        let provider = TdsConnectionProvider {};
        provider.create_client(context, datasource, None).await
    }

    /// Execute SELECT 1 and verify we get a result
    async fn test_simple_query(client: &mut TdsClient) -> TdsResult<()> {
        client.execute("SELECT 1 AS value".to_string(), ()).await?;

        let mut has_result = false;
        loop {
            if let Some(resultset) = client.get_current_resultset() {
                while let Some(_row) = resultset.next_row().await? {
                    has_result = true;
                }
            }
            if !client.advance_to_rows().await? {
                break;
            }
        }
        client.close_query().await?;
        assert!(has_result, "Query should return a result");
        Ok(())
    }

    /// Test connection with EncryptionSetting::PreferOff
    #[tokio::test]
    async fn test_encryption_prefer_off() -> TdsResult<()> {
        init_tracing();
        let datasource = build_tcp_datasource_explicit();

        let mut client =
            create_client_with_encryption(&datasource, EncryptionSetting::PreferOff).await?;
        test_simple_query(&mut client).await?;
        client.close().await?;
        Ok(())
    }

    /// Test connection with EncryptionSetting::On
    #[tokio::test]
    async fn test_encryption_on() -> TdsResult<()> {
        init_tracing();
        let datasource = build_tcp_datasource_explicit();

        let mut client = create_client_with_encryption(&datasource, EncryptionSetting::On).await?;
        test_simple_query(&mut client).await?;
        client.close().await?;
        Ok(())
    }

    /// Test connection with EncryptionSetting::Required
    #[tokio::test]
    async fn test_encryption_required() -> TdsResult<()> {
        init_tracing();
        let datasource = build_tcp_datasource_explicit();

        let mut client =
            create_client_with_encryption(&datasource, EncryptionSetting::Required).await?;
        test_simple_query(&mut client).await?;
        client.close().await?;
        Ok(())
    }

    /// Test connection with EncryptionSetting::Strict (TDS 8.0)
    /// Note: Strict encryption enforces certificate validation even when TrustServerCertificate=true.
    /// This test is skipped when TRUST_SERVER_CERTIFICATE=true because proper CA certificates are required.
    #[tokio::test]
    async fn test_encryption_strict() -> TdsResult<()> {
        init_tracing();

        // Skip this test if TRUST_SERVER_CERTIFICATE=true because Strict encryption
        // enforces certificate validation - TrustServerCertificate is ignored for Strict mode.
        if trust_server_certificate() {
            println!(
                "Skipping test_encryption_strict: TRUST_SERVER_CERTIFICATE=true is incompatible with Strict encryption mode"
            );
            return Ok(());
        }

        let datasource = build_tcp_datasource_explicit();

        let mut client =
            create_client_with_encryption(&datasource, EncryptionSetting::Strict).await?;
        test_simple_query(&mut client).await?;
        client.close().await?;
        Ok(())
    }
}
