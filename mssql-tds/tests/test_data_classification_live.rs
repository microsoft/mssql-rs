// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data Classification against a live SQL Server (2019 or later): the
//! classification is configured with `ADD SENSITIVITY CLASSIFICATION`, and the
//! metadata the client exposes must match it.
//!
//! Requires DB_HOST, DB_USERNAME, and SQL_PASSWORD; the login needs permission
//! to classify columns and to `KILL` its own session. Run with
//! `ENABLE_TEST_TRACE=true` to log the FEATUREEXTACK and `0xA3` tokens.

#[cfg(test)]
mod common;

#[cfg(test)]
mod data_classification_live {
    use crate::common::{build_tcp_datasource, create_context, init_tracing};
    use mssql_tds::connection::tds_client::{ResultSet, StatementResult, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::token::data_classification::{
        DataClassificationMetadata, DataClassificationVersion, InformationTypeReference,
        SensitivityLabelReference, SensitivityProperty, SensitivityRank,
    };

    struct Classification {
        label: &'static str,
        label_id: &'static str,
        information_type: &'static str,
        information_type_id: &'static str,
        rank: SensitivityRank,
        rank_name: &'static str,
    }

    const SSN: Classification = Classification {
        label: "Highly Confidential",
        label_id: "3302ae7f-b8ac-46bc-97f8-378828781efd",
        information_type: "National ID",
        information_type_id: "6f5a11a7-08b1-19c3-59e5-8c89cf4f8444",
        rank: SensitivityRank::Critical,
        rank_name: "CRITICAL",
    };

    const EMAIL: Classification = Classification {
        label: "Confidential",
        label_id: "331f0b13-76b5-2f1b-a77b-def5a73c73c2",
        information_type: "Contact Info",
        information_type_id: "5c503e21-22c6-81fa-620b-f369b8ec38d1",
        rank: SensitivityRank::Medium,
        rank_name: "MEDIUM",
    };

    /// `(id, ssn, email, note)`; `id` and `note` stay unclassified.
    const ROWS: [(i32, &str, &str, &str); 2] = [
        (1, "123-45-6789", "ann@contoso.com", "first"),
        (2, "987-65-4321", "bob@contoso.com", "second"),
    ];

    async fn connect(requested: Option<DataClassificationVersion>) -> TdsClient {
        init_tracing();
        let mut context = create_context();
        context.requested_data_classification_version = requested;
        TdsConnectionProvider {}
            .create_client(context, &build_tcp_datasource(), None)
            .await
            .unwrap()
    }

    async fn exec_and_drain(client: &mut TdsClient, sql: &str) {
        client.execute(sql.to_string(), ()).await.unwrap();
        loop {
            while client.next_row().await.unwrap().is_some() {}
            if !client.advance_to_rows().await.unwrap() {
                break;
            }
        }
        client.close_query().await.unwrap();
    }

    /// A table in tempdb, unique per test so runs sharing a server do not
    /// collide. Dropped by [`drop`](Self::drop).
    struct TempTable {
        name: String,
    }

    impl TempTable {
        /// `setup` gets the bare table name and returns the batch that creates,
        /// fills and classifies it. ADD SENSITIVITY CLASSIFICATION rejects a
        /// database part (error 16110), so the batch runs in tempdb with
        /// two-part names.
        async fn create(setup: impl FnOnce(&str) -> String) -> Self {
            use rand::Rng;
            let table = format!("dc_live_{:016x}", rand::rng().random::<u64>());
            let mut admin = connect(None).await;
            exec_and_drain(&mut admin, "USE tempdb").await;
            exec_and_drain(&mut admin, &setup(&table)).await;
            admin.close_connection().await.unwrap();
            Self {
                name: format!("tempdb.dbo.{table}"),
            }
        }

        async fn drop(self) {
            let mut admin = connect(None).await;
            exec_and_drain(&mut admin, &format!("DROP TABLE {}", self.name)).await;
            admin.close_connection().await.unwrap();
        }
    }

    /// Runs `test` with the three-part name of a fresh table, dropping the
    /// table even when `test` panics so a failing run leaves nothing in tempdb.
    async fn with_table<F, Fut>(setup: impl FnOnce(&str) -> String, test: F)
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        use futures::FutureExt;
        let table = TempTable::create(setup).await;
        let outcome = std::panic::AssertUnwindSafe(test(table.name.clone()))
            .catch_unwind()
            .await;
        table.drop().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// `id, ssn, email, note` with [`SSN`] and [`EMAIL`] classified; `test`
    /// gets the SELECT for it.
    async fn with_classified_table<F, Fut>(test: F)
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let setup = |table: &str| {
            let rows: Vec<String> = ROWS
                .iter()
                .map(|(id, ssn, email, note)| format!("({id}, N'{ssn}', N'{email}', N'{note}')"))
                .collect();
            let classify = |column: &str, c: &Classification| {
                format!(
                    "ADD SENSITIVITY CLASSIFICATION TO dbo.{table}.{column} WITH (LABEL = '{}', \
                     LABEL_ID = '{}', INFORMATION_TYPE = '{}', INFORMATION_TYPE_ID = '{}', \
                     RANK = {});",
                    c.label, c.label_id, c.information_type, c.information_type_id, c.rank_name
                )
            };
            format!(
                "CREATE TABLE dbo.{table} (id INT NOT NULL, ssn NVARCHAR(11) NOT NULL, \
                 email NVARCHAR(50) NOT NULL, note NVARCHAR(20) NOT NULL); \
                 INSERT INTO dbo.{table} VALUES {}; {} {}",
                rows.join(", "),
                classify("ssn", &SSN),
                classify("email", &EMAIL)
            )
        };
        with_table(setup, |table| {
            test(format!(
                "SELECT id, ssn, email, note FROM {table} ORDER BY id"
            ))
        })
        .await;
    }

