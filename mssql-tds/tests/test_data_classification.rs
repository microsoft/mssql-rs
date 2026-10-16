// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data Classification end to end against the mock TDS server: the client
//! requests feature `0x09` in LOGIN7, the server acknowledges it, and each
//! result set exposes the DATACLASSIFICATION (`0xA3`) token sent with its
//! column metadata — and only that one.

#[cfg(test)]
mod data_classification_tests {
    use mssql_mock_tds::{
        ColumnDefinition, ColumnValue, ConnectionStore, DataClassificationAck, MockTdsServer,
        QueryRegistry, QueryResponse, ResultSetResponse, Row, SqlDataType,
    };
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection::tds_client::{
        ExecuteOptions, ResultSet, StatementResult, TdsClient,
    };
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{CancelHandle, EncryptionOptions, EncryptionSetting};
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::error::Error;
    use mssql_tds::token::data_classification::{
        DataClassificationMetadata, DataClassificationVersion, InformationTypeReference,
        SensitivityLabelReference, SensitivityRank,
    };
    use std::sync::Arc;
    use tokio::sync::{Mutex, oneshot};

    // Upper case so the mock's RPC lookup, which matches the registered
    // (upper-cased) text byte for byte, finds them too.
    const CLASSIFIED: &str = "SELECT SSN, PHONE FROM PEOPLE";
    const UNCLASSIFIED: &str = "SELECT ID FROM PEOPLE";
    const OTHER_CLASSIFIED: &str = "SELECT EMAIL FROM PEOPLE";
    const THREE_RESULT_SETS: &str = "SELECT SSN FROM A; SELECT ID FROM B; SELECT EMAIL FROM C";
    const KILL_SESSION: &str = "KILL SESSION";

    const UNSPECIFIED: u16 = 0xFFFF;

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

    /// `(label index, information-type index, rank)`; ranks are written only for v2.
    type Property = (u16, u16, i32);

    fn token(
        version: DataClassificationVersion,
        labels: &[(&str, &str)],
        information_types: &[(&str, &str)],
        rank: i32,
        columns: &[&[Property]],
    ) -> Vec<u8> {
        let v2 = version == DataClassificationVersion::Version2;
        let mut bytes = vec![0xA3];
        for dictionary in [labels, information_types] {
            bytes.extend_from_slice(&u16::try_from(dictionary.len()).unwrap().to_le_bytes());
            for (name, id) in dictionary {
                b_varchar(&mut bytes, name);
                b_varchar(&mut bytes, id);
            }
        }
        if v2 {
            bytes.extend_from_slice(&rank.to_le_bytes());
        }
        bytes.extend_from_slice(&u16::try_from(columns.len()).unwrap().to_le_bytes());
        for properties in columns {
            bytes.extend_from_slice(&u16::try_from(properties.len()).unwrap().to_le_bytes());
            for (label, information_type, rank) in *properties {
                bytes.extend_from_slice(&label.to_le_bytes());
                bytes.extend_from_slice(&information_type.to_le_bytes());
                if v2 {
                    bytes.extend_from_slice(&rank.to_le_bytes());
                }
            }
        }
        bytes
    }

    /// SSN is classified by both dictionaries; PHONE has two properties, each
    /// leaving one dictionary unspecified.
    fn classified_token(version: DataClassificationVersion) -> Vec<u8> {
        token(
            version,
            &[("Confidential", "L1"), ("Public", "L0")],
            &[("National ID", "I1"), ("Contact Info", "I2")],
            40,
            &[&[(0, 0, 30)], &[(1, UNSPECIFIED, 10), (UNSPECIFIED, 1, 0)]],
        )
    }

    fn single_label_token(version: DataClassificationVersion, label: &str) -> Vec<u8> {
        token(
            version,
            &[(label, "L")],
            &[],
            20,
            &[&[(0, UNSPECIFIED, 20)]],
        )
    }

    fn int_response(columns: &[&str], value: i32) -> QueryResponse {
        QueryResponse::new(
            columns
                .iter()
                .map(|name| ColumnDefinition::new(*name, SqlDataType::Int))
                .collect(),
            vec![Row::new(
                columns.iter().map(|_| ColumnValue::Int(value)).collect(),
            )],
        )
    }

