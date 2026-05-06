// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(windows)]

//! Local integration test for SSRP via SQL Browser with integrated security.
//!
//! Windows-only and gated on the `SSPI_TEST` environment variable. SSPI
//! integrated authentication is only available on Windows, and there is no
//! SQL Server available on Windows ARM, so SSPI tests are skipped unless
//! `SSPI_TEST=1` is explicitly set (matching `test_windows_sspi.rs`).
//!
//! Requires SQL Browser running and a named instance available on localhost.
//! The instance name is taken from the `SSPI_NAMED_INSTANCE` environment
//! variable (default: `sqldev`), matching the convention used by the other
//! SSRP/SSPI tests and the CI provisioning in
//! `.pipeline/templates/sql-setup-template.yml`.

#[cfg(test)]
mod common;

#[cfg(test)]
mod ssrp_local {
    use std::env;

    use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting, TdsResult};
    use mssql_tds::datatypes::column_values::ColumnValues;

    use crate::common::init_tracing;

    fn named_instance() -> String {
        env::var("SSPI_NAMED_INSTANCE").unwrap_or_else(|_| "sqldev".to_string())
    }

    async fn connect_ssrp_integrated(datasource: &str) -> TdsResult<TdsClient> {
        let mut ctx = ClientContext::default();
        ctx.tds_authentication_method = TdsAuthenticationMethod::SSPI;
        ctx.database = "master".to_string();
        ctx.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::On,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        provider.create_client(ctx, datasource, None).await
    }

    #[tokio::test]
    async fn test_ssrp_named_instance_integrated_auth() -> TdsResult<()> {
        if env::var("SSPI_TEST").is_err() {
            return Ok(());
        }

        init_tracing();

        let instance = named_instance();
        let datasource = format!(r"localhost\{instance}");
        let mut client = connect_ssrp_integrated(&datasource).await?;

        // Validate the connection works and we reached the right instance
        let query = "SELECT @@SERVICENAME AS instance_name";
        client.execute(query.to_string(), None, None).await?;

        let mut instance_name = String::new();
        loop {
            if let Some(rs) = client.get_current_resultset()
                && let Some(row) = rs.next_row().await?
                && let ColumnValues::String(s) = &row[0]
            {
                instance_name = s.to_utf8_string();
            }
            if !client.move_to_next().await? {
                break;
            }
        }
        client.close_query().await?;

        assert!(
            instance_name.eq_ignore_ascii_case(&instance),
            "Expected {instance} instance, got: {instance_name}"
        );
        println!("Successfully connected to instance: {instance_name}");

        Ok(())
    }

    #[tokio::test]
    async fn test_ssrp_named_instance_hostname_integrated_auth() -> TdsResult<()> {
        if env::var("SSPI_TEST").is_err() {
            return Ok(());
        }

        init_tracing();

        let hostname = hostname::get()
            .expect("failed to get hostname")
            .to_string_lossy()
            .to_string();
        let instance = named_instance();
        let datasource = format!("{hostname}\\{instance}");
        let mut client = connect_ssrp_integrated(&datasource).await?;

        let query = "SELECT @@SERVICENAME AS instance_name";
        client.execute(query.to_string(), None, None).await?;

        let mut instance_name = String::new();
        loop {
            if let Some(rs) = client.get_current_resultset()
                && let Some(row) = rs.next_row().await?
                && let ColumnValues::String(s) = &row[0]
            {
                instance_name = s.to_utf8_string();
            }
            if !client.move_to_next().await? {
                break;
            }
        }
        client.close_query().await?;

        assert!(
            instance_name.eq_ignore_ascii_case(&instance),
            "Expected {instance} instance, got: {instance_name}"
        );
        println!("Successfully connected via {hostname} to instance: {instance_name}");

        Ok(())
    }
}
