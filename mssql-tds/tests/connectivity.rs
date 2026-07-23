// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod connectivity {

    use std::{collections::HashMap, env, sync::Arc};

    use azure_core::cloud::{CloudConfiguration, CustomConfiguration};
    use azure_core::credentials::TokenCredential;
    use azure_core::http::ClientOptions;
    use mssql_tds::connection::client_context::CloneableEntraIdTokenFactory;

    use crate::common::{create_context, get_scalar_value, init_tracing};
    use azure_identity::{
        DeveloperToolsCredential, ManagedIdentityCredential, ManagedIdentityCredentialOptions,
    };
    use dotenv::dotenv;
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::EncryptionOptions;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::{
        connection::client_context::{ClientContext, EntraIdTokenFactory, TdsAuthenticationMethod},
        connection_provider::tds_connection_provider::TdsConnectionProvider,
        core::{EncryptionSetting, TdsResult},
        message::login_options::ApplicationIntent,
    };

    // The scope we want an access token for.
    // For Azure SQL Database, the usual resource is "https://database.windows.net/.default".
    const SCOPE: &str = "https://database.windows.net/.default";

    async fn generate_access_token() -> String {
        let credential = DeveloperToolsCredential::new(None);

        let token_response = credential.unwrap().get_token(&[SCOPE], None).await;

        let secret = token_response.as_ref().unwrap().token.secret();
        print!("{secret}");
        secret.to_string()
    }

    async fn generate_access_token_with_sts_and_resource(
        spn: String,
        sts: String,
        auth_method: &TdsAuthenticationMethod,
    ) -> String {
        let scopes = &[spn.as_ref()];
        // `CustomConfiguration` is `#[non_exhaustive]`, so it must be built by
        // mutating a default; the field-reassign lint does not fire on it.
        let mut custom = CustomConfiguration::default();
        custom.authority_host = sts;
        let client_options = ClientOptions {
            cloud: Some(Arc::new(CloudConfiguration::Custom(custom))),
            ..Default::default()
        };
        let token_response = match auth_method {
            TdsAuthenticationMethod::Password => todo!(),
            TdsAuthenticationMethod::SSPI => todo!(),
            TdsAuthenticationMethod::ActiveDirectoryPassword => todo!(),
            TdsAuthenticationMethod::ActiveDirectoryInteractive => todo!(),
            TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow => todo!(),
            TdsAuthenticationMethod::ActiveDirectoryServicePrincipal => todo!(),
            TdsAuthenticationMethod::ActiveDirectoryManagedIdentity => {
                let options = ManagedIdentityCredentialOptions {
                    client_options,
                    user_assigned_id: None,
                };
                let vm_credential = ManagedIdentityCredential::new(Some(options)).unwrap();
                vm_credential.get_token(scopes, None).await
            }
            TdsAuthenticationMethod::ActiveDirectoryDefault => {
                let credential = DeveloperToolsCredential::new(None);
                credential.unwrap().get_token(scopes, None).await
            }
            TdsAuthenticationMethod::ActiveDirectoryMSI => todo!(),
            TdsAuthenticationMethod::ActiveDirectoryWorkloadIdentity => todo!(),
            TdsAuthenticationMethod::ActiveDirectoryIntegrated => todo!(),
            TdsAuthenticationMethod::AccessToken => todo!(),
        };

        let secret = token_response.as_ref().unwrap().token.secret();
        print!("{secret}");
        secret.to_string()
    }

    pub fn create_context_with_accesstoken(access_token: String) -> ClientContext {
        dotenv().ok();
        println!(
            "This test expects that `az login --tenant E8F4741A-817A-403A-B28F-200D2B07D656` was run to get a token."
        );
        init_tracing();

        let mut context = ClientContext::default();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::On,
            trust_server_certificate: false,
            host_name_in_cert: env::var("CERT_HOST_NAME").ok(),
            server_certificate: None,
        };
        context.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
        context.access_token = Some(access_token);
        context
    }

    pub fn create_context_with_auth_method(auth_method: TdsAuthenticationMethod) -> ClientContext {
        dotenv().ok();
        init_tracing();
        let mut auth_method_map = HashMap::new();

        let factory: Box<dyn CloneableEntraIdTokenFactory> =
            Box::new(DefaultEntraIdTokenFactory {});

        auth_method_map.insert(TdsAuthenticationMethod::ActiveDirectoryDefault, factory);

        let factory: Box<dyn CloneableEntraIdTokenFactory> =
            Box::new(DefaultEntraIdTokenFactory {});

        auth_method_map.insert(
            TdsAuthenticationMethod::ActiveDirectoryManagedIdentity,
            factory.clone_box(),
        );

        let mut context = ClientContext::default();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::On,
            trust_server_certificate: false,
            host_name_in_cert: env::var("CERT_HOST_NAME").ok(),
            server_certificate: None,
        };
        context.tds_authentication_method = auth_method;
        context.auth_method_map = auth_method_map;
        context.connect_timeout = 3600;
        context
    }

    #[tokio::test]
    pub async fn select_1() {
        let access_token = generate_access_token().await;
        let context = create_context_with_accesstoken(access_token);
        let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
        let datasource = format!("tcp:{},1433", host);
        let provider = TdsConnectionProvider {};
        let connection_result = provider.create_client(context, &datasource, None).await;
        let mut connection = connection_result.unwrap();
        let command = "select 1".to_string();
        connection.execute(command, ()).await.unwrap();

        if let Some(resultset) = connection.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.unwrap() {
                for cell in row {
                    print!("{cell:?},");
                }
            }
        }
        connection.close_query().await.unwrap();
    }

    #[tokio::test]
    pub async fn test_authentication_provider() {
        let context =
            create_context_with_auth_method(TdsAuthenticationMethod::ActiveDirectoryDefault);
        let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
        let datasource = format!("tcp:{},1433", host);
        let provider = TdsConnectionProvider {};
        let connection_result = provider.create_client(context, &datasource, None).await;
        let mut connection = connection_result.unwrap();
        let command = "select 1".to_string();
        connection.execute(command, ()).await.unwrap();

        if let Some(resultset) = connection.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.unwrap() {
                for cell in row {
                    print!("{cell:?},");
                }
            }
        }
        connection.close_query().await.unwrap();
    }

    #[derive(Clone)]
    struct DefaultEntraIdTokenFactory {}

    #[async_trait::async_trait]
    impl EntraIdTokenFactory for DefaultEntraIdTokenFactory {
        async fn create_token(
            &self,
            _spn: String,
            _sts_url: String,
            auth_method: TdsAuthenticationMethod,
        ) -> TdsResult<Vec<u8>> {
            let spn = if !_spn.ends_with("/.default") {
                if _spn.ends_with('/') {
                    format!("{_spn}.default")
                } else {
                    format!("{_spn}/.default")
                }
            } else {
                _spn.clone()
            };
            let token =
                generate_access_token_with_sts_and_resource(spn, _sts_url, &auth_method).await;
            let utf16: Vec<u16> = token.encode_utf16().collect();
            let bytes: Vec<u8> = utf16.iter().flat_map(|u| u.to_le_bytes()).collect();
            Ok(bytes)
        }
    }

    #[tokio::test]
    pub async fn trust_server_cert() {
        let access_token = generate_access_token().await;
        let mut context = create_context_with_accesstoken(access_token);
        context.encryption_options.trust_server_certificate = true;
        let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
        let datasource = format!("tcp:{},1433", host);
        let provider = TdsConnectionProvider {};
        let connection_result = provider.create_client(context, &datasource, None).await;
        let mut connection = connection_result.unwrap();
        let command = "select 1".to_string();
        connection.execute(command, ()).await.unwrap();

        if let Some(resultset) = connection.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.unwrap() {
                for cell in row {
                    print!("{cell:?},");
                }
            }
        }
        connection.close_query().await.unwrap();
    }

    #[tokio::test]
    pub async fn validate_host_name() {
        let context = create_context();
        let workstation_id = context.workstation_id.clone();
        let datasource = format!(
            "tcp:{},{}",
            env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string()),
            env::var("DB_PORT").unwrap_or_else(|_| "1433".to_string())
        );
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &datasource, None)
            .await
            .unwrap();
        let command =
            "select host_name from sys.dm_exec_sessions where client_interface_name = 'TdsX'"
                .to_string();
        client.execute(command, ()).await.unwrap();
        let col_hostname = get_scalar_value(&mut client).await.unwrap();
        if let Some(column_value) = col_hostname {
            match column_value {
                ColumnValues::String(value) => {
                    assert_eq!(value.to_utf8_string(), workstation_id);
                }
                _ => unreachable!("Expected a string value"),
            }
        } else {
            unreachable!("Expected a string value");
        }
    }

    #[tokio::test]
    pub async fn validate_app_intent_doesnt_cause_problems() {
        let mut context = create_context();
        context.application_intent = ApplicationIntent::ReadOnly;
        let datasource = format!(
            "tcp:{},{}",
            env::var("DB_HOST").unwrap_or_else(|_| "localhost".to_string()),
            env::var("DB_PORT").unwrap_or_else(|_| "1433".to_string())
        );
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &datasource, None)
            .await
            .unwrap();
        let command = "select 1".to_string();
        client.execute(command, ()).await.unwrap();
        let col_hostname = get_scalar_value(&mut client).await.unwrap();
        if let Some(column_value) = col_hostname {
            match column_value {
                ColumnValues::Int(value) => {
                    assert_eq!(value, 1);
                }
                _ => unreachable!("Expected a int value"),
            }
        } else {
            unreachable!("Expected a int value");
        }
    }

    // ── Session Recovery Integration Tests ────────────────────────
    //
    // These tests require a live SQL Server with DB_HOST, DB_USERNAME,
    // and SQL_PASSWORD env vars set. They are excluded from CI by the
    // `not (test(connectivity))` nextest filter.

    mod session_recovery {
        use crate::common::{build_tcp_datasource, create_context, get_scalar_value, init_tracing};
        use mssql_tds::connection::tds_client::ResultSet;
        use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
        use mssql_tds::datatypes::column_values::ColumnValues;

        #[ctor::ctor]
        fn init() {
            init_tracing();
        }

        /// Helper: get the current SPID (session ID) for a connection.
        async fn get_spid(
            client: &mut mssql_tds::connection::tds_client::TdsClient,
        ) -> Result<i16, Box<dyn std::error::Error>> {
            client.execute("SELECT @@SPID".to_string(), ()).await?;
            let value = get_scalar_value(client).await?;
            match value {
                Some(ColumnValues::SmallInt(spid)) => Ok(spid),
                other => Err(format!("Expected SmallInt for @@SPID, got {:?}", other).into()),
            }
        }

        /// Helper: execute a query and drain all results.
        async fn exec_and_drain(
            client: &mut mssql_tds::connection::tds_client::TdsClient,
            query: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            client.execute(query.to_string(), ()).await?;
            while client.next_row().await?.is_some() {}
            client.close_query().await?;
            Ok(())
        }

        /// Helper: execute a query and return a single string scalar.
        async fn get_string_scalar(
            client: &mut mssql_tds::connection::tds_client::TdsClient,
            query: &str,
        ) -> Result<String, Box<dyn std::error::Error>> {
            client.execute(query.to_string(), ()).await?;
            let value = get_scalar_value(client).await?;
            match value {
                Some(ColumnValues::String(s)) => Ok(s.to_string()),
                other => Err(format!("Expected String, got {:?}", other).into()),
            }
        }

        // ── Feature Negotiation ───────────────────────────────────────

        /// Verify that session recovery is negotiated when connect_retry_count > 0
        /// (the default). The server must acknowledge feature 0x01 in FEATUREEXTACK.
        #[tokio::test]
        async fn feature_negotiation_enabled_by_default() -> Result<(), Box<dyn std::error::Error>>
        {
            let context = create_context();
            // connect_retry_count defaults to 1, which enables session recovery
            assert!(context.connect_retry_count > 0);

            let provider = TdsConnectionProvider {};
            let client = provider
                .create_client(context, &build_tcp_datasource(), None)
                .await?;

            assert!(
                client.is_session_recovery_enabled(),
                "Server should acknowledge session recovery feature"
            );
            assert_eq!(client.connection_recovery_count(), 0);
            Ok(())
        }

        /// Verify that session recovery is NOT negotiated when connect_retry_count == 0.
        #[tokio::test]
        async fn feature_negotiation_disabled_when_retry_zero()
        -> Result<(), Box<dyn std::error::Error>> {
            let mut context = create_context();
            context.connect_retry_count = 0;

            let provider = TdsConnectionProvider {};
            let client = provider
                .create_client(context, &build_tcp_datasource(), None)
                .await?;

            assert!(
                !client.is_session_recovery_enabled(),
                "Session recovery should not be negotiated when connect_retry_count == 0"
            );
            Ok(())
        }

        // ── Session State Accumulation ────────────────────────────────

        /// Verify that after USE [database], the connection still works (session
        /// state is tracked internally).
        #[tokio::test]
        async fn session_state_tracked_after_use_database() -> Result<(), Box<dyn std::error::Error>>
        {
            let provider = TdsConnectionProvider {};
            let mut client = provider
                .create_client(create_context(), &build_tcp_datasource(), None)
                .await?;

            // Switch database — generates ENVCHANGE + potentially SESSIONSTATE tokens
            exec_and_drain(&mut client, "USE [tempdb]").await?;

            let db = get_string_scalar(&mut client, "SELECT DB_NAME()").await?;
            assert_eq!(db, "tempdb");

            exec_and_drain(&mut client, "USE [master]").await?;
            let db = get_string_scalar(&mut client, "SELECT DB_NAME()").await?;
            assert_eq!(db, "master");

            Ok(())
        }

        // ── Reconnection After KILL ─────────────────────────────

        /// Kill the connection's SPID from a second connection, then verify that
        /// the next command transparently reconnects and succeeds.
        #[tokio::test]
        async fn transparent_reconnect_after_kill() -> Result<(), Box<dyn std::error::Error>> {
            let provider = TdsConnectionProvider {};
            let mut client = provider
                .create_client(create_context(), &build_tcp_datasource(), None)
                .await?;

            assert!(client.is_session_recovery_enabled());

            let original_spid = get_spid(&mut client).await?;

            let mut killer = provider
                .create_client(create_context(), &build_tcp_datasource(), None)
                .await?;
            exec_and_drain(&mut killer, &format!("KILL {}", original_spid)).await?;

            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let new_spid = get_spid(&mut client).await?;

            assert_ne!(
                original_spid, new_spid,
                "SPID should change after reconnection (was {}, now {})",
                original_spid, new_spid
            );
            assert_eq!(
                client.connection_recovery_count(),
                1,
                "Recovery count should be 1 after one reconnection"
            );

            Ok(())
        }

        /// Verify that session state (database context) is restored after reconnection.
        #[tokio::test]
        async fn session_state_restored_after_reconnect() -> Result<(), Box<dyn std::error::Error>>
        {
            let provider = TdsConnectionProvider {};
            let mut client = provider
                .create_client(create_context(), &build_tcp_datasource(), None)
                .await?;

            exec_and_drain(&mut client, "USE [tempdb]").await?;
            let db_before = get_string_scalar(&mut client, "SELECT DB_NAME()").await?;
            assert_eq!(db_before, "tempdb");

            let original_spid = get_spid(&mut client).await?;
            let mut killer = provider
                .create_client(create_context(), &build_tcp_datasource(), None)
                .await?;
            exec_and_drain(&mut killer, &format!("KILL {}", original_spid)).await?;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let db_after = get_string_scalar(&mut client, "SELECT DB_NAME()").await?;
            assert_eq!(
                db_after, "tempdb",
                "Database context should be restored to tempdb after reconnection"
            );
            assert_eq!(client.connection_recovery_count(), 1);

            Ok(())
        }

        /// Verify that multiple successive recoveries work and the count increments.
        #[tokio::test]
        async fn multiple_recoveries_increment_count() -> Result<(), Box<dyn std::error::Error>> {
            let provider = TdsConnectionProvider {};
            let mut client = provider
                .create_client(create_context(), &build_tcp_datasource(), None)
                .await?;

            for expected_count in 1..=2u32 {
                let spid = get_spid(&mut client).await?;

                let mut killer = provider
                    .create_client(create_context(), &build_tcp_datasource(), None)
                    .await?;
                exec_and_drain(&mut killer, &format!("KILL {}", spid)).await?;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                exec_and_drain(&mut client, "SELECT 1").await?;
                assert_eq!(client.connection_recovery_count(), expected_count);
            }

            Ok(())
        }

        // ── Recovery Blocked by Transaction ───────────────────────────

        /// When a transaction is active and the connection is killed, recovery
        /// should fail because is_recovery_possible() returns false.
        #[tokio::test]
        async fn recovery_blocked_during_transaction() -> Result<(), Box<dyn std::error::Error>> {
            use mssql_tds::message::transaction_management::TransactionIsolationLevel;

            let provider = TdsConnectionProvider {};
            let mut client = provider
                .create_client(create_context(), &build_tcp_datasource(), None)
                .await?;

            client
                .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
                .await?;

            let spid = get_spid(&mut client).await?;

            let mut killer = provider
                .create_client(create_context(), &build_tcp_datasource(), None)
                .await?;
            exec_and_drain(&mut killer, &format!("KILL {}", spid)).await?;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let result = client.execute("SELECT 1".to_string(), ()).await;
            assert!(
                result.is_err(),
                "Should fail when connection is dead and transaction is active"
            );

            Ok(())
        }

        // ── Connection Open Retry ────────────────────────────────────

        /// Verify that connection to an unreachable host eventually fails with
        /// a timeout error (not instant), confirming retry logic runs.
        #[tokio::test]
        async fn connection_open_retry_with_unreachable_host()
        -> Result<(), Box<dyn std::error::Error>> {
            let mut context = create_context();
            context.connect_retry_count = 1;
            context.connect_retry_interval = 1;
            context.connect_timeout = 5;

            let provider = TdsConnectionProvider {};
            let start = std::time::Instant::now();
            let result = provider
                .create_client(context, "tcp:192.0.2.1,1433", None)
                .await;
            let elapsed = start.elapsed();

            assert!(
                result.is_err(),
                "Connection to unreachable host should fail"
            );
            assert!(
                elapsed.as_secs() >= 1,
                "Should have retried at least once (elapsed: {:?})",
                elapsed
            );

            Ok(())
        }
    }
}