    struct Server {
        datasource: String,
        registry: Arc<Mutex<QueryRegistry>>,
        connections: Arc<Mutex<ConnectionStore>>,
        shutdown: oneshot::Sender<()>,
        handle: tokio::task::JoinHandle<std::io::Result<()>>,
    }

    impl Server {
        /// Serves classification bytes laid out for `version`, which must be
        /// the version clients negotiate: the payload carries no version.
        async fn start(version: DataClassificationVersion) -> Self {
            let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
            let addr = server.local_addr();
            let registry = server.query_registry();
            let connections = server.connection_store();
            {
                let mut registry = registry.lock().await;
                registry.register_with_data_classification(
                    CLASSIFIED,
                    int_response(&["ssn", "phone"], 7),
                    classified_token(version),
                );
                registry.register(UNCLASSIFIED, int_response(&["id"], 1));
                registry.register_with_data_classification(
                    OTHER_CLASSIFIED,
                    int_response(&["email"], 3),
                    single_label_token(version, "Secret"),
                );
                registry.register_result_sets(
                    THREE_RESULT_SETS,
                    vec![
                        ResultSetResponse::new(
                            int_response(&["ssn"], 7),
                            single_label_token(version, "First"),
                        ),
                        ResultSetResponse::new(int_response(&["id"], 1), Vec::new()),
                        ResultSetResponse::new(
                            int_response(&["email"], 3),
                            single_label_token(version, "Third"),
                        ),
                    ],
                );
            }
            let (shutdown, shutdown_rx) = oneshot::channel();
            let handle = tokio::spawn(async move { server.run_with_shutdown(shutdown_rx).await });
            Self {
                datasource: format!("tcp:{},{}", addr.ip(), addr.port()),
                registry,
                connections,
                shutdown,
                handle,
            }
        }

        /// Like [`start`](Self::start), but acknowledges session recovery and
        /// closes the connection after [`KILL_SESSION`], so the client's next
        /// command finds it dead and reconnects.
        async fn start_recoverable(version: DataClassificationVersion) -> Self {
            let server = Self::start(version).await;
            {
                let mut registry = server.registry.lock().await;
                registry.set_session_recovery(true);
                registry.register(KILL_SESSION, QueryResponse::select_one());
                registry.close_connection_after(KILL_SESSION);
            }
            server
        }

        /// Data Classification versions requested by the logins that carried
        /// session-recovery state, i.e. the reconnects.
        async fn recovery_login_requests(&self) -> Vec<Option<u8>> {
            self.connections
                .lock()
                .await
                .all()
                .values()
                .filter(|connection| connection.recovery_login)
                .map(|connection| connection.data_classification_requested)
                .collect()
        }

