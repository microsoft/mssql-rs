// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end Always Encrypted integration tests against a live SQL Server.
//!
//! These tests require:
//! * the `column-encryption` cargo feature, and
//! * a reachable SQL Server (configured through the same `DB_HOST`/`DB_PORT`/
//!   `DB_USERNAME`/`SQL_PASSWORD` environment variables as the other
//!   integration tests).
//!
//! The test provisions its own column master key, column encryption key, and
//! an encrypted table, then verifies that a value inserted through an encrypted
//! parameter round-trips back through an encrypted result set. The column
//! master key is a throwaway RSA key embedded below (it never protects real
//! data), registered with the certificate key store provider.
#![cfg(feature = "column-encryption")]

#[cfg(test)]
mod common;

mod always_encrypted {
    use std::sync::Arc;

    use crate::common::{build_tcp_datasource, create_context, get_first_row, init_tracing};
    use mssql_tds::connection::client_context::ColumnEncryptionSetting;
    use mssql_tds::connection::tds_client::{ResultSetClient, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
    use mssql_tds::security::CertificateKeyStoreProvider;

    /// Opaque master key path. SQL Server stores this verbatim in the column
    /// master key definition; the provider matches it case-insensitively.
    const MASTER_KEY_PATH: &str = "CurrentUser/My/MSSQL-RS-AE-TEST";
    const CMK_NAME: &str = "AeTestRustCmk";
    const CEK_NAME: &str = "AeTestRustCek";
    const TABLE_NAME: &str = "dbo.AeTestRust";

    /// The plaintext column encryption key. Fixed so the wrapped value is
    /// reproducible across runs.
    const PLAINTEXT_CEK: [u8; 32] = [
        0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE,
        0xAF, 0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xBB, 0xBC, 0xBD,
        0xBE, 0xBF,
    ];

    const SECRET_VALUE: i32 = 1_234_567;

    fn build_certificate_provider() -> CertificateKeyStoreProvider {
        let mut provider = CertificateKeyStoreProvider::new();
        provider
            .generate_and_add_key(MASTER_KEY_PATH)
            .expect("test master key should generate");
        provider
    }

    /// Connects with Always Encrypted enabled and the certificate provider
    /// registered under the certificate-store provider name.
    async fn connect_with_always_encrypted() -> TdsClient {
        let mut context = create_context();
        context.column_encryption_setting = ColumnEncryptionSetting::Enabled;
        context.register_column_encryption_key_store_provider(
            "MSSQL_CERTIFICATE_STORE",
            Arc::new(build_certificate_provider()),
        );

        let provider = TdsConnectionProvider {};
        provider
            .create_client(context, &build_tcp_datasource(), None)
            .await
            .expect("connect with Always Encrypted enabled")
    }

    /// Runs a non-query statement and drains any (empty) result.
    async fn run_statement(client: &mut TdsClient, sql: &str) -> TdsResult<()> {
        client.execute(sql.to_string(), None, None).await?;
        while client.move_to_next().await? {}
        client.close_query().await?;
        Ok(())
    }

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// End-to-end check: a value inserted through an encrypted parameter is read
    /// back through an encrypted result set, transparently decrypted.
    ///
    /// Ignored by default because it requires a SQL Server instance that permits
    /// Always Encrypted column DDL (`CREATE TABLE ... ENCRYPTED WITH (...)`).
    /// Run explicitly against such a server with:
    /// `cargo test --features column-encryption --test test_always_encrypted -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a SQL Server that supports Always Encrypted column DDL"]
    async fn always_encrypted_parameter_and_result_roundtrip() {
        init_tracing();

        // Wrap the CEK with the master key to obtain the value SQL Server stores.
        let encrypted_cek = build_certificate_provider()
            .encrypt_column_encryption_key(MASTER_KEY_PATH, "RSA_OAEP", &PLAINTEXT_CEK)
            .expect("wrap CEK");
        let encrypted_cek_hex = hex(&encrypted_cek);

        let mut client = connect_with_always_encrypted().await;

        // Provision the AE schema (idempotent).
        run_statement(
            &mut client,
            &format!("IF OBJECT_ID('{TABLE_NAME}','U') IS NOT NULL DROP TABLE {TABLE_NAME};"),
        )
        .await
        .unwrap();
        run_statement(
            &mut client,
            &format!(
                "IF EXISTS (SELECT 1 FROM sys.column_encryption_keys WHERE name='{CEK_NAME}') \
                 DROP COLUMN ENCRYPTION KEY {CEK_NAME};"
            ),
        )
        .await
        .unwrap();
        run_statement(
            &mut client,
            &format!(
                "IF EXISTS (SELECT 1 FROM sys.column_master_keys WHERE name='{CMK_NAME}') \
                 DROP COLUMN MASTER KEY {CMK_NAME};"
            ),
        )
        .await
        .unwrap();

        run_statement(
            &mut client,
            &format!(
                "CREATE COLUMN MASTER KEY {CMK_NAME} WITH (KEY_STORE_PROVIDER_NAME = \
                 'MSSQL_CERTIFICATE_STORE', KEY_PATH = '{MASTER_KEY_PATH}');"
            ),
        )
        .await
        .unwrap();
        run_statement(
            &mut client,
            &format!(
                "CREATE COLUMN ENCRYPTION KEY {CEK_NAME} WITH VALUES (COLUMN_MASTER_KEY = \
                 {CMK_NAME}, ALGORITHM = 'RSA_OAEP', ENCRYPTED_VALUE = 0x{encrypted_cek_hex});"
            ),
        )
        .await
        .unwrap();
        run_statement(
            &mut client,
            &format!(
                "CREATE TABLE {TABLE_NAME} (id INT IDENTITY(1,1) PRIMARY KEY, secret INT \
                 ENCRYPTED WITH (COLUMN_ENCRYPTION_KEY = {CEK_NAME}, ENCRYPTION_TYPE = \
                 DETERMINISTIC, ALGORITHM = 'AEAD_AES_256_CBC_HMAC_SHA256') NOT NULL);"
            ),
        )
        .await
        .unwrap();

        // Insert through an encrypted parameter: the driver calls
        // sp_describe_parameter_encryption, learns @secret is encrypted, and
        // encrypts the value before sending it.
        let insert_param = RpcParameter::new(
            Some("@secret".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(SECRET_VALUE)),
        );
        client
            .execute_sp_executesql(
                format!("INSERT INTO {TABLE_NAME} (secret) VALUES (@secret);"),
                vec![insert_param],
                None,
                None,
            )
            .await
            .expect("encrypted insert");
        while client.move_to_next().await.unwrap() {}
        client.close_query().await.unwrap();

        // Read it back: the encrypted result column is transparently decrypted.
        client
            .execute(format!("SELECT secret FROM {TABLE_NAME};"), None, None)
            .await
            .expect("select encrypted column");
        let (_metadata, row) = get_first_row(&mut client).await.unwrap();

        assert_eq!(row.len(), 1, "expected a single secret column");
        match &row[0] {
            ColumnValues::Int(value) => assert_eq!(*value, SECRET_VALUE),
            other => panic!("expected a decrypted INT, got {other:?}"),
        }

        // Clean up.
        run_statement(
            &mut client,
            &format!("IF OBJECT_ID('{TABLE_NAME}','U') IS NOT NULL DROP TABLE {TABLE_NAME};"),
        )
        .await
        .unwrap();
    }
}
