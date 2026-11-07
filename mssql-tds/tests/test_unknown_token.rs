// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A token the client has no parser for, received mid-result, against the mock
//! TDS server. TDS gives no reliable way to find such a token's end, so the
//! read must fail and the connection must not be reused on a stream whose
//! position is lost.

#[cfg(test)]
mod unknown_token_tests {
    use mssql_mock_tds::{
        ColumnDefinition, ColumnValue, MockTdsServer, QueryResponse, Row, SqlDataType,
    };
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{BatchErrorMode, ExecuteOptions, ResultSet, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
    use mssql_tds::error::Error;
    use tokio::sync::oneshot;

    const QUERY: &str = "SELECT V FROM T";

    fn generate_test_password() -> String {
        use rand::Rng;
        const CHARSET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*";
        let mut rng = rand::rng();
        (0..24)
            .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
            .collect()
    }

    /// Serves `QUERY` with `token` placed right after its COLMETADATA.
    async fn start(
        token: Vec<u8>,
        session_recovery: bool,
    ) -> (
        String,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<std::io::Result<()>>,
    ) {
        let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr();
        {
            let registry = server.query_registry();
            let mut registry = registry.lock().await;
            registry.set_session_recovery(session_recovery);
            // The classification slot sends its bytes verbatim after COLMETADATA.
            registry.register_with_data_classification(
                QUERY,
                QueryResponse::new(
                    vec![ColumnDefinition::new("v", SqlDataType::Int)],
                    vec![Row::new(vec![ColumnValue::Int(7)])],
                ),
                token,
            );
        }
        let (shutdown, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });
        (
            format!("tcp:{},{}", addr.ip(), addr.port()),
            shutdown,
            handle,
        )
    }

    async fn connect(datasource: &str) -> TdsClient {
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
        TdsConnectionProvider {}
            .create_client(context, datasource, None)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_token_without_a_parser_fails_closed() {
        // A DONE (`FD`, status 0, command 0xC1, count 0) that ends the stream
        // as far as a drain can tell. As the payload of a token the client
        // cannot read past, it would let `close_query` report success over a
        // desynchronized stream.
        const FINAL_DONE: [u8; 13] = [0xFD, 0, 0, 0xC1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let unknown_aliasing_done = [&[0xA6][..], &FINAL_DONE].concat();
        let unparsed_aliasing_done = [&[0x88][..], &FINAL_DONE].concat();
        let cases = [
            (
                vec![0xA6, 0x02, 0x00, 0xAA, 0xBB],
                "Unknown token type: 0xA6",
            ),
            (
                vec![0x88, 0x00, 0x00],
                "No parser implemented for token type: AltMetadata",
            ),
            (unknown_aliasing_done, "Unknown token type: 0xA6"),
            (
                unparsed_aliasing_done,
                "No parser implemented for token type: AltMetadata",
            ),
        ];
        for (token, expected) in cases {
            for mode in [BatchErrorMode::Abort, BatchErrorMode::Continue] {
                for session_recovery in [false, true] {
                    let (datasource, shutdown, handle) =
                        start(token.clone(), session_recovery).await;
                    let mut client = connect(&datasource).await;
                    assert_eq!(client.is_session_recovery_enabled(), session_recovery);
                    let case = format!("{token:02X?} {mode:?} recovery={session_recovery}");

                    client
                        .execute(QUERY.to_string(), ExecuteOptions::new().on_error(mode))
                        .await
                        .unwrap();
                    let error = client.next_row().await.unwrap_err();
                    assert!(
                        matches!(&error, Error::ProtocolError(message) if message.contains(expected)),
                        "{case}: {error}"
                    );

                    // The failing read itself retires the connection: nothing
                    // after the token can be told apart from tokens, so it is
                    // neither drained, reused, nor recovered.
                    assert!(client.is_connection_dead(), "{case}");
                    assert!(!client.is_session_recovery_enabled(), "{case}");
                    client.close_query().await.unwrap();
                    let error = client
                        .execute("SELECT 1".to_string(), ())
                        .await
                        .unwrap_err();
                    assert!(
                        matches!(error, Error::ConnectionClosed(_)),
                        "{case}: {error}"
                    );
                    assert_eq!(client.connection_recovery_count(), 0, "{case}");

                    let _ = shutdown.send(());
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
                }
            }
        }
    }
}
