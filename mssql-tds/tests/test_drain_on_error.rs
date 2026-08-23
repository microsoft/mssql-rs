// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test reproducing go-mssqldb #410 against the mock TDS server.
//!
//! A statement-scoped error (for example a lock timeout, error 1222) does not
//! abort the batch, so the server keeps streaming a trailing result set after
//! the error. The client must drain that result set — including its ROW tokens,
//! which can only be parsed with the preceding COLMETADATA — so the real SQL
//! error surfaces and the connection stays usable for the next query.
//!
//! Find attributed to go-mssqldb: https://github.com/microsoft/go-mssqldb/pull/410

#[cfg(test)]
mod drain_on_error_tests {
    use mssql_mock_tds::{
        ColumnDefinition, ColumnValue, LeadingError, MockTdsServer, QueryResponse, Row, SqlDataType,
    };
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
    use mssql_tds::datatypes::column_values::ColumnValues;
    use tokio::sync::oneshot;

    fn generate_test_password() -> String {
        use rand::Rng;
        const CHARSET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*";
        let mut rng = rand::rng();
        (0..24)
            .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
            .collect()
    }

    /// The batch `RAISERROR(...); SELECT ...` produces a statement-scoped error
    /// followed by a row set. The client must surface the SQL error and remain
    /// usable: a second query on the *same* connection must still succeed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn statement_error_with_trailing_rowset_is_drained_and_connection_reusable()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockTdsServer::new("127.0.0.1:0").await?;
        let server_addr = server.local_addr();

        let error_query = "RAISERROR('lock timeout', 16, 1); SELECT id FROM t";
        {
            let registry = server.query_registry();
            let mut reg = registry.lock().await;
            reg.register(
                error_query,
                QueryResponse::new(
                    vec![ColumnDefinition::new("id", SqlDataType::Int)],
                    vec![
                        Row::new(vec![ColumnValue::Int(10)]),
                        Row::new(vec![ColumnValue::Int(20)]),
                    ],
                )
                .with_leading_error(LeadingError::new(1222, 16, "lock timeout")),
            );
            reg.register("SELECT 42", {
                QueryResponse::new(
                    vec![ColumnDefinition::new("answer", SqlDataType::Int)],
                    vec![Row::new(vec![ColumnValue::Int(42)])],
                )
            });
        }

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server_handle =
            tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let datasource = format!("tcp:{},{}", server_addr.ip(), server_addr.port());
        let mut context = ClientContext::default();
        context.user_name = "sa".to_string();
        context.password = generate_test_password();
        context.database = "master".to_string();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let mut client = provider.create_client(context, &datasource, None).await?;

        // The statement error must surface — masked neither by a parse failure
        // nor swallowed — and it must carry the real server error number.
        let err = client
            .execute(error_query.to_string(), ())
            .await
            .expect_err("statement-scoped error must surface");
        match err {
            mssql_tds::error::Error::SqlServerError { diagnostics } => {
                assert!(
                    diagnostics.errors.iter().any(|e| e.number == 1222),
                    "expected server error 1222, got: {diagnostics:?}"
                );
            }
            other => panic!("expected a SqlServerError, got: {other:?}"),
        }

        // The trailing result set was fully drained, so the connection is clean:
        // a fresh query on the same connection must succeed and return its row.
        client.execute("SELECT 42".to_string(), ()).await?;
        assert!(
            client.on_rows(),
            "reused connection should return a result set"
        );
        let row = client
            .next_row()
            .await?
            .expect("reused connection should return one row");
        assert_eq!(row[0], ColumnValues::Int(42));
        client.close_query().await?;
        client.close_connection().await?;

        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), server_handle).await;
        Ok(())
    }
}
