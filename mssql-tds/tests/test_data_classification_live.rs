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
        DataClassificationMetadata, DataClassificationVersion, SensitivityRank,
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

    /// A classified table in tempdb, unique per test so runs sharing a server
    /// do not collide. Dropped by [`drop`](Self::drop).
    struct ClassifiedTable {
        name: String,
    }

    impl ClassifiedTable {
        async fn create() -> Self {
            use rand::Rng;
            let table = format!("dc_live_{:016x}", rand::rng().random::<u64>());
            let rows: Vec<String> = ROWS
                .iter()
                .map(|(id, ssn, email, note)| format!("({id}, N'{ssn}', N'{email}', N'{note}')"))
                .collect();
            // ADD SENSITIVITY CLASSIFICATION rejects a database part (error
            // 16110), so the setup runs in tempdb with two-part names.
            let classify = |column: &str, c: &Classification| {
                format!(
                    "ADD SENSITIVITY CLASSIFICATION TO dbo.{table}.{column} WITH (LABEL = '{}', \
                     LABEL_ID = '{}', INFORMATION_TYPE = '{}', INFORMATION_TYPE_ID = '{}', \
                     RANK = {});",
                    c.label, c.label_id, c.information_type, c.information_type_id, c.rank_name
                )
            };
            let mut admin = connect(None).await;
            exec_and_drain(&mut admin, "USE tempdb").await;
            exec_and_drain(
                &mut admin,
                &format!(
                    "CREATE TABLE dbo.{table} (id INT NOT NULL, ssn NVARCHAR(11) NOT NULL, \
                     email NVARCHAR(50) NOT NULL, note NVARCHAR(20) NOT NULL); \
                     INSERT INTO dbo.{table} VALUES {}; {} {}",
                    rows.join(", "),
                    classify("ssn", &SSN),
                    classify("email", &EMAIL)
                ),
            )
            .await;
            admin.close_connection().await.unwrap();
            Self {
                name: format!("tempdb.dbo.{table}"),
            }
        }

        fn select(&self) -> String {
            format!("SELECT id, ssn, email, note FROM {} ORDER BY id", self.name)
        }

        async fn drop(self) {
            let mut admin = connect(None).await;
            exec_and_drain(&mut admin, &format!("DROP TABLE {}", self.name)).await;
            admin.close_connection().await.unwrap();
        }
    }

    /// Runs `test` with the SELECT for a fresh classified table, dropping the
    /// table even when `test` panics so a failing run leaves nothing in tempdb.
    async fn with_classified_table<F, Fut>(test: F)
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        use futures::FutureExt;
        let table = ClassifiedTable::create().await;
        let outcome = std::panic::AssertUnwindSafe(test(table.select()))
            .catch_unwind()
            .await;
        table.drop().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
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