    fn assert_column(
        metadata: &DataClassificationMetadata,
        column: usize,
        expected: Option<&Classification>,
        version: DataClassificationVersion,
    ) {
        let sensitivity = &metadata.columns()[column];
        let Some(expected) = expected else {
            assert!(
                !sensitivity.is_classified(),
                "column {column}: {sensitivity:?}"
            );
            return;
        };
        let [property] = sensitivity.properties() else {
            panic!("column {column}: expected one property, got {sensitivity:?}");
        };
        let label = metadata
            .resolve_label(property.sensitivity_label())
            .expect("label");
        assert_eq!(
            (label.name(), label.id()),
            (expected.label, expected.label_id)
        );
        let information_type = metadata
            .resolve_information_type(property.information_type())
            .expect("information type");
        assert_eq!(
            (information_type.name(), information_type.id()),
            (expected.information_type, expected.information_type_id)
        );
        let rank = (version == DataClassificationVersion::Version2).then_some(expected.rank);
        assert_eq!(property.rank(), rank, "column {column}");
    }

    async fn assert_classified_result(client: &mut TdsClient, version: DataClassificationVersion) {
        let metadata = client
            .data_classification()
            .expect("SQL Server sent no classification")
            .clone();
        println!("DATACLASSIFICATION received: {metadata:#?}");
        assert_eq!(metadata.version(), version);
        assert_eq!(metadata.columns().len(), 4);
        assert_column(&metadata, 0, None, version);
        assert_column(&metadata, 1, Some(&SSN), version);
        assert_column(&metadata, 2, Some(&EMAIL), version);
        assert_column(&metadata, 3, None, version);
        let result_set_rank =
            (version == DataClassificationVersion::Version2).then_some(SensitivityRank::Critical);
        assert_eq!(metadata.rank(), result_set_rank);

        for (id, ssn, email, note) in ROWS {
            let row = client.next_row().await.unwrap().expect("row");
            assert_eq!(row[0], ColumnValues::Int(id));
            for (value, expected) in row[1..].iter().zip([ssn, email, note]) {
                let ColumnValues::String(value) = value else {
                    panic!("expected a string, got {value:?}");
                };
                assert_eq!(value.to_utf8_string(), expected);
            }
        }
        assert!(client.next_row().await.unwrap().is_none());
    }

    async fn print_server(client: &mut TdsClient) {
        client
            .execute(
                "SELECT @@VERSION, SERVERPROPERTY('ProductVersion')".to_string(),
                (),
            )
            .await
            .unwrap();
        let row = client.next_row().await.unwrap().expect("row");
        println!("SQL Server: {row:?}");
        client.close_query().await.unwrap();
    }

