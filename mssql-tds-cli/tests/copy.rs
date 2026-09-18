// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{
    env,
    process::Output,
    time::{Duration, SystemTime},
};

use mssql_tds::{
    connection::{
        client_context::ClientContext,
        tds_client::{ResultSet, TdsClient},
    },
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
    datatypes::column_values::ColumnValues,
};
use tokio::process::Command;

struct CopyTest {
    client: TdsClient,
    server: String,
    user: String,
    password: String,
    source: String,
    destination: String,
}

impl CopyTest {
    async fn new() -> Self {
        let host = env::var("DB_HOST").expect("DB_HOST environment variable not set");
        let port = env::var("DB_PORT").unwrap_or_else(|_| "1433".into());
        let user = env::var("DB_USERNAME").expect("DB_USERNAME environment variable not set");
        let password = env::var("SQL_PASSWORD")
            .or_else(|_| std::fs::read_to_string("/tmp/password").map(|s| s.trim().to_owned()))
            .expect("Set SQL_PASSWORD or /tmp/password");
        let server = format!("tcp:{host},{port}");
        let mut context = ClientContext::default();
        context.user_name = user.clone();
        context.password = password.clone();
        context.database = "master".into();
        context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::On,
            trust_server_certificate: true,
            host_name_in_cert: None,
            server_certificate: None,
        };
        let client = TdsConnectionProvider::new()
            .create_client(context, &server, None)
            .await
            .unwrap();
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!("##CliCopy_{}_{suffix}", std::process::id());
        Self {
            client,
            server,
            user,
            password,
            source: format!("{prefix}_source"),
            destination: format!("{prefix}_destination"),
        }
    }

    async fn execute(&mut self, sql: String) {
        self.client.execute(sql, ()).await.unwrap();
        self.client.close_query().await.unwrap();
    }

    async fn copy(&self, query: &str, batch_size: u32) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mssql-tds-cli"));
        command
            .args([
                "copy",
                "--source-server",
                &self.server,
                "--source-database",
                "master",
                "--source-user",
                &self.user,
                "--destination-server",
                &self.server,
                "--destination-database",
                "master",
                "--destination-user",
                &self.user,
                "--destination-table",
                &self.destination,
                "--query",
                query,
                "--batch-size",
                &batch_size.to_string(),
                "--trust-server-certificate",
            ])
            .env("MSSQL_SOURCE_PASSWORD", &self.password)
            .env("MSSQL_DESTINATION_PASSWORD", &self.password)
            .kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(120), command.output())
            .await
            .expect("copy command timed out")
            .unwrap()
    }

    async fn assert_ids(&mut self, expected: &[i32]) {
        self.client
            .execute(
                format!("SELECT id FROM {} ORDER BY id", self.destination),
                (),
            )
            .await
            .unwrap();
        for expected in expected {
            let row = self.client.next_row().await.unwrap().unwrap();
            assert!(matches!(row[0], ColumnValues::Int(id) if id == *expected));
        }
        assert!(self.client.next_row().await.unwrap().is_none());
        self.client.close_query().await.unwrap();
    }

    async fn cleanup(&mut self) {
        self.execute(format!(
            "DROP TABLE {}; DROP TABLE {}",
            self.source, self.destination
        ))
        .await;
    }
}

fn assert_success(output: &Output, rows: usize) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("Copied {rows} rows")));
}

