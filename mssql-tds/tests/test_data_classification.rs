// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data Classification end to end against the mock TDS server: the client
//! requests feature `0x09` in LOGIN7, the server acknowledges it, and result sets
//! carrying a DATACLASSIFICATION (`0xA3`) token read normally.

#[cfg(test)]
mod data_classification_tests {
    use mssql_mock_tds::{
        ColumnDefinition, ColumnValue, MockTdsServer, QueryResponse, Row, SqlDataType,
    };
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
    use mssql_tds::datatypes::column_values::ColumnValues;
    use tokio::sync::oneshot;

    // Upper case so the mock's RPC lookup, which matches the registered
    // (upper-cased) text byte for byte, finds it too.
    const QUERY: &str = "SELECT SSN FROM PEOPLE";

    fn generate_test_password() -> String {
        use rand::Rng;
        const CHARSET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*";
        let mut rng = rand::rng();
        (0..24)
            .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
            .collect()
    }

    fn b_varchar(bytes: &mut Vec<u8>, value: &str) {
        let units: Vec<u16> = value.encode_utf16().collect();
        bytes.push(u8::try_from(units.len()).unwrap());
        units
            .iter()
            .for_each(|unit| bytes.extend_from_slice(&unit.to_le_bytes()));
    }

    /// One label, one information type, and one column classified by both;
    /// version 2 adds the result-set and property ranks.
    fn classification_token(version: u8) -> Vec<u8> {
        let rank = (version == 2).then_some(30i32);
        let mut bytes = vec![0xA3];
        bytes.extend_from_slice(&1u16.to_le_bytes());
        b_varchar(&mut bytes, "Confidential");
        b_varchar(&mut bytes, "L1");
        bytes.extend_from_slice(&1u16.to_le_bytes());
        b_varchar(&mut bytes, "National ID");
        b_varchar(&mut bytes, "I1");
        if let Some(rank) = rank {
            bytes.extend_from_slice(&rank.to_le_bytes());
        }
        bytes.extend_from_slice(&1u16.to_le_bytes()); // columns
        bytes.extend_from_slice(&1u16.to_le_bytes()); // properties
        bytes.extend_from_slice(&0u16.to_le_bytes()); // label index
        bytes.extend_from_slice(&0u16.to_le_bytes()); // information-type index
        if let Some(rank) = rank {
            bytes.extend_from_slice(&rank.to_le_bytes());
        }
        bytes
    }

    struct Server {
        datasource: String,
        shutdown: oneshot::Sender<()>,
        handle: tokio::task::JoinHandle<std::io::Result<()>>,
    }

    impl Server {
        async fn start(token: Vec<u8>) -> Self {
            let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
            let addr = server.local_addr();
            server
                .query_registry()
                .lock()
                .await
                .register_with_data_classification(
                    QUERY,
                    QueryResponse::new(
                        vec![ColumnDefinition::new("ssn", SqlDataType::Int)],
                        vec![Row::new(vec![ColumnValue::Int(7)])],
                    ),
                    token,
                );
            let (shutdown, shutdown_rx) = oneshot::channel();
            let handle = tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });
            Self {
                datasource: format!("tcp:{},{}", addr.ip(), addr.port()),
                shutdown,
                handle,
            }
        }

        async fn connect(&self, requested_version: Option<u8>) -> TdsClient {
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
            if let Some(version) = requested_version {
                context
                    .request_data_classification_for_test(version)
                    .unwrap();
            }
            TdsConnectionProvider {}
                .create_client(context, &self.datasource, None)
                .await
                .unwrap()
        }

        async fn stop(self) {
            let _ = self.shutdown.send(());
            let _ = tokio::time::timeout(tokio::time::Duration::from_secs(2), self.handle).await;
        }
    }

    async fn assert_single_row(client: &mut TdsClient) {
        assert!(client.on_rows());
        let row = client.next_row().await.unwrap().expect("one row");
        assert_eq!(row[0], ColumnValues::Int(7));
        assert!(client.next_row().await.unwrap().is_none());
        client.close_query().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn negotiated_version_parses_classified_batch_results() {
        for version in [1, 2] {
            let server = Server::start(classification_token(version)).await;
            let mut client = server.connect(Some(version)).await;
            assert_eq!(
                client.negotiated_data_classification_version_for_test(),
                Some(version)
            );

            client.execute(QUERY.to_string(), ()).await.unwrap();
            assert_single_row(&mut client).await;

            // The token was consumed exactly, so the connection is reusable.
            client.execute(QUERY.to_string(), ()).await.unwrap();
            assert_single_row(&mut client).await;

            client.close_connection().await.unwrap();
            server.stop().await;
        }
    }

    /// An unrequested client fails only if the token really arrives, which a
    /// negotiated client reading the row cannot show on its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn classified_rpc_results_are_delivered_and_parsed() {
        let server = Server::start(classification_token(2)).await;

        let mut unrequested = server.connect(None).await;
        let outcome = match unrequested
            .execute_sp_executesql(QUERY.to_string(), Vec::new(), ())
            .await
        {
            Ok(_) => unrequested.next_row().await.map(|_| ()),
            Err(error) => Err(error),
        };
        let error = outcome.unwrap_err();
        assert!(error.to_string().contains("not negotiated"), "{error}");

        let mut client = server.connect(Some(2)).await;
        client
            .execute_sp_executesql(QUERY.to_string(), Vec::new(), ())
            .await
            .unwrap();
        assert_single_row(&mut client).await;

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unrequested_feature_rejects_a_classified_result_set() {
        let server = Server::start(classification_token(2)).await;
        let mut client = server.connect(None).await;
        assert_eq!(
            client.negotiated_data_classification_version_for_test(),
            None
        );

        let outcome = match client.execute(QUERY.to_string(), ()).await {
            Ok(_) => client.next_row().await.map(|_| ()),
            Err(error) => Err(error),
        };
        let error = outcome.unwrap_err();
        assert!(error.to_string().contains("not negotiated"), "{error}");

        server.stop().await;
    }
}
