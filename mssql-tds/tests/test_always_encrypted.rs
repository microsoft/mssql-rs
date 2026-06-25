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
//! Each test provisions its own throwaway column master key (CMK), column
//! encryption key (CEK), and encrypted table(s). No static certificate or key
//! material is embedded in (or committed with) this file: a fresh RSA-2048
//! master key and a random CEK are generated for every test run. Object names
//! are suffixed with a per-run UUID so concurrently running tests never collide,
//! and every test tears down its server-side objects even if an assertion fails.
//!
//! Note the T-SQL column DDL algorithm name is `AEAD_AES_256_CBC_HMAC_SHA_256`
//! (with the underscore before `256`), which differs from the wire/internal
//! algorithm identifier `AEAD_AES_256_CBC_HMAC_SHA256` used in the protocol.
#![cfg(feature = "column-encryption")]

#[cfg(test)]
mod common;

mod always_encrypted {
    use std::panic::AssertUnwindSafe;
    use std::sync::Arc;

    use futures::future::FutureExt;
    use rand::RngCore;
    use uuid::Uuid;

    use crate::common::{build_tcp_datasource, create_context, get_first_row, init_tracing};
    use mssql_tds::connection::client_context::ColumnEncryptionSetting;
    use mssql_tds::connection::tds_client::{ResultSetClient, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::{
        ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
        SqlSmallDateTime, SqlSmallMoney, SqlTime,
    };
    use mssql_tds::datatypes::decoder::DecimalParts;
    use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
    use mssql_tds::security::CertificateKeyStoreProvider;

    /// The certificate key-store provider name SQL Server records in the CMK.
    const KEY_STORE_PROVIDER_NAME: &str = "MSSQL_CERTIFICATE_STORE";
    /// The only cell/key encryption algorithm Always Encrypted supports.
    const COLUMN_ALGORITHM: &str = "AEAD_AES_256_CBC_HMAC_SHA_256";
    /// A `_BIN2` collation is required for DETERMINISTIC encryption of character
    /// columns.
    const BIN2_COLLATION: &str = "Latin1_General_BIN2";

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Runs a non-query statement and drains any (empty) result.
    async fn run_statement(client: &mut TdsClient, sql: &str) -> TdsResult<()> {
        client.execute(sql.to_string(), None, None).await?;
        while client.move_to_next().await? {}
        client.close_query().await?;
        Ok(())
    }

    /// Connects with Always Encrypted enabled and the supplied certificate
    /// provider registered under the certificate-store provider name.
    async fn connect_enabled(provider: Arc<CertificateKeyStoreProvider>) -> TdsClient {
        let mut context = create_context();
        context.column_encryption_setting = ColumnEncryptionSetting::Enabled;
        context.register_column_encryption_key_store_provider(KEY_STORE_PROVIDER_NAME, provider);

        TdsConnectionProvider {}
            .create_client(context, &build_tcp_datasource(), None)
            .await
            .expect("connect with Always Encrypted enabled")
    }

    /// Connects with Always Encrypted disabled (the default). Encrypted columns
    /// are returned as raw `varbinary` ciphertext rather than decrypted.
    async fn connect_disabled() -> TdsClient {
        TdsConnectionProvider {}
            .create_client(create_context(), &build_tcp_datasource(), None)
            .await
            .expect("connect with Always Encrypted disabled")
    }

    /// Selects the single `val` column from `table`, returning the decoded value
    /// (or an error, so failure-path tests can assert decryption failures).
    async fn select_val(client: &mut TdsClient, table: &str) -> TdsResult<ColumnValues> {
        client
            .execute(format!("SELECT val FROM {table};"), None, None)
            .await?;
        let (_metadata, row) = get_first_row(client).await?;
        assert_eq!(row.len(), 1, "expected a single column");
        Ok(row.into_iter().next().expect("one column value"))
    }

    /// Per-run Always Encrypted fixture: a fresh master key, a random CEK, an
    /// AE-enabled connection, and bookkeeping for cleanup.
    struct AeHarness {
        client: TdsClient,
        master_key_path: String,
        cmk_name: String,
        cek_name: String,
        suffix: String,
        table_seq: u32,
        created_tables: Vec<String>,
    }

    impl AeHarness {
        /// Provisions a throwaway CMK + CEK for this test run. The RSA master key
        /// and the CEK are both generated fresh in memory and never persisted.
        async fn setup() -> AeHarness {
            init_tracing();

            let suffix = Uuid::new_v4().simple().to_string();
            let master_key_path = format!("CurrentUser/My/mssql-rs-ae-{suffix}");
            let cmk_name = format!("ae_cmk_{suffix}");
            let cek_name = format!("ae_cek_{suffix}");

            // Generate a throwaway column master key (RSA-2048) for this run only.
            let mut provider = CertificateKeyStoreProvider::new();
            provider
                .generate_and_add_key(&master_key_path)
                .expect("generate throwaway master key");
            let provider = Arc::new(provider);

            // Generate a random column encryption key for this run only and wrap
            // it with the master key to obtain the value SQL Server stores.
            let mut plaintext_cek = [0u8; 32];
            rand::rng().fill_bytes(&mut plaintext_cek);
            let encrypted_cek = provider
                .encrypt_column_encryption_key(&master_key_path, "RSA_OAEP", &plaintext_cek)
                .expect("wrap CEK with master key");
            let encrypted_cek_hex = hex(&encrypted_cek);

            let mut client = connect_enabled(provider).await;

            run_statement(
                &mut client,
                &format!(
                    "CREATE COLUMN MASTER KEY {cmk_name} WITH (KEY_STORE_PROVIDER_NAME = \
                     '{KEY_STORE_PROVIDER_NAME}', KEY_PATH = '{master_key_path}');"
                ),
            )
            .await
            .expect("create column master key");

            run_statement(
                &mut client,
                &format!(
                    "CREATE COLUMN ENCRYPTION KEY {cek_name} WITH VALUES (COLUMN_MASTER_KEY = \
                     {cmk_name}, ALGORITHM = 'RSA_OAEP', ENCRYPTED_VALUE = 0x{encrypted_cek_hex});"
                ),
            )
            .await
            .expect("create column encryption key");

            AeHarness {
                client,
                master_key_path,
                cmk_name,
                cek_name,
                suffix,
                table_seq: 0,
                created_tables: Vec::new(),
            }
        }

        /// Reserves and records a unique table name for this run.
        fn next_table(&mut self) -> String {
            let name = format!("dbo.ae_t{}_{}", self.table_seq, self.suffix);
            self.table_seq += 1;
            self.created_tables.push(name.clone());
            name
        }

        /// Creates a table with a single encrypted `val` column using this run's
        /// CEK, the given column definition, and encryption type.
        async fn create_encrypted_table(
            &mut self,
            column_ddl: &str,
            encryption_type: &str,
        ) -> String {
            let table = self.next_table();
            let sql = format!(
                "CREATE TABLE {table} (id INT IDENTITY(1,1) PRIMARY KEY, val {column_ddl} \
                 ENCRYPTED WITH (COLUMN_ENCRYPTION_KEY = {cek}, ENCRYPTION_TYPE = \
                 {encryption_type}, ALGORITHM = '{COLUMN_ALGORITHM}') NULL);",
                cek = self.cek_name,
            );
            run_statement(&mut self.client, &sql)
                .await
                .expect("create encrypted table");
            table
        }

        /// Inserts `value` into `table.val` through an encrypted parameter. The
        /// driver calls `sp_describe_parameter_encryption`, learns the column is
        /// encrypted, and encrypts the value before sending it.
        async fn insert_encrypted(&mut self, table: &str, value: SqlType) {
            let param = RpcParameter::new(Some("@val".to_string()), StatusFlags::NONE, value);
            self.client
                .execute_sp_executesql(
                    format!("INSERT INTO {table} (val) VALUES (@val);"),
                    vec![param],
                    None,
                    None,
                )
                .await
                .expect("encrypted insert");
            while self.client.move_to_next().await.unwrap() {}
            self.client.close_query().await.unwrap();
        }

        /// Full round-trip: create an encrypted table, insert `value` through an
        /// encrypted parameter, read it back transparently decrypted, and hand
        /// the decoded value to `expect`.
        async fn roundtrip(
            &mut self,
            column_ddl: &str,
            encryption_type: &str,
            value: SqlType,
            expect: impl FnOnce(&ColumnValues),
        ) {
            let table = self
                .create_encrypted_table(column_ddl, encryption_type)
                .await;
            self.insert_encrypted(&table, value).await;
            let got = select_val(&mut self.client, &table)
                .await
                .expect("read back encrypted column");
            expect(&got);
        }

        /// Drops every object this run created, ignoring errors so cleanup is
        /// best-effort even if the connection is unhealthy.
        async fn teardown(mut self) {
            for table in self.created_tables.clone() {
                let _ = run_statement(
                    &mut self.client,
                    &format!("IF OBJECT_ID('{table}','U') IS NOT NULL DROP TABLE {table};"),
                )
                .await;
            }
            let _ = run_statement(
                &mut self.client,
                &format!(
                    "IF EXISTS (SELECT 1 FROM sys.column_encryption_keys WHERE name='{cek}') \
                     DROP COLUMN ENCRYPTION KEY {cek};",
                    cek = self.cek_name,
                ),
            )
            .await;
            let _ = run_statement(
                &mut self.client,
                &format!(
                    "IF EXISTS (SELECT 1 FROM sys.column_master_keys WHERE name='{cmk}') \
                     DROP COLUMN MASTER KEY {cmk};",
                    cmk = self.cmk_name,
                ),
            )
            .await;
        }
    }

    /// Runs an AE test body with a fresh [`AeHarness`], guaranteeing teardown
    /// even if the body panics (so failed assertions never leak CMK/CEK/tables).
    macro_rules! ae_test {
        (|$h:ident| $body:block) => {{
            let mut $h = AeHarness::setup().await;
            let outcome = AssertUnwindSafe(async { $body }).catch_unwind().await;
            $h.teardown().await;
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
        }};
    }

    // ----- Success paths: per-data-type round-trips (DETERMINISTIC) -----

    #[tokio::test]
    async fn roundtrip_integer_types() {
        ae_test!(|h| {
            h.roundtrip("BIT", "DETERMINISTIC", SqlType::Bit(Some(true)), |v| {
                assert!(matches!(v, ColumnValues::Bit(true)), "bit, got {v:?}");
            })
            .await;
            h.roundtrip(
                "TINYINT",
                "DETERMINISTIC",
                SqlType::TinyInt(Some(200)),
                |v| {
                    assert!(
                        matches!(v, ColumnValues::TinyInt(200)),
                        "tinyint, got {v:?}"
                    );
                },
            )
            .await;
            h.roundtrip(
                "SMALLINT",
                "DETERMINISTIC",
                SqlType::SmallInt(Some(-12345)),
                |v| {
                    assert!(
                        matches!(v, ColumnValues::SmallInt(-12345)),
                        "smallint, got {v:?}"
                    )
                },
            )
            .await;
            h.roundtrip("INT", "DETERMINISTIC", SqlType::Int(Some(1_234_567)), |v| {
                assert!(matches!(v, ColumnValues::Int(1_234_567)), "int, got {v:?}");
            })
            .await;
            h.roundtrip(
                "BIGINT",
                "DETERMINISTIC",
                SqlType::BigInt(Some(-9_876_543_210)),
                |v| {
                    assert!(
                        matches!(v, ColumnValues::BigInt(-9_876_543_210)),
                        "bigint, got {v:?}"
                    )
                },
            )
            .await;
        });
    }

    #[tokio::test]
    async fn roundtrip_real_and_float_types() {
        ae_test!(|h| {
            h.roundtrip(
                "REAL",
                "DETERMINISTIC",
                SqlType::Real(Some(3.14_f32)),
                |v| match v {
                    ColumnValues::Real(value) => assert_eq!(*value, 3.14_f32),
                    other => panic!("expected real, got {other:?}"),
                },
            )
            .await;
            h.roundtrip(
                "FLOAT",
                "DETERMINISTIC",
                SqlType::Float(Some(2.718281828_f64)),
                |v| match v {
                    ColumnValues::Float(value) => assert_eq!(*value, 2.718281828_f64),
                    other => panic!("expected float, got {other:?}"),
                },
            )
            .await;
        });
    }

    #[tokio::test]
    async fn roundtrip_decimal_and_money_types() {
        ae_test!(|h| {
            // 1234.5678 as decimal(18,4).
            let decimal = DecimalParts {
                is_positive: true,
                int_parts: vec![12_345_678],
                scale: 4,
                precision: 18,
            };
            h.roundtrip(
                "DECIMAL(18,4)",
                "DETERMINISTIC",
                SqlType::Decimal(Some(decimal.clone())),
                |v| match v {
                    ColumnValues::Decimal(value) => assert_eq!(value, &decimal),
                    other => panic!("expected decimal, got {other:?}"),
                },
            )
            .await;

            let numeric = DecimalParts {
                is_positive: false,
                int_parts: vec![98_765],
                scale: 2,
                precision: 12,
            };
            h.roundtrip(
                "NUMERIC(12,2)",
                "DETERMINISTIC",
                SqlType::Numeric(Some(numeric.clone())),
                |v| match v {
                    ColumnValues::Numeric(value) => assert_eq!(value, &numeric),
                    other => panic!("expected numeric, got {other:?}"),
                },
            )
            .await;

            // 123.4567 stored as money (value * 10^4).
            let money = SqlMoney {
                lsb_part: 1_234_567,
                msb_part: 0,
            };
            h.roundtrip(
                "MONEY",
                "DETERMINISTIC",
                SqlType::Money(Some(money.clone())),
                |v| match v {
                    ColumnValues::Money(value) => {
                        assert_eq!(value.lsb_part, money.lsb_part);
                        assert_eq!(value.msb_part, money.msb_part);
                    }
                    other => panic!("expected money, got {other:?}"),
                },
            )
            .await;

            // 12.3450 stored as smallmoney (value * 10^4).
            h.roundtrip(
                "SMALLMONEY",
                "DETERMINISTIC",
                SqlType::SmallMoney(Some(SqlSmallMoney { int_val: 123_450 })),
                |v| match v {
                    ColumnValues::SmallMoney(value) => assert_eq!(value.int_val, 123_450),
                    other => panic!("expected smallmoney, got {other:?}"),
                },
            )
            .await;
        });
    }

    #[tokio::test]
    async fn roundtrip_temporal_types() {
        ae_test!(|h| {
            let date = SqlDate::create(730_119).unwrap();
            h.roundtrip(
                "DATE",
                "DETERMINISTIC",
                SqlType::Date(Some(date.clone())),
                |v| match v {
                    ColumnValues::Date(value) => assert_eq!(value.get_days(), date.get_days()),
                    other => panic!("expected date, got {other:?}"),
                },
            )
            .await;

            let time = SqlTime {
                time_nanoseconds: 123_456_700,
                scale: 7,
            };
            h.roundtrip(
                "TIME(7)",
                "DETERMINISTIC",
                SqlType::Time(Some(time.clone())),
                |v| match v {
                    ColumnValues::Time(value) => assert_eq!(value, &time),
                    other => panic!("expected time, got {other:?}"),
                },
            )
            .await;

            let datetime2 = SqlDateTime2 {
                days: 730_119,
                time: SqlTime {
                    time_nanoseconds: 123_456_700,
                    scale: 7,
                },
            };
            h.roundtrip(
                "DATETIME2(7)",
                "DETERMINISTIC",
                SqlType::DateTime2(Some(datetime2.clone())),
                |v| match v {
                    ColumnValues::DateTime2(value) => assert_eq!(value, &datetime2),
                    other => panic!("expected datetime2, got {other:?}"),
                },
            )
            .await;

            let datetimeoffset = SqlDateTimeOffset {
                datetime2: datetime2.clone(),
                offset: 330,
            };
            h.roundtrip(
                "DATETIMEOFFSET(7)",
                "DETERMINISTIC",
                SqlType::DateTimeOffset(Some(datetimeoffset.clone())),
                |v| match v {
                    ColumnValues::DateTimeOffset(value) => assert_eq!(value, &datetimeoffset),
                    other => panic!("expected datetimeoffset, got {other:?}"),
                },
            )
            .await;

            let small_datetime = SqlSmallDateTime {
                days: 40_000,
                time: 720,
            };
            h.roundtrip(
                "SMALLDATETIME",
                "DETERMINISTIC",
                SqlType::SmallDateTime(Some(small_datetime.clone())),
                |v| match v {
                    ColumnValues::SmallDateTime(value) => assert_eq!(value, &small_datetime),
                    other => panic!("expected smalldatetime, got {other:?}"),
                },
            )
            .await;

            let datetime = SqlDateTime {
                days: 40_000,
                time: 1_080_000,
            };
            h.roundtrip(
                "DATETIME",
                "DETERMINISTIC",
                SqlType::DateTime(Some(datetime.clone())),
                |v| match v {
                    ColumnValues::DateTime(value) => assert_eq!(value, &datetime),
                    other => panic!("expected datetime, got {other:?}"),
                },
            )
            .await;
        });
    }

    #[tokio::test]
    async fn roundtrip_string_types() {
        ae_test!(|h| {
            // nvarchar: UTF-16 round-trip, including a non-ASCII codepoint.
            let nvarchar_text = "Always Encrypted \u{2726}";
            h.roundtrip(
                &format!("NVARCHAR(50) COLLATE {BIN2_COLLATION}"),
                "DETERMINISTIC",
                SqlType::NVarchar(
                    Some(SqlString::from_utf8_string(nvarchar_text.to_string())),
                    50,
                ),
                |v| match v {
                    ColumnValues::String(value) => {
                        assert_eq!(value.to_utf8_string(), nvarchar_text);
                    }
                    other => panic!("expected nvarchar string, got {other:?}"),
                },
            )
            .await;

            // varchar: single-byte (code page) round-trip with ASCII content.
            let varchar_text = "hello-ae";
            h.roundtrip(
                &format!("VARCHAR(50) COLLATE {BIN2_COLLATION}"),
                "DETERMINISTIC",
                SqlType::Varchar(
                    Some(SqlString::new(
                        varchar_text.as_bytes().to_vec(),
                        EncodingType::Utf8,
                    )),
                    50,
                ),
                |v| match v {
                    ColumnValues::String(value) => {
                        assert_eq!(value.to_utf8_string(), varchar_text);
                    }
                    other => panic!("expected varchar string, got {other:?}"),
                },
            )
            .await;
        });
    }

    #[tokio::test]
    async fn roundtrip_binary_and_guid_types() {
        ae_test!(|h| {
            let binary = vec![0xDE_u8, 0xAD, 0xBE, 0xEF];
            h.roundtrip(
                "BINARY(4)",
                "DETERMINISTIC",
                SqlType::Binary(Some(binary.clone()), 4),
                |v| match v {
                    ColumnValues::Bytes(value) => assert_eq!(value, &binary),
                    other => panic!("expected binary, got {other:?}"),
                },
            )
            .await;

            let varbinary = vec![1_u8, 2, 3, 4, 5];
            h.roundtrip(
                "VARBINARY(8)",
                "DETERMINISTIC",
                SqlType::VarBinary(Some(varbinary.clone()), 8),
                |v| match v {
                    ColumnValues::Bytes(value) => assert_eq!(value, &varbinary),
                    other => panic!("expected varbinary, got {other:?}"),
                },
            )
            .await;

            let guid = Uuid::new_v4();
            h.roundtrip(
                "UNIQUEIDENTIFIER",
                "DETERMINISTIC",
                SqlType::Uuid(Some(guid)),
                |v| match v {
                    ColumnValues::Uuid(value) => assert_eq!(*value, guid),
                    other => panic!("expected uniqueidentifier, got {other:?}"),
                },
            )
            .await;
        });
    }

    // ----- Success paths: RANDOMIZED encryption -----

    #[tokio::test]
    async fn roundtrip_randomized_encryption() {
        ae_test!(|h| {
            h.roundtrip("INT", "RANDOMIZED", SqlType::Int(Some(424_242)), |v| {
                assert!(
                    matches!(v, ColumnValues::Int(424_242)),
                    "rand int, got {v:?}"
                );
            })
            .await;

            let text = "randomized";
            h.roundtrip(
                "NVARCHAR(50)",
                "RANDOMIZED",
                SqlType::NVarchar(Some(SqlString::from_utf8_string(text.to_string())), 50),
                |v| match v {
                    ColumnValues::String(value) => assert_eq!(value.to_utf8_string(), text),
                    other => panic!("expected nvarchar string, got {other:?}"),
                },
            )
            .await;

            let datetime2 = SqlDateTime2 {
                days: 700_000,
                time: SqlTime {
                    time_nanoseconds: 555_000_000,
                    scale: 7,
                },
            };
            h.roundtrip(
                "DATETIME2(7)",
                "RANDOMIZED",
                SqlType::DateTime2(Some(datetime2.clone())),
                |v| match v {
                    ColumnValues::DateTime2(value) => assert_eq!(value, &datetime2),
                    other => panic!("expected datetime2, got {other:?}"),
                },
            )
            .await;
        });
    }

    // ----- Success paths: NULL values through encrypted columns -----

    #[tokio::test]
    async fn roundtrip_null_values() {
        ae_test!(|h| {
            h.roundtrip("INT", "DETERMINISTIC", SqlType::Int(None), |v| {
                assert!(matches!(v, ColumnValues::Null), "null int, got {v:?}");
            })
            .await;
            h.roundtrip(
                &format!("NVARCHAR(50) COLLATE {BIN2_COLLATION}"),
                "DETERMINISTIC",
                SqlType::NVarchar(None, 50),
                |v| assert!(matches!(v, ColumnValues::Null), "null nvarchar, got {v:?}"),
            )
            .await;
            h.roundtrip(
                "DATETIME2(7)",
                "RANDOMIZED",
                SqlType::DateTime2(None),
                |v| assert!(matches!(v, ColumnValues::Null), "null datetime2, got {v:?}"),
            )
            .await;
        });
    }

    // ----- AE-off behavior -----

    /// Reading an encrypted column over a connection that has Always Encrypted
    /// disabled returns the raw `varbinary` ciphertext (no decryption).
    #[tokio::test]
    async fn encrypted_column_read_with_ae_disabled_returns_varbinary() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            h.insert_encrypted(&table, SqlType::Int(Some(987_654)))
                .await;

            let mut plain = connect_disabled().await;
            let got = select_val(&mut plain, &table)
                .await
                .expect("read encrypted column with AE disabled");
            let _ = plain.close_query().await;

            match got {
                ColumnValues::Bytes(bytes) => {
                    // AEAD ciphertext (version + IV + ciphertext + MAC) is much
                    // larger than the 4-byte plaintext int, and is not the
                    // plaintext.
                    assert!(
                        bytes.len() > 4,
                        "expected AEAD ciphertext larger than the plaintext, got {} bytes",
                        bytes.len()
                    );
                    assert_ne!(
                        bytes,
                        987_654_i32.to_le_bytes().to_vec(),
                        "ciphertext must not equal the plaintext bytes"
                    );
                }
                other => {
                    panic!("expected raw varbinary ciphertext with AE disabled, got {other:?}")
                }
            }
        });
    }

    // ----- Failure paths -----

    /// A provider holding the wrong master key for the recorded key path cannot
    /// unwrap the CEK, so reading the encrypted column must fail.
    #[tokio::test]
    async fn wrong_master_key_fails_decryption() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            h.insert_encrypted(&table, SqlType::Int(Some(13))).await;

            let mut wrong_provider = CertificateKeyStoreProvider::new();
            wrong_provider
                .generate_and_add_key(&h.master_key_path)
                .expect("generate wrong master key");
            let mut client = connect_enabled(Arc::new(wrong_provider)).await;

            let result = select_val(&mut client, &table).await;
            let _ = client.close_query().await;
            assert!(
                result.is_err(),
                "decryption must fail with the wrong master key, got {result:?}"
            );
        });
    }

    /// A provider with no key registered for the recorded key path cannot unwrap
    /// the CEK, so reading the encrypted column must fail.
    #[tokio::test]
    async fn unregistered_master_key_fails_decryption() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            h.insert_encrypted(&table, SqlType::Int(Some(7))).await;

            // Empty provider: no key registered for any path.
            let mut client = connect_enabled(Arc::new(CertificateKeyStoreProvider::new())).await;

            let result = select_val(&mut client, &table).await;
            let _ = client.close_query().await;
            assert!(
                result.is_err(),
                "decryption must fail when no master key is registered, got {result:?}"
            );
        });
    }
}
