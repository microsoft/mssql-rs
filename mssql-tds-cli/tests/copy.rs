// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{env, time::SystemTime};

use mssql_tds::{
    connection::{client_context::ClientContext, tds_client::ResultSet},
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
    datatypes::column_values::ColumnValues,
};
use tokio::process::Command;

#[tokio::test]
#[ignore = "Requires SQL Server: DB_HOST, DB_PORT, DB_USERNAME, SQL_PASSWORD"]
async fn copy_binary_values_across_connections() {
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
    let mut client = TdsConnectionProvider::new()
        .create_client(context, &server, None)
        .await
        .unwrap();
    let suffix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let table = format!("##CliCopy_{}_{suffix}", std::process::id());
    client
        .execute(
            format!("CREATE TABLE {table} (id INT NOT NULL, payload VARBINARY(MAX) NULL)"),
            (),
        )
        .await
        .unwrap();
    client.close_query().await.unwrap();

    let query = "SELECT id, payload FROM (VALUES
        (1, CAST(NULL AS VARBINARY(MAX))),
        (2, CAST(0x AS VARBINARY(MAX))),
        (3, CAST(0x00FF0180 AS VARBINARY(MAX))),
        (4, CAST(REPLICATE(CAST('x' AS VARCHAR(MAX)), 65536) AS VARBINARY(MAX))),
        (5, CAST(0xFF00 AS VARBINARY(MAX)))
        ) AS data(id, payload)";
    let command = |query: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mssql-tds-cli"));
        command
            .args([
                "copy",
                "--source-server",
                &server,
                "--source-database",
                "master",
                "--source-user",
                &user,
                "--destination-server",
                &server,
                "--destination-database",
                "master",
                "--destination-user",
                &user,
                "--destination-table",
                &table,
                "--query",
                query,
                "--batch-size",
                "2",
                "--trust-server-certificate",
            ])
            .env("MSSQL_SOURCE_PASSWORD", &password)
            .env("MSSQL_DESTINATION_PASSWORD", &password);
        command
    };
    let output = command(query).output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Copied 5 rows"));
    client
        .execute(format!("SELECT id, payload FROM {table} ORDER BY id"), ())
        .await
        .unwrap();
    let expected = [
        None,
        Some(vec![]),
        Some(vec![0, 255, 1, 128]),
        Some(vec![b'x'; 65536]),
        Some(vec![255, 0]),
    ];
    for (index, bytes) in expected.into_iter().enumerate() {
        let row = client.next_row().await.unwrap().unwrap();
        assert!(matches!(row[0], ColumnValues::Int(id) if id == index as i32 + 1));
        match (&row[1], bytes) {
            (ColumnValues::Null, None) => {}
            (ColumnValues::Bytes(actual), Some(expected)) => assert_eq!(*actual, expected),
            (actual, expected) => panic!("Unexpected payload: {actual:?}, expected {expected:?}"),
        }
    }
    assert!(client.next_row().await.unwrap().is_none());
    client.close_query().await.unwrap();

    let output = command(&format!("SELECT id, payload FROM {table} WHERE 1 = 0"))
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Copied 0 rows"));

    let output = command("SELECT CAST(1 AS INT) AS wrong_column")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("column count"));

    client
        .execute(format!("DROP TABLE {table}"), ())
        .await
        .unwrap();
    client.close_query().await.unwrap();
}