    async fn assert_classification_matches(requested: DataClassificationVersion) {
        with_classified_table(|select| async move {
            let mut client = connect(Some(requested)).await;
            print_server(&mut client).await;
            println!(
                "requested {requested:?}, negotiated {:?}",
                client.negotiated_data_classification_version()
            );
            assert_eq!(
                client.negotiated_data_classification_version(),
                Some(requested)
            );

            // The unclassified second result set must not inherit the first's.
            client
                .execute(format!("{select}; SELECT 1 AS n"), ())
                .await
                .unwrap();
            assert_classified_result(&mut client, requested).await;
            assert_eq!(client.advance().await.unwrap(), StatementResult::Rows);
            assert!(client.data_classification().is_none());
            assert_eq!(
                client.next_row().await.unwrap(),
                Some(vec![ColumnValues::Int(1)])
            );
            client.close_query().await.unwrap();

            // The connection stays usable, and classifies the next query again.
            client.execute(select, ()).await.unwrap();
            assert_classified_result(&mut client, requested).await;
            client.close_query().await.unwrap();

            client.close_connection().await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn live_v2_classification_matches_the_server() {
        assert_classification_matches(DataClassificationVersion::Version2).await;
    }

    #[tokio::test]
    async fn live_v1_classification_matches_the_server() {
        assert_classification_matches(DataClassificationVersion::Version1).await;
    }

    /// `(label, information type, rank)` a property resolves to.
    type Resolved<'a> = (Option<&'a str>, Option<&'a str>, Option<SensitivityRank>);

    fn resolve<'a>(
        metadata: &'a DataClassificationMetadata,
        property: &SensitivityProperty,
    ) -> Resolved<'a> {
        (
            metadata
                .resolve_label(property.sensitivity_label())
                .map(|label| label.name()),
            metadata
                .resolve_information_type(property.information_type())
                .map(|information_type| information_type.name()),
            property.rank(),
        )
    }

    fn resolved_column<'a>(
        metadata: &'a DataClassificationMetadata,
        column: usize,
    ) -> Vec<Resolved<'a>> {
        metadata.columns()[column]
            .properties()
            .iter()
            .map(|property| resolve(metadata, property))
            .collect()
    }

    /// Shapes the main table does not produce: a classification with only a
    /// label or only an information type, two columns sharing a label, and
    /// derived columns combining classified sources.
    async fn assert_classification_shapes(version: DataClassificationVersion) {
        let setup = |table: &str| {
            format!(
                "CREATE TABLE dbo.{table} (label_only NVARCHAR(10), type_only NVARCHAR(10), \
                 first NVARCHAR(10), second NVARCHAR(10)); \
                 INSERT INTO dbo.{table} VALUES (N'a', N'b', N'c', N'd'); \
                 ADD SENSITIVITY CLASSIFICATION TO dbo.{table}.label_only \
                 WITH (LABEL = 'Confidential', LABEL_ID = 'L-conf'); \
                 ADD SENSITIVITY CLASSIFICATION TO dbo.{table}.type_only \
                 WITH (INFORMATION_TYPE = 'Contact Info', INFORMATION_TYPE_ID = 'T-contact'); \
                 ADD SENSITIVITY CLASSIFICATION TO dbo.{table}.first \
                 WITH (LABEL = 'Highly Confidential', LABEL_ID = 'L-high', \
                 INFORMATION_TYPE = 'National ID', INFORMATION_TYPE_ID = 'T-nid', RANK = CRITICAL); \
                 ADD SENSITIVITY CLASSIFICATION TO dbo.{table}.second \
                 WITH (LABEL = 'Highly Confidential', LABEL_ID = 'L-high', \
                 INFORMATION_TYPE = 'Financial', INFORMATION_TYPE_ID = 'T-fin', RANK = HIGH);"
            )
        };
        with_table(setup, |table| async move {
            let mut client = connect(Some(version)).await;
            client
                .execute(
                    format!(
                        "SELECT label_only, type_only, first, second, \
                         first + second AS derived, label_only + type_only AS mixed FROM {table}"
                    ),
                    (),
                )
                .await
                .unwrap();
            let metadata = client
                .data_classification()
                .expect("SQL Server sent no classification")
                .clone();
            println!("DATACLASSIFICATION received: {metadata:#?}");
            client.close_query().await.unwrap();
            client.close_connection().await.unwrap();

            let rank = |rank| (version == DataClassificationVersion::Version2).then_some(rank);
            let label_only = (
                Some("Confidential"),
                None,
                rank(SensitivityRank::NotDefined),
            );
            let type_only = (
                None,
                Some("Contact Info"),
                rank(SensitivityRank::NotDefined),
            );
            let first = (
                Some("Highly Confidential"),
                Some("National ID"),
                rank(SensitivityRank::Critical),
            );
            let second = (
                Some("Highly Confidential"),
                Some("Financial"),
                rank(SensitivityRank::High),
            );

            assert_eq!(metadata.version(), version);
            assert_eq!(metadata.rank(), rank(SensitivityRank::Critical));
            assert_eq!(metadata.columns().len(), 6);

            // The missing half of a partial classification is 0xFFFF on the wire.
            let [label_only_property] = metadata.columns()[0].properties() else {
                panic!("label_only: {:?}", metadata.columns()[0]);
            };
            assert_eq!(
                label_only_property.information_type(),
                InformationTypeReference::Unspecified
            );
            let [type_only_property] = metadata.columns()[1].properties() else {
                panic!("type_only: {:?}", metadata.columns()[1]);
            };
            assert_eq!(
                type_only_property.sensitivity_label(),
                SensitivityLabelReference::Unspecified
            );
            assert_eq!(resolved_column(&metadata, 0), [label_only]);
            assert_eq!(resolved_column(&metadata, 1), [type_only]);
            assert_eq!(resolved_column(&metadata, 2), [first]);
            assert_eq!(resolved_column(&metadata, 3), [second]);

            // SQL Server sends each dictionary entry once per result set, so
            // the two columns sharing a label refer to the same entry.
            assert_eq!(metadata.sensitivity_labels().len(), 2);
            assert_eq!(metadata.information_types().len(), 3);
            assert_eq!(
                metadata.columns()[2].properties()[0].sensitivity_label(),
                metadata.columns()[3].properties()[0].sensitivity_label()
            );

            // A derived column carries one property per classified source.
            for (column, sources) in [(4, [first, second]), (5, [label_only, type_only])] {
                let properties = resolved_column(&metadata, column);
                assert_eq!(properties.len(), 2, "column {column}: {properties:?}");
                for source in sources {
                    assert!(
                        properties.contains(&source),
                        "column {column}: {source:?} missing from {properties:?}"
                    );
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn live_v2_partial_shared_and_derived_classifications_match_the_server() {
        assert_classification_shapes(DataClassificationVersion::Version2).await;
    }

    #[tokio::test]
    async fn live_v1_partial_shared_and_derived_classifications_match_the_server() {
        assert_classification_shapes(DataClassificationVersion::Version1).await;
    }

    /// Without the feature SQL Server sends no `0xA3`, so the query reads as before.
    #[tokio::test]
    async fn live_unrequested_connection_receives_no_classification() {
        with_classified_table(|select| async move {
            let mut client = connect(None).await;
            assert_eq!(client.negotiated_data_classification_version(), None);

            client.execute(select, ()).await.unwrap();
            assert!(client.data_classification().is_none());
            let mut rows = 0;
            while client.next_row().await.unwrap().is_some() {
                rows += 1;
            }
            assert_eq!(rows, ROWS.len());
            client.close_query().await.unwrap();

            client.close_connection().await.unwrap();
        })
        .await;
    }

    /// The real recovery path: the session is killed from another connection,
    /// and the next command reconnects with a LOGIN7 that requests the feature
    /// again.
    #[tokio::test]
    async fn live_session_recovery_keeps_classification_working() {
        let version = DataClassificationVersion::Version2;
        with_classified_table(|select| async move {
            let mut client = connect(Some(version)).await;
            assert!(client.is_session_recovery_enabled());

            client.execute(select.clone(), ()).await.unwrap();
            assert_classified_result(&mut client, version).await;
            client.close_query().await.unwrap();

            client
                .execute("SELECT @@SPID".to_string(), ())
                .await
                .unwrap();
            let Some(row) = client.next_row().await.unwrap() else {
                panic!("@@SPID returned no row");
            };
            let ColumnValues::SmallInt(spid) = row[0] else {
                panic!("expected SMALLINT @@SPID, got {:?}", row[0]);
            };
            client.close_query().await.unwrap();
            let mut killer = connect(None).await;
            exec_and_drain(&mut killer, &format!("KILL {spid}")).await;
            killer.close_connection().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            client.execute(select, ()).await.unwrap();
            assert_eq!(client.connection_recovery_count(), 1);
            assert_eq!(
                client.negotiated_data_classification_version(),
                Some(version)
            );
            assert_classified_result(&mut client, version).await;
            client.close_query().await.unwrap();

            client.close_connection().await.unwrap();
        })
        .await;
    }
}