        async fn connect(&self, requested: Option<DataClassificationVersion>) -> TdsClient {
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
            context.requested_data_classification_version = requested;
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

    fn label(client: &TdsClient) -> Option<String> {
        let classification = client.data_classification()?;
        let property = &classification.columns()[0].properties()[0];
        classification
            .resolve_label(property.sensitivity_label())
            .map(|label| label.name().to_string())
    }

    fn assert_classified(
        metadata: &DataClassificationMetadata,
        version: DataClassificationVersion,
    ) {
        let v2 = version == DataClassificationVersion::Version2;
        let rank = |rank| v2.then_some(rank);
        assert_eq!(metadata.version(), version);
        assert_eq!(metadata.rank(), rank(SensitivityRank::Critical));

        let labels: Vec<_> = metadata
            .sensitivity_labels()
            .iter()
            .map(|label| (label.name(), label.id()))
            .collect();
        assert_eq!(labels, [("Confidential", "L1"), ("Public", "L0")]);
        let types: Vec<_> = metadata
            .information_types()
            .iter()
            .map(|information_type| (information_type.name(), information_type.id()))
            .collect();
        assert_eq!(types, [("National ID", "I1"), ("Contact Info", "I2")]);

        let [ssn, phone] = metadata.columns() else {
            panic!("expected two columns, got {:?}", metadata.columns());
        };
        let [ssn] = ssn.properties() else {
            panic!("expected one SSN property, got {ssn:?}");
        };
        assert_eq!(ssn.sensitivity_label(), SensitivityLabelReference::Index(0));
        assert_eq!(ssn.information_type(), InformationTypeReference::Index(0));
        assert_eq!(ssn.rank(), rank(SensitivityRank::High));
        assert_eq!(
            metadata
                .resolve_label(ssn.sensitivity_label())
                .unwrap()
                .name(),
            "Confidential"
        );
        assert_eq!(
            metadata
                .resolve_information_type(ssn.information_type())
                .unwrap()
                .name(),
            "National ID"
        );

        let [by_label, by_type] = phone.properties() else {
            panic!("expected two PHONE properties, got {phone:?}");
        };
        assert_eq!(
            metadata
                .resolve_label(by_label.sensitivity_label())
                .unwrap()
                .id(),
            "L0"
        );
        assert_eq!(
            metadata.resolve_information_type(by_label.information_type()),
            None
        );
        assert_eq!(by_label.rank(), rank(SensitivityRank::Low));
        assert_eq!(metadata.resolve_label(by_type.sensitivity_label()), None);
        assert_eq!(
            metadata
                .resolve_information_type(by_type.information_type())
                .unwrap()
                .id(),
            "I2"
        );
        assert_eq!(by_type.rank(), rank(SensitivityRank::None));
    }

    async fn assert_classified_result(client: &mut TdsClient, version: DataClassificationVersion) {
        assert!(client.on_rows());
        // Available once positioned, before any row is read.
        assert_classified(client.data_classification().unwrap(), version);
        let row = client.next_row().await.unwrap().expect("one row");
        assert_eq!(row, [ColumnValues::Int(7), ColumnValues::Int(7)]);
        assert!(client.next_row().await.unwrap().is_none());
        // Still describes the exhausted result until the client moves on.
        assert_classified(client.data_classification().unwrap(), version);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v1_classification_is_attached_to_its_result_set() {
        let version = DataClassificationVersion::Version1;
        let server = Server::start(version).await;
        let mut client = server.connect(Some(version)).await;
        assert_eq!(
            client.negotiated_data_classification_version(),
            Some(version)
        );

        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_classified_result(&mut client, version).await;
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_classification_carries_ranks() {
        let version = DataClassificationVersion::Version2;
        let server = Server::start(version).await;
        let mut client = server.connect(Some(version)).await;
        assert_eq!(
            client.negotiated_data_classification_version(),
            Some(version)
        );

        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_classified_result(&mut client, version).await;
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    /// RPC responses go through the same result-set positioning as batches.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_result_sets_carry_their_classification() {
        let version = DataClassificationVersion::Version2;
        let server = Server::start(version).await;
        let mut client = server.connect(Some(version)).await;

        client
            .execute_sp_executesql(CLASSIFIED.to_string(), Vec::new(), ())
            .await
            .unwrap();
        assert_classified_result(&mut client, version).await;
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unclassified_result_has_no_classification() {
        let version = DataClassificationVersion::Version2;
        let server = Server::start(version).await;
        let mut client = server.connect(Some(version)).await;

        client.execute(UNCLASSIFIED.to_string(), ()).await.unwrap();
        assert!(client.on_rows());
        assert!(client.data_classification().is_none());
        assert_eq!(
            client.next_row().await.unwrap(),
            Some(vec![ColumnValues::Int(1)])
        );
        assert!(client.next_row().await.unwrap().is_none());
        assert!(client.data_classification().is_none());
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    /// The first set is skipped without reading its rows, so its classification
    /// would reach the second set if attachment followed the stream rather
    /// than the COLMETADATA it belongs to.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_result_set_carries_only_its_own_classification() {
        let version = DataClassificationVersion::Version2;
        let server = Server::start(version).await;
        let mut client = server.connect(Some(version)).await;

        client
            .execute(THREE_RESULT_SETS.to_string(), ())
            .await
            .unwrap();
        assert_eq!(label(&client).as_deref(), Some("First"));

        assert_eq!(client.advance().await.unwrap(), StatementResult::Rows);
        assert!(client.data_classification().is_none());
        assert_eq!(
            client.next_row().await.unwrap(),
            Some(vec![ColumnValues::Int(1)])
        );
        assert!(client.next_row().await.unwrap().is_none());
        assert!(client.data_classification().is_none());

        assert_eq!(client.advance().await.unwrap(), StatementResult::Rows);
        assert_eq!(label(&client).as_deref(), Some("Third"));
        assert_eq!(
            client.next_row().await.unwrap(),
            Some(vec![ColumnValues::Int(3)])
        );

        assert_eq!(client.advance().await.unwrap(), StatementResult::End);
        assert!(client.data_classification().is_none());

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    /// Covers connection reuse: each completed request leaves nothing behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consecutive_queries_do_not_share_classification() {
        let version = DataClassificationVersion::Version1;
        let server = Server::start(version).await;
        let mut client = server.connect(Some(version)).await;

        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_classified_result(&mut client, version).await;
        client.close_query().await.unwrap();
        assert!(client.data_classification().is_none());

        client.execute(UNCLASSIFIED.to_string(), ()).await.unwrap();
        assert!(client.data_classification().is_none());
        client.close_query().await.unwrap();

        client
            .execute(OTHER_CLASSIFIED.to_string(), ())
            .await
            .unwrap();
        assert_eq!(label(&client).as_deref(), Some("Secret"));
        client.close_query().await.unwrap();

        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_classified_result(&mut client, version).await;
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    /// Closing drains the third, classified set; none of it may surface on the
    /// next request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_a_classified_result_early_leaves_no_classification() {
        let version = DataClassificationVersion::Version2;
        let server = Server::start(version).await;
        let mut client = server.connect(Some(version)).await;

        client
            .execute(THREE_RESULT_SETS.to_string(), ())
            .await
            .unwrap();
        assert_eq!(label(&client).as_deref(), Some("First"));
        client.close_query().await.unwrap();
        assert!(client.data_classification().is_none());

        client.execute(UNCLASSIFIED.to_string(), ()).await.unwrap();
        assert!(client.on_rows());
        assert!(client.data_classification().is_none());
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    /// The ATTENTION drain discards the rest of the response, including the
    /// third set's classification token.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_classified_results_leave_no_classification() {
        let version = DataClassificationVersion::Version2;
        let server = Server::start(version).await;
        let mut client = server.connect(Some(version)).await;

        let handle = CancelHandle::new();
        client
            .execute(
                THREE_RESULT_SETS.to_string(),
                ExecuteOptions::new().cancel(&handle),
            )
            .await
            .unwrap();
        assert_eq!(label(&client).as_deref(), Some("First"));

        handle.cancel();
        let error = client.next_row().await.unwrap_err();
        assert!(
            matches!(error, Error::OperationCancelledError(_)),
            "{error}"
        );
        assert!(!client.is_connection_dead());
        assert!(client.data_classification().is_none());

        client.execute(UNCLASSIFIED.to_string(), ()).await.unwrap();
        assert!(client.data_classification().is_none());
        assert_eq!(
            client.next_row().await.unwrap(),
            Some(vec![ColumnValues::Int(1)])
        );
        client.close_query().await.unwrap();

        client
            .execute(OTHER_CLASSIFIED.to_string(), ())
            .await
            .unwrap();
        assert_eq!(label(&client).as_deref(), Some("Secret"));
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    /// Without the feature the server should send no classification; one that
    /// does anyway is rejected rather than surfaced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unrequested_feature_rejects_a_classified_result_set() {
        let server = Server::start(DataClassificationVersion::Version2).await;
        let mut client = server.connect(None).await;
        assert_eq!(client.negotiated_data_classification_version(), None);

        let outcome = match client.execute(CLASSIFIED.to_string(), ()).await {
            Ok(_) => {
                assert!(client.data_classification().is_none());
                client.next_row().await.map(|_| ())
            }
            Err(error) => Err(error),
        };
        let error = outcome.unwrap_err();
        assert!(error.to_string().contains("not negotiated"), "{error}");
        assert!(client.data_classification().is_none());

        server.stop().await;
    }

    /// An unrequested client fails only if the token really arrives, which a
    /// negotiated client reading the row cannot show on its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_classification_reaches_an_unrequested_client_as_a_rejection() {
        let server = Server::start(DataClassificationVersion::Version2).await;
        let mut client = server.connect(None).await;

        let outcome = match client
            .execute_sp_executesql(CLASSIFIED.to_string(), Vec::new(), ())
            .await
        {
            Ok(_) => client.next_row().await.map(|_| ()),
            Err(error) => Err(error),
        };
        let error = outcome.unwrap_err();
        assert!(error.to_string().contains("not negotiated"), "{error}");

        server.stop().await;
    }

    /// Ends the session the way a KILL does between requests: the server
    /// closes the socket, so the client's next command must recover it.
    async fn kill_session(client: &mut TdsClient) {
        client.execute(KILL_SESSION.to_string(), ()).await.unwrap();
        while client.next_row().await.unwrap().is_some() {}
        client.close_query().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    }

    /// The reconnect LOGIN7 requests Data Classification again, and the
    /// recovered session parses with what its own FEATUREEXTACK enabled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_recovery_renegotiates_data_classification() {
        let version = DataClassificationVersion::Version2;
        let server = Server::start_recoverable(version).await;
        let mut client = server.connect(Some(version)).await;
        assert!(client.is_session_recovery_enabled());

        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_classified_result(&mut client, version).await;
        client.close_query().await.unwrap();

        kill_session(&mut client).await;
        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_eq!(client.connection_recovery_count(), 1);
        assert_eq!(server.recovery_login_requests().await, [Some(2)]);
        assert_eq!(
            client.negotiated_data_classification_version(),
            Some(version)
        );
        assert_classified_result(&mut client, version).await;
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    /// A recovered session that enables a lower version must parse `0xA3` with
    /// it. V1 bytes read with the old V2 layout would take ranks from the
    /// column data and fail or misreport.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovered_session_parses_with_the_version_it_negotiated() {
        let server = Server::start_recoverable(DataClassificationVersion::Version2).await;
        let mut client = server
            .connect(Some(DataClassificationVersion::Version2))
            .await;
        assert_eq!(
            client.negotiated_data_classification_version(),
            Some(DataClassificationVersion::Version2)
        );
        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_classified_result(&mut client, DataClassificationVersion::Version2).await;
        client.close_query().await.unwrap();

        {
            let mut registry = server.registry.lock().await;
            registry.set_data_classification_ack(DataClassificationAck::Version(1));
            registry.register_with_data_classification(
                CLASSIFIED,
                int_response(&["ssn", "phone"], 7),
                classified_token(DataClassificationVersion::Version1),
            );
        }
        kill_session(&mut client).await;

        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_eq!(client.connection_recovery_count(), 1);
        assert_eq!(server.recovery_login_requests().await, [Some(2)]);
        assert_eq!(
            client.negotiated_data_classification_version(),
            Some(DataClassificationVersion::Version1)
        );
        assert_classified_result(&mut client, DataClassificationVersion::Version1).await;
        client.close_query().await.unwrap();

        client.close_connection().await.unwrap();
        server.stop().await;
    }

    /// When the recovered session does not enable the feature, the version the
    /// dead session negotiated must not be used to parse a stray `0xA3`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovered_session_without_the_feature_rejects_classification() {
        let version = DataClassificationVersion::Version2;
        let server = Server::start_recoverable(version).await;
        let mut client = server.connect(Some(version)).await;
        client.execute(CLASSIFIED.to_string(), ()).await.unwrap();
        assert_classified_result(&mut client, version).await;
        client.close_query().await.unwrap();

        server
            .registry
            .lock()
            .await
            .set_data_classification_ack(DataClassificationAck::Omitted);
        kill_session(&mut client).await;

        let outcome = match client.execute(CLASSIFIED.to_string(), ()).await {
            Ok(_) => {
                assert!(client.data_classification().is_none());
                client.next_row().await.map(|_| ())
            }
            Err(error) => Err(error),
        };
        assert_eq!(client.connection_recovery_count(), 1);
        assert_eq!(client.negotiated_data_classification_version(), None);
        let error = outcome.unwrap_err();
        assert!(error.to_string().contains("not negotiated"), "{error}");

        server.stop().await;
    }
}