fn assert_failure(output: &Output) -> String {
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Copied "));
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_bytes(value: &ColumnValues, expected: &[u8]) {
    let ColumnValues::Bytes(actual) = value else {
        panic!("Expected binary value, got {value:?}");
    };
    assert_eq!(actual.len(), expected.len());
    assert!(actual == expected, "Binary contents differ");
}

fn assert_text(value: &ColumnValues, expected: &str) {
    let ColumnValues::String(actual) = value else {
        panic!("Expected string value, got {value:?}");
    };
    let actual = actual.to_utf8_string();
    assert_eq!(actual.len(), expected.len());
    assert!(actual == expected, "Text contents differ");
}

#[tokio::test]
#[ignore = "Requires SQL Server: DB_HOST, DB_PORT, DB_USERNAME, SQL_PASSWORD"]
async fn copy_max_values_across_connections() {
    let mut test = CopyTest::new().await;
    let all_bytes: Vec<u8> = (0..=255).collect();
    let hex: String = all_bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    let large_size = 2 * 1024 * 1024 + 17;
    let schema = "(id INT NOT NULL, payload VARBINARY(MAX) NULL,
        utf8 VARCHAR(MAX) COLLATE Latin1_General_100_CI_AS_SC_UTF8 NULL,
        unicode NVARCHAR(MAX) NULL, second_payload VARBINARY(MAX) NULL, tail INT NOT NULL)";
    test.execute(format!(
        "CREATE TABLE {} {schema}; CREATE TABLE {} {schema};
         INSERT INTO {} VALUES
         (1, NULL, NULL, NULL, NULL, 101),
         (2, 0x, '', N'', 0x, 102),
         (3, 0x{hex}, N'é漢😀', N'é漢😀', 0xFF0080, 103),
         (4, CONVERT(VARBINARY(MAX), REPLICATE(CAST('x' AS VARCHAR(MAX)), {large_size})) + 0x{hex},
          REPLICATE(CAST('a' AS VARCHAR(MAX)), 65535) +
            CONVERT(VARCHAR(MAX), N'😀é漢' COLLATE Latin1_General_100_CI_AS_SC_UTF8),
          REPLICATE(CAST(N'b' AS NVARCHAR(MAX)), 32767) + N'😀é漢',
          0x{hex}, 104),
         (5, 0x00FF, NULL, N'after large value 😀', 0x, 105)",
        test.source, test.destination, test.source
    ))
    .await;
    let query = format!("SELECT * FROM {} ORDER BY id", test.source);
    for batch_size in [2, 5000] {
        assert_success(&test.copy(&query, batch_size).await, 5);
        test.client
            .execute(
                format!("SELECT * FROM {} ORDER BY id", test.destination),
                (),
            )
            .await
            .unwrap();
        for id in 1..=5 {
            let row = test.client.next_row().await.unwrap().unwrap();
            assert!(matches!(row[0], ColumnValues::Int(actual) if actual == id));
            assert!(matches!(row[5], ColumnValues::Int(actual) if actual == id + 100));
            match id {
                1 => assert!(row[1..5].iter().all(|v| matches!(v, ColumnValues::Null))),
                2 => {
                    assert_bytes(&row[1], &[]);
                    assert_text(&row[2], "");
                    assert_text(&row[3], "");
                    assert_bytes(&row[4], &[]);
                }
                3 => {
                    assert_bytes(&row[1], &all_bytes);
                    assert_text(&row[2], "é漢😀");
                    assert_text(&row[3], "é漢😀");
                    assert_bytes(&row[4], &[255, 0, 128]);
                }
                4 => {
                    let mut expected = vec![b'x'; large_size];
                    expected.extend_from_slice(&all_bytes);
                    assert_bytes(&row[1], &expected);
                    assert_text(&row[2], &("a".repeat(65535) + "😀é漢"));
                    assert_text(&row[3], &("b".repeat(32767) + "😀é漢"));
                    assert_bytes(&row[4], &all_bytes);
                }
                5 => {
                    assert_bytes(&row[1], &[0, 255]);
                    assert!(matches!(row[2], ColumnValues::Null));
                    assert_text(&row[3], "after large value 😀");
                    assert_bytes(&row[4], &[]);
                }
                _ => unreachable!(),
            }
        }
        assert!(test.client.next_row().await.unwrap().is_none());
        test.client.close_query().await.unwrap();
        test.execute(format!("TRUNCATE TABLE {}", test.destination))
            .await;
        let empty_query = format!("SELECT * FROM {} WHERE 1 = 0", test.source);
        assert_success(&test.copy(&empty_query, batch_size).await, 0);
        test.assert_ids(&[]).await;
    }
    let output = test.copy("SELECT CAST(1 AS INT) AS wrong_column", 2).await;
    assert!(assert_failure(&output).contains("column count"));
    test.assert_ids(&[]).await;
    test.cleanup().await;
}

#[tokio::test]
#[ignore = "Requires SQL Server: DB_HOST, DB_PORT, DB_USERNAME, SQL_PASSWORD"]
async fn copy_streaming_failures_roll_back_current_batch() {
    let mut test = CopyTest::new().await;
    test.execute(format!(
        "CREATE TABLE {} (ordinal INT PRIMARY KEY, id INT NOT NULL, payload VARBINARY(MAX), tail INT);
         CREATE TABLE {} (id INT PRIMARY KEY, payload VARBINARY(MAX), tail INT);
         INSERT INTO {} VALUES
         (1, 1, 0x01, 101), (2, 2, 0x02, 102),
         (3, 3, CONVERT(VARBINARY(MAX), REPLICATE(CAST('x' AS VARCHAR(MAX)), 2097169)), 103),
         (4, 3, 0x04, 104), (5, 5, 0x05, 105)",
        test.source, test.destination, test.source
    ))
    .await;
    for batch_size in [2, 5000] {
        let query = format!(
            "SELECT id, payload, tail FROM {} ORDER BY ordinal",
            test.source
        );
        let error = assert_failure(&test.copy(&query, batch_size).await);
        assert!(
            error.contains("PRIMARY KEY") || error.contains("duplicate key"),
            "{error}"
        );
        let committed: &[i32] = if batch_size == 2 { &[1, 2] } else { &[] };
        test.assert_ids(committed).await;
        test.execute(format!("TRUNCATE TABLE {}", test.destination))
            .await;

        // The large third row flushes source packets before the fourth row fails.
        let query = format!(
            "SELECT ordinal AS id, payload, 10 / (4 - ordinal) AS tail
             FROM {} ORDER BY ordinal",
            test.source
        );
        let error = assert_failure(&test.copy(&query, batch_size).await);
        assert!(error.to_lowercase().contains("divide by zero"), "{error}");
        test.assert_ids(committed).await;
        test.execute(format!("TRUNCATE TABLE {}", test.destination))
            .await;
    }
    test.cleanup().await;
}
