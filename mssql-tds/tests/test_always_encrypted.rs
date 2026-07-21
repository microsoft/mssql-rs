// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end Always Encrypted integration tests against a live SQL Server.
//!
//! These tests require a reachable SQL Server (configured through the same
//! `DB_HOST`/`DB_PORT`/`DB_USERNAME`/`SQL_PASSWORD` environment variables as the
//! other integration tests).
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

#[cfg(test)]
mod common;

mod always_encrypted {
    use std::panic::AssertUnwindSafe;
    use std::sync::Arc;

    use futures::future::FutureExt;
    use rand::RngCore;
    use uuid::Uuid;

    use async_trait::async_trait;

    use crate::common::{build_tcp_datasource, create_context, get_first_row, init_tracing};
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::client_context::{
        ColumnEncryptionSetting, ExecutionColumnEncryptionSetting,
    };
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
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
    use mssql_tds::security::RsaKeyStoreProvider;

    /// The certificate key-store provider name SQL Server records in the CMK.
    const KEY_STORE_PROVIDER_NAME: &str = "MSSQL_CERTIFICATE_STORE";
    /// The only cell/key encryption algorithm Always Encrypted supports.
    const COLUMN_ALGORITHM: &str = "AEAD_AES_256_CBC_HMAC_SHA_256";
    /// A `_BIN2` collation is required for DETERMINISTIC encryption of character
    /// columns.
    const BIN2_COLLATION: &str = "Latin1_General_BIN2";
    /// A `_BIN2_UTF8` collation: required for DETERMINISTIC encryption of a UTF-8
    /// (`varchar`/`char`) character column so multi-byte UTF-8 bytes are stored.
    const BIN2_UTF8_COLLATION: &str = "Latin1_General_100_BIN2_UTF8";
    /// A rich Unicode sample exercising Latin diacritics, CJK, Cyrillic, Arabic,
    /// an em dash, and two supplementary-plane emoji (UTF-16 surrogate pairs /
    /// 4-byte UTF-8 sequences).
    const UNICODE_SAMPLE: &str = "Ünïcödé café — 日本語 Привет مرحبا 😀🔐";

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
    async fn connect_enabled(provider: Arc<RsaKeyStoreProvider>) -> TdsClient {
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
        created_procs: Vec<String>,
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
            let mut provider = RsaKeyStoreProvider::new();
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
                created_procs: Vec::new(),
            }
        }

        /// Reserves and records a unique table name for this run.
        fn next_table(&mut self) -> String {
            let name = format!("dbo.ae_t{}_{}", self.table_seq, self.suffix);
            self.table_seq += 1;
            self.created_tables.push(name.clone());
            name
        }

        /// Reserves and records a unique stored-procedure name for this run.
        fn next_proc(&mut self) -> String {
            let name = format!("dbo.ae_p{}_{}", self.created_procs.len(), self.suffix);
            self.created_procs.push(name.clone());
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

        /// Builds the `ENCRYPTED WITH (...)` column clause for this run's CEK.
        fn enc_clause(&self, encryption_type: &str) -> String {
            format!(
                "ENCRYPTED WITH (COLUMN_ENCRYPTION_KEY = {cek}, ENCRYPTION_TYPE = \
                 {encryption_type}, ALGORITHM = '{COLUMN_ALGORITHM}')",
                cek = self.cek_name,
            )
        }

        /// Creates a table (recorded for teardown) whose column list is `columns`
        /// (everything between the parentheses of `CREATE TABLE name (...)`).
        async fn create_table(&mut self, columns: &str) -> String {
            let table = self.next_table();
            run_statement(
                &mut self.client,
                &format!("CREATE TABLE {table} ({columns});"),
            )
            .await
            .expect("create table");
            table
        }

        /// Creates `(id INT NOT NULL, val <column_ddl> ENCRYPTED ... <null_ddl>)`
        /// and bulk-copies one row per value (id = 1..=n), returning the table.
        async fn bulk_copy_vals(
            &mut self,
            column_ddl: &str,
            encryption_type: &str,
            null_ddl: &str,
            vals: Vec<ColumnValues>,
        ) -> String {
            let enc = self.enc_clause(encryption_type);
            let table = self
                .create_table(&format!(
                    "id INT NOT NULL, val {column_ddl} {enc} {null_ddl}"
                ))
                .await;
            let n = vals.len();
            let rows: Vec<GenericBulkRow> = vals
                .into_iter()
                .enumerate()
                .map(|(i, v)| GenericBulkRow {
                    values: vec![ColumnValues::Int((i + 1) as i32), v],
                })
                .collect();
            let result = BulkCopy::new(&mut self.client, table.as_str())
                .batch_size(100)
                .write_to_server_zerocopy(rows)
                .await
                .expect("bulk copy into encrypted column");
            assert_eq!(result.rows_affected, n as u64, "expected {n} rows copied");
            table
        }

        /// Runs `select_sql` over the AE-enabled connection and returns every row
        /// as a vector of its first `ncols` (transparently decrypted) values.
        async fn query_rows(&mut self, select_sql: &str, ncols: usize) -> Vec<Vec<ColumnValues>> {
            self.client
                .execute(select_sql.to_string(), None, None)
                .await
                .expect("select rows");
            let mut rows = Vec::new();
            if let Some(resultset) = self.client.get_current_resultset() {
                while let Some(row) = resultset.next_row().await.expect("read row") {
                    rows.push((0..ncols).map(|i| row[i].clone()).collect());
                }
            }
            self.client.close_query().await.unwrap();
            rows
        }

        /// Drops every object this run created, ignoring errors so cleanup is
        /// best-effort even if the connection is unhealthy.
        async fn teardown(mut self) {
            for proc in self.created_procs.clone() {
                let _ = run_statement(
                    &mut self.client,
                    &format!("IF OBJECT_ID('{proc}','P') IS NOT NULL DROP PROCEDURE {proc};"),
                )
                .await;
            }
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
                SqlType::Real(Some(3.5_f32)),
                |v| match v {
                    ColumnValues::Real(value) => assert_eq!(*value, 3.5_f32),
                    other => panic!("expected real, got {other:?}"),
                },
            )
            .await;
            h.roundtrip(
                "FLOAT",
                "DETERMINISTIC",
                SqlType::Float(Some(2.5009_f64)),
                |v| match v {
                    ColumnValues::Float(value) => assert_eq!(*value, 2.5009_f64),
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

            // Negative smallmoney must sign-extend into the 8-byte money form.
            h.roundtrip(
                "SMALLMONEY",
                "DETERMINISTIC",
                SqlType::SmallMoney(Some(SqlSmallMoney { int_val: -123_450 })),
                |v| match v {
                    ColumnValues::SmallMoney(value) => assert_eq!(value.int_val, -123_450),
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

    /// `nvarchar` columns round-trip arbitrary Unicode through encryption,
    /// including non-Latin scripts and supplementary-plane characters (UTF-16
    /// surrogate pairs), under both DETERMINISTIC and RANDOMIZED encryption.
    #[tokio::test]
    async fn roundtrip_unicode_nvarchar() {
        ae_test!(|h| {
            h.roundtrip(
                &format!("NVARCHAR(256) COLLATE {BIN2_COLLATION}"),
                "DETERMINISTIC",
                SqlType::NVarchar(
                    Some(SqlString::from_utf8_string(UNICODE_SAMPLE.to_string())),
                    256,
                ),
                |v| match v {
                    ColumnValues::String(value) => {
                        assert_eq!(value.to_utf8_string(), UNICODE_SAMPLE);
                    }
                    other => panic!("expected nvarchar string, got {other:?}"),
                },
            )
            .await;

            h.roundtrip(
                "NVARCHAR(256)",
                "RANDOMIZED",
                SqlType::NVarchar(
                    Some(SqlString::from_utf8_string(UNICODE_SAMPLE.to_string())),
                    256,
                ),
                |v| match v {
                    ColumnValues::String(value) => {
                        assert_eq!(value.to_utf8_string(), UNICODE_SAMPLE);
                    }
                    other => panic!("expected nvarchar string, got {other:?}"),
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

    // ----- ForceColumnEncryption -----

    /// A parameter with `ForceColumnEncryption` set that targets an encrypted
    /// column is encrypted normally and round-trips — the flag only changes the
    /// failure behavior, not the success path.
    #[tokio::test]
    async fn force_column_encryption_encrypts_encrypted_column() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            let param = RpcParameter::new(
                Some("@val".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(4242)),
            )
            .with_force_column_encryption(true);
            h.client
                .execute_sp_executesql(
                    format!("INSERT INTO {table} (val) VALUES (@val);"),
                    vec![param],
                    None,
                    None,
                )
                .await
                .expect("force-encrypted insert into an encrypted column should succeed");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            let got = select_val(&mut h.client, &table)
                .await
                .expect("read back force-encrypted value");
            assert!(matches!(got, ColumnValues::Int(4242)), "got {got:?}");
        });
    }

    /// A parameter with `ForceColumnEncryption` set that targets a **plaintext**
    /// column is rejected: the server reports the column as not encrypted, so the
    /// driver refuses to send the value as plaintext (defending against a server
    /// that downgrades an encrypted column to harvest plaintext).
    #[tokio::test]
    async fn force_column_encryption_rejects_plaintext_column() {
        ae_test!(|h| {
            let table = h.create_table("val INT NULL").await;
            let param = RpcParameter::new(
                Some("@val".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(1)),
            )
            .with_force_column_encryption(true);
            let err = h
                .client
                .execute_sp_executesql(
                    format!("INSERT INTO {table} (val) VALUES (@val);"),
                    vec![param],
                    None,
                    None,
                )
                .await
                .expect_err("ForceColumnEncryption on a plaintext column must be rejected");
            assert!(
                matches!(&err, mssql_tds::error::Error::ColumnEncryptionError(m) if m.contains("ForceColumnEncryption")),
                "expected a ForceColumnEncryption column-encryption error, got {err:?}"
            );
        });
    }

    /// A parameter with `ForceColumnEncryption` set on a connection where Always
    /// Encrypted is not enabled is rejected before the value is sent, rather than
    /// silently transmitting it as plaintext.
    #[tokio::test]
    async fn force_column_encryption_without_ae_errors() {
        let mut client = connect_disabled().await;
        let param = RpcParameter::new(
            Some("@val".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(1)),
        )
        .with_force_column_encryption(true);
        let err = client
            .execute_sp_executesql("SELECT @val;".to_string(), vec![param], None, None)
            .await
            .expect_err("ForceColumnEncryption without Always Encrypted must be rejected");
        assert!(
            matches!(&err, mssql_tds::error::Error::UsageError(m) if m.contains("ForceColumnEncryption")),
            "expected a ForceColumnEncryption usage error, got {err:?}"
        );
    }

    // ----- Stored-procedure parameter encryption -----

    /// A named parameter passed to [`TdsClient::execute_stored_procedure`] that
    /// flows into an encrypted column is encrypted transparently: the driver
    /// runs `sp_describe_parameter_encryption` against the `EXEC` form of the
    /// call, learns the parameter must be encrypted, and sends ciphertext.
    #[tokio::test]
    async fn stored_procedure_encrypts_named_parameter() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            let proc = h.next_proc();
            run_statement(
                &mut h.client,
                &format!(
                    "CREATE PROCEDURE {proc} @val INT AS BEGIN \
                     INSERT INTO {table} (val) VALUES (@val); END"
                ),
            )
            .await
            .expect("create stored procedure");

            let param = RpcParameter::new(
                Some("@val".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(321)),
            );
            h.client
                .execute_stored_procedure(proc.clone(), None, Some(vec![param]), None, None)
                .await
                .expect("execute stored procedure with encrypted parameter");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            let got = select_val(&mut h.client, &table)
                .await
                .expect("read back value inserted via stored procedure");
            assert!(
                matches!(got, ColumnValues::Int(321)),
                "stored-procedure encrypted insert round-trip, got {got:?}"
            );
        });
    }

    /// An encrypted stored-procedure OUTPUT parameter comes back as a RETURNVALUE
    /// carrying `CryptoMetaData` and ciphertext (no CEK table). The driver must
    /// decrypt it transparently using the CEK it resolved for the matching input
    /// parameter during `sp_describe_parameter_encryption`.
    #[tokio::test]
    async fn encrypted_output_parameter_is_decrypted() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            h.insert_encrypted(&table, SqlType::Int(Some(4242))).await;

            let proc = h.next_proc();
            run_statement(
                &mut h.client,
                &format!(
                    "CREATE PROCEDURE {proc} @out INT OUTPUT AS BEGIN \
                     SELECT TOP 1 @out = val FROM {table} ORDER BY id; END"
                ),
            )
            .await
            .expect("create stored procedure with encrypted output parameter");

            // Output parameter: NULL placeholder input value, marked BY_REF so it
            // is declared `OUTPUT`. The driver encrypts the (NULL) input, then
            // decrypts the ciphertext the server returns.
            let out_param = RpcParameter::new(
                Some("@out".to_string()),
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(None),
            );
            h.client
                .execute_stored_procedure(proc.clone(), None, Some(vec![out_param]), None, None)
                .await
                .expect("execute stored procedure with encrypted output parameter");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            let return_values = h.client.get_return_values();
            let out = return_values
                .iter()
                .find(|rv| rv.param_name.eq_ignore_ascii_case("@out"))
                .expect("output parameter present in return values");
            assert!(
                matches!(out.value, ColumnValues::Int(4242)),
                "decrypted encrypted output parameter, got {:?}",
                out.value
            );
        });
    }

    /// A positional stored-procedure parameter that flows into an encrypted
    /// column is encrypted transparently: the driver describes the `EXEC` form
    /// with a synthetic name bound by position, learns the parameter must be
    /// encrypted, and sends ciphertext.
    #[tokio::test]
    async fn stored_procedure_encrypts_positional_parameter() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            let proc = h.next_proc();
            run_statement(
                &mut h.client,
                &format!(
                    "CREATE PROCEDURE {proc} @val INT AS BEGIN \
                     INSERT INTO {table} (val) VALUES (@val); END"
                ),
            )
            .await
            .expect("create stored procedure");

            // Unnamed (positional) parameter: bound to @val by position.
            let param = RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(321)));
            h.client
                .execute_stored_procedure(proc.clone(), Some(vec![param]), None, None, None)
                .await
                .expect("execute stored procedure with positional encrypted parameter");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            let got = select_val(&mut h.client, &table)
                .await
                .expect("read back value inserted via positional stored-procedure param");
            assert!(
                matches!(got, ColumnValues::Int(321)),
                "positional stored-procedure encrypted insert round-trip, got {got:?}"
            );
        });
    }

    /// A stored procedure called with both a positional and a named parameter,
    /// each flowing into a distinct encrypted column, encrypts both: positional
    /// parameters are described under synthetic names bound by position and
    /// named parameters bind by name, in a single describe/encrypt pass.
    #[tokio::test]
    async fn stored_procedure_encrypts_mixed_positional_and_named_parameters() {
        ae_test!(|h| {
            let enc = h.enc_clause("DETERMINISTIC");
            let table = h
                .create_table(&format!("a INT {enc} NULL, b INT {enc} NULL"))
                .await;
            let proc = h.next_proc();
            run_statement(
                &mut h.client,
                &format!(
                    "CREATE PROCEDURE {proc} @a INT, @b INT AS BEGIN \
                     INSERT INTO {table} (a, b) VALUES (@a, @b); END"
                ),
            )
            .await
            .expect("create stored procedure with two encrypted params");

            // @a supplied positionally, @b by name.
            let positional = RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(11)));
            let named = RpcParameter::new(
                Some("@b".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(22)),
            );
            h.client
                .execute_stored_procedure(
                    proc.clone(),
                    Some(vec![positional]),
                    Some(vec![named]),
                    None,
                    None,
                )
                .await
                .expect("execute stored procedure with mixed positional/named encrypted params");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            let rows = h.query_rows(&format!("SELECT a, b FROM {table};"), 2).await;
            assert_eq!(
                rows,
                vec![vec![ColumnValues::Int(11), ColumnValues::Int(22)]],
                "mixed positional/named encrypted stored-procedure insert round-trip"
            );
        });
    }

    /// An **encrypted** positional (unnamed) OUTPUT parameter cannot have its
    /// RETURNVALUE decrypted — it arrives unnamed, so its ciphertext can't be
    /// matched back to the CEK retained under the synthetic describe name — so
    /// the driver rejects it with an actionable error rather than returning
    /// ciphertext the caller can't read. (Pass such output parameters by name.)
    #[tokio::test]
    async fn encrypted_positional_output_parameter_is_rejected() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            h.insert_encrypted(&table, SqlType::Int(Some(4242))).await;
            let proc = h.next_proc();
            run_statement(
                &mut h.client,
                &format!(
                    "CREATE PROCEDURE {proc} @out INT OUTPUT AS BEGIN \
                     SELECT TOP 1 @out = val FROM {table} ORDER BY id; END"
                ),
            )
            .await
            .expect("create stored procedure with encrypted output parameter");

            // Positional (unnamed) OUTPUT parameter targeting the encrypted column.
            let out_param = RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::Int(None));
            let err = h
                .client
                .execute_stored_procedure(proc.clone(), Some(vec![out_param]), None, None, None)
                .await
                .expect_err("encrypted positional OUTPUT parameter must be rejected");
            assert!(
                matches!(&err, mssql_tds::error::Error::UsageError(m) if m.to_lowercase().contains("output")),
                "expected an actionable OUTPUT usage error, got {err:?}"
            );
        });
    }

    /// A **non-encrypted** positional OUTPUT parameter still works on an Always
    /// Encrypted connection: it is not flagged for encryption, so it is not
    /// rejected and its plaintext RETURNVALUE decodes normally. Guards against
    /// the encrypted-positional-OUTPUT rejection over-restricting.
    #[tokio::test]
    async fn plaintext_positional_output_parameter_round_trips_under_ae() {
        ae_test!(|h| {
            let proc = h.next_proc();
            run_statement(
                &mut h.client,
                &format!("CREATE PROCEDURE {proc} @out INT OUTPUT AS BEGIN SET @out = 7; END"),
            )
            .await
            .expect("create stored procedure with plaintext output parameter");

            let out_param = RpcParameter::new(None, StatusFlags::BY_REF_VALUE, SqlType::Int(None));
            h.client
                .execute_stored_procedure(proc.clone(), Some(vec![out_param]), None, None, None)
                .await
                .expect("plaintext positional OUTPUT parameter should be accepted");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            let return_values = h.client.get_return_values();
            assert_eq!(return_values.len(), 1, "one return value expected");
            assert!(
                matches!(return_values[0].value, ColumnValues::Int(7)),
                "plaintext positional output parameter round-trip, got {:?}",
                return_values[0].value
            );
        });
    }

    /// `sp_prepexec` prepares and executes in one round-trip; under Always
    /// Encrypted it describes the statement and encrypts flagged parameters, so
    /// an encrypted insert round-trips.
    #[tokio::test]
    async fn sp_prepexec_encrypts_parameter() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            let param = RpcParameter::new(
                Some("@val".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(555)),
            );
            h.client
                .execute_sp_prepexec(
                    format!("INSERT INTO {table} (val) VALUES (@val);"),
                    vec![param],
                    None,
                    None,
                    None,
                )
                .await
                .expect("sp_prepexec with encrypted parameter");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            let got = select_val(&mut h.client, &table)
                .await
                .expect("read back value inserted via sp_prepexec");
            assert!(
                matches!(got, ColumnValues::Int(555)),
                "sp_prepexec encrypted insert round-trip, got {got:?}"
            );
        });
    }

    /// `sp_prepare` describes the statement's parameters once and caches the
    /// metadata under the handle; each `sp_execute` then encrypts values from the
    /// cache without describing again. Uses named parameters.
    #[tokio::test]
    async fn sp_prepare_execute_encrypts_named_parameters() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            let decl = RpcParameter::new(
                Some("@val".to_string()),
                StatusFlags::NONE,
                SqlType::Int(None),
            );
            let handle = h
                .client
                .execute_sp_prepare(
                    format!("INSERT INTO {table} (val) VALUES (@val);"),
                    vec![decl],
                    None,
                    None,
                )
                .await
                .expect("sp_prepare with encrypted parameter");

            for v in [111, 222] {
                let param = RpcParameter::new(
                    Some("@val".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(v)),
                );
                h.client
                    .execute_sp_execute(handle, None, Some(vec![param]), None, None)
                    .await
                    .expect("sp_execute with encrypted named parameter");
                while h.client.move_to_next().await.unwrap() {}
                h.client.close_query().await.unwrap();
            }

            h.client
                .execute_sp_unprepare(handle, None, None)
                .await
                .expect("unprepare");

            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            assert_eq!(rows.len(), 2, "expected two encrypted rows inserted");
            assert!(
                matches!(rows[0][0], ColumnValues::Int(111)),
                "row0 {:?}",
                rows[0][0]
            );
            assert!(
                matches!(rows[1][0], ColumnValues::Int(222)),
                "row1 {:?}",
                rows[1][0]
            );
        });
    }

    /// `sp_execute` positional parameters (unnamed) are matched to the cached
    /// describe metadata by ordinal and encrypted.
    #[tokio::test]
    async fn sp_execute_encrypts_positional_parameter() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            let decl = RpcParameter::new(
                Some("@val".to_string()),
                StatusFlags::NONE,
                SqlType::Int(None),
            );
            let handle = h
                .client
                .execute_sp_prepare(
                    format!("INSERT INTO {table} (val) VALUES (@val);"),
                    vec![decl],
                    None,
                    None,
                )
                .await
                .expect("sp_prepare");

            // Positional (unnamed) value, matched to the declared @val by ordinal.
            let param = RpcParameter::new(None, StatusFlags::NONE, SqlType::Int(Some(999)));
            h.client
                .execute_sp_execute(handle, Some(vec![param]), None, None, None)
                .await
                .expect("sp_execute with positional encrypted parameter");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            h.client
                .execute_sp_unprepare(handle, None, None)
                .await
                .expect("unprepare");

            let got = select_val(&mut h.client, &table)
                .await
                .expect("read back positional prepared insert");
            assert!(
                matches!(got, ColumnValues::Int(999)),
                "sp_execute positional encrypted insert, got {got:?}"
            );
        });
    }

    /// A prepared statement with two encrypted parameters executed with one value
    /// supplied positionally and the other by name: both must be encrypted in a
    /// single pass. Regression for the two-list double-apply bug, where each list
    /// was described separately and the parameter in the other list was reported
    /// as "not supplied".
    #[tokio::test]
    async fn sp_execute_encrypts_mixed_positional_and_named_parameters() {
        ae_test!(|h| {
            let enc = h.enc_clause("DETERMINISTIC");
            let table = h
                .create_table(&format!(
                    "id INT IDENTITY(1,1) PRIMARY KEY, a INT {enc} NULL, b INT {enc} NULL"
                ))
                .await;
            let decls = vec![
                RpcParameter::new(
                    Some("@a".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(None),
                ),
                RpcParameter::new(
                    Some("@b".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(None),
                ),
            ];
            let handle = h
                .client
                .execute_sp_prepare(
                    format!("INSERT INTO {table} (a, b) VALUES (@a, @b);"),
                    decls,
                    None,
                    None,
                )
                .await
                .expect("sp_prepare two encrypted params");

            // @a supplied positionally (ordinal 1); @b supplied by name.
            let positional = vec![RpcParameter::new(
                None,
                StatusFlags::NONE,
                SqlType::Int(Some(11)),
            )];
            let named = vec![RpcParameter::new(
                Some("@b".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(22)),
            )];
            h.client
                .execute_sp_execute(handle, Some(positional), Some(named), None, None)
                .await
                .expect("sp_execute with mixed positional and named encrypted params");
            while h.client.move_to_next().await.unwrap() {}
            h.client.close_query().await.unwrap();

            h.client
                .execute_sp_unprepare(handle, None, None)
                .await
                .expect("unprepare");

            let rows = h.query_rows(&format!("SELECT a, b FROM {table};"), 2).await;
            assert_eq!(rows.len(), 1, "expected one row");
            assert!(
                matches!(rows[0][0], ColumnValues::Int(11)),
                "a {:?}",
                rows[0][0]
            );
            assert!(
                matches!(rows[0][1], ColumnValues::Int(22)),
                "b {:?}",
                rows[0][1]
            );
        });
    }

    /// `sp_execute` with parameters under Always Encrypted requires the handle to
    /// have been prepared on this connection (so its parameter-encryption metadata
    /// is cached). An unknown handle must error rather than send plaintext.
    #[tokio::test]
    async fn sp_execute_without_prepared_metadata_errors_under_ae() {
        ae_test!(|h| {
            let param = RpcParameter::new(
                Some("@val".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(1)),
            );
            let err = h
                .client
                .execute_sp_execute(999_999, None, Some(vec![param]), None, None)
                .await
                .expect_err("sp_execute with an unprepared handle must error under AE");
            assert!(
                matches!(err, mssql_tds::error::Error::ColumnEncryptionError(_)),
                "expected ColumnEncryptionError for unknown handle, got {err:?}"
            );
        });
    }

    /// The connection's query-metadata cache elides repeat
    /// `sp_describe_parameter_encryption` round-trips: executing the same
    /// statement multiple times describes it only once.
    #[tokio::test]
    async fn query_metadata_cache_reuses_describe() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            let sql = format!("INSERT INTO {table} (val) VALUES (@val);");

            let before = h.client.describe_round_trips();
            for v in [1, 2, 3] {
                let param = RpcParameter::new(
                    Some("@val".to_string()),
                    StatusFlags::NONE,
                    SqlType::Int(Some(v)),
                );
                h.client
                    .execute_sp_executesql(sql.clone(), vec![param], None, None)
                    .await
                    .expect("encrypted insert");
                while h.client.move_to_next().await.unwrap() {}
                h.client.close_query().await.unwrap();
            }
            let describes = h.client.describe_round_trips() - before;
            assert_eq!(
                describes, 1,
                "three identical executions should describe once, got {describes}"
            );
        });
    }

    // ----- Bulk copy parameter encryption -----

    /// A row written through the streaming bulk-copy writer. The `id` column is
    /// plaintext; the `val` column lands in an encrypted column, so the writer
    /// encrypts it transparently before the data goes on the wire.
    struct EncBulkRow {
        id: i32,
        val: i32,
    }

    #[async_trait]
    impl BulkLoadRow for EncBulkRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.id))
                .await?;
            *column_index += 1;
            writer
                .write_column_value(*column_index, &ColumnValues::Int(self.val))
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    /// Bulk-copying rows into a table whose `val` column is encrypted encrypts
    /// each value on the client: the destination metadata (fetched over the
    /// Always Encrypted-enabled connection) reports the column as encrypted, the
    /// driver resolves the CEK, and the streaming writer emits ciphertext. The
    /// values round-trip back transparently decrypted and are stored as
    /// `varbinary` ciphertext at rest.
    #[tokio::test]
    async fn bulk_copy_encrypts_values() {
        ae_test!(|h| {
            let table = h.next_table();
            run_statement(
                &mut h.client,
                &format!(
                    "CREATE TABLE {table} (id INT NOT NULL, val INT ENCRYPTED WITH \
                     (COLUMN_ENCRYPTION_KEY = {cek}, ENCRYPTION_TYPE = DETERMINISTIC, \
                     ALGORITHM = '{COLUMN_ALGORITHM}') NOT NULL);",
                    cek = h.cek_name,
                ),
            )
            .await
            .expect("create encrypted bulk-copy table");

            let rows = vec![
                EncBulkRow { id: 1, val: 111 },
                EncBulkRow { id: 2, val: 222 },
                EncBulkRow { id: 3, val: 333 },
            ];
            let result = BulkCopy::new(&mut h.client, table.as_str())
                .batch_size(100)
                .write_to_server_zerocopy(rows)
                .await
                .expect("bulk copy into encrypted column");
            assert_eq!(result.rows_affected, 3, "expected three rows copied");

            // Read back over the AE-enabled connection: values are decrypted.
            h.client
                .execute(
                    format!("SELECT id, val FROM {table} ORDER BY id;"),
                    None,
                    None,
                )
                .await
                .expect("select decrypted values");
            let mut got = Vec::new();
            if let Some(resultset) = h.client.get_current_resultset() {
                while let Some(row) = resultset.next_row().await.expect("read row") {
                    got.push((row[0].clone(), row[1].clone()));
                }
            }
            h.client.close_query().await.unwrap();
            assert_eq!(
                got,
                vec![
                    (ColumnValues::Int(1), ColumnValues::Int(111)),
                    (ColumnValues::Int(2), ColumnValues::Int(222)),
                    (ColumnValues::Int(3), ColumnValues::Int(333)),
                ],
                "bulk-copied encrypted values must round-trip"
            );

            // The column is stored as ciphertext: an AE-disabled connection sees
            // raw varbinary, not the plaintext int.
            let mut plain = connect_disabled().await;
            plain
                .execute(format!("SELECT val FROM {table} WHERE id = 1;"), None, None)
                .await
                .expect("select ciphertext with AE disabled");
            let (_metadata, row) = get_first_row(&mut plain).await.expect("ciphertext row");
            let _ = plain.close_query().await;
            match &row[0] {
                ColumnValues::Bytes(bytes) => {
                    assert!(
                        bytes.len() > 4,
                        "expected AEAD ciphertext larger than the plaintext, got {} bytes",
                        bytes.len()
                    );
                    assert_ne!(
                        *bytes,
                        111_i32.to_le_bytes().to_vec(),
                        "ciphertext must not equal the plaintext bytes"
                    );
                }
                other => panic!("expected varbinary ciphertext at rest, got {other:?}"),
            }
        });
    }

    /// A bulk-copy row holding an arbitrary column layout; each value is written
    /// in order. Encrypted destination columns are encrypted transparently.
    struct GenericBulkRow {
        values: Vec<ColumnValues>,
    }

    #[async_trait]
    impl BulkLoadRow for GenericBulkRow {
        async fn write_to_packet(
            &self,
            writer: &mut mssql_tds::message::bulk_load::StreamingBulkLoadWriter<'_>,
            column_index: &mut usize,
        ) -> TdsResult<()> {
            for value in &self.values {
                writer.write_column_value(*column_index, value).await?;
                *column_index += 1;
            }
            Ok(())
        }
    }

    /// Reads the raw `varbinary` ciphertext stored for `table.val WHERE id = {id}`
    /// over an Always Encrypted-disabled connection (no decryption).
    async fn read_ciphertext(table: &str, id: i32) -> Vec<u8> {
        let mut plain = connect_disabled().await;
        plain
            .execute(
                format!("SELECT val FROM {table} WHERE id = {id};"),
                None,
                None,
            )
            .await
            .expect("select ciphertext with AE disabled");
        let (_metadata, row) = get_first_row(&mut plain).await.expect("ciphertext row");
        let _ = plain.close_query().await;
        match row.into_iter().next().expect("one column") {
            ColumnValues::Bytes(bytes) => bytes,
            other => panic!("expected varbinary ciphertext at rest, got {other:?}"),
        }
    }

    /// `allow_encrypted_value_modifications` lets bulk copy move ciphertext
    /// between two columns that share a CEK without the driver re-encrypting it:
    /// the raw ciphertext read from one encrypted column is bulk-copied verbatim
    /// into another and still decrypts to the original plaintext.
    #[tokio::test]
    async fn bulk_copy_allow_encrypted_value_modifications_passthrough() {
        ae_test!(|h| {
            const SECRET: i32 = 4242;

            // Source table: insert through an encrypted parameter, then read the
            // raw ciphertext back over an AE-disabled connection.
            let source = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            h.insert_encrypted(&source, SqlType::Int(Some(SECRET)))
                .await;
            let ciphertext = read_ciphertext(&source, 1).await;

            // Destination table with the same CEK / algorithm / encryption type.
            let enc = h.enc_clause("DETERMINISTIC");
            let dest = h
                .create_table(&format!("id INT NOT NULL, val INT {enc} NULL"))
                .await;

            // Bulk-copy the ciphertext verbatim into the encrypted column. Without
            // passthrough the driver would try to encrypt these bytes again.
            let rows = vec![GenericBulkRow {
                values: vec![
                    ColumnValues::Int(1),
                    ColumnValues::Bytes(ciphertext.clone()),
                ],
            }];
            let result = BulkCopy::new(&mut h.client, dest.as_str())
                .allow_encrypted_value_modifications(true)
                .write_to_server_zerocopy(rows)
                .await
                .expect("passthrough bulk copy into encrypted column");
            assert_eq!(result.rows_affected, 1, "expected one row copied");

            // The destination stores the identical ciphertext at rest.
            let dest_ciphertext = read_ciphertext(&dest, 1).await;
            assert_eq!(
                dest_ciphertext, ciphertext,
                "passthrough must store the ciphertext verbatim"
            );

            // Reading the destination over the AE-enabled connection decrypts it
            // back to the original plaintext, proving the ciphertext was valid.
            let got = h
                .query_rows(&format!("SELECT val FROM {dest} WHERE id = 1;"), 1)
                .await;
            assert_eq!(got, vec![vec![ColumnValues::Int(SECRET)]]);
        });
    }

    /// Bulk-copying string and binary values into encrypted columns encrypts each
    /// one on the client and round-trips it back transparently decrypted.
    #[tokio::test]
    async fn bulk_copy_roundtrips_string_and_binary_types() {
        ae_test!(|h| {
            // nvarchar: UTF-16 round-trip including a non-ASCII codepoint.
            let nvarchar_text = "Bulk \u{2726} copy";
            let table = h
                .bulk_copy_vals(
                    &format!("NVARCHAR(50) COLLATE {BIN2_COLLATION}"),
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::String(SqlString::from_utf8_string(
                        nvarchar_text.to_string(),
                    ))],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::String(v) => assert_eq!(v.to_utf8_string(), nvarchar_text),
                other => panic!("expected nvarchar string, got {other:?}"),
            }

            // varchar: single-byte (code page) round-trip with ASCII content.
            let varchar_text = "bulk-ae-varchar";
            let table = h
                .bulk_copy_vals(
                    &format!("VARCHAR(50) COLLATE {BIN2_COLLATION}"),
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::String(SqlString::new(
                        varchar_text.as_bytes().to_vec(),
                        EncodingType::Utf8,
                    ))],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::String(v) => assert_eq!(v.to_utf8_string(), varchar_text),
                other => panic!("expected varchar string, got {other:?}"),
            }

            // varbinary: raw bytes round-trip.
            let varbinary = vec![0x01_u8, 0x02, 0x03, 0xFE, 0xFF];
            let table = h
                .bulk_copy_vals(
                    "VARBINARY(16)",
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::Bytes(varbinary.clone())],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::Bytes(v) => assert_eq!(v, &varbinary),
                other => panic!("expected varbinary, got {other:?}"),
            }

            // uniqueidentifier round-trip.
            let guid = Uuid::new_v4();
            let table = h
                .bulk_copy_vals(
                    "UNIQUEIDENTIFIER",
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::Uuid(guid)],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::Uuid(v) => assert_eq!(*v, guid),
                other => panic!("expected uniqueidentifier, got {other:?}"),
            }
        });
    }

    /// Bulk copy round-trips Unicode `nvarchar` (UTF-16) and UTF-8-collated
    /// `varchar` values through encrypted columns, including non-Latin scripts
    /// and supplementary-plane characters.
    #[tokio::test]
    async fn bulk_copy_unicode_and_utf8_strings() {
        ae_test!(|h| {
            // nvarchar (UTF-16) carrying rich Unicode.
            let table = h
                .bulk_copy_vals(
                    &format!("NVARCHAR(256) COLLATE {BIN2_COLLATION}"),
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::String(SqlString::from_utf8_string(
                        UNICODE_SAMPLE.to_string(),
                    ))],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::String(v) => assert_eq!(v.to_utf8_string(), UNICODE_SAMPLE),
                other => panic!("expected nvarchar, got {other:?}"),
            }

            // varchar with a UTF-8 collation carrying multi-byte UTF-8 content.
            let table = h
                .bulk_copy_vals(
                    &format!("VARCHAR(256) COLLATE {BIN2_UTF8_COLLATION}"),
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::String(SqlString::new(
                        UNICODE_SAMPLE.as_bytes().to_vec(),
                        EncodingType::Utf8,
                    ))],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::String(v) => assert_eq!(v.to_utf8_string(), UNICODE_SAMPLE),
                other => panic!("expected varchar, got {other:?}"),
            }
        });
    }

    /// Bulk-copying numeric and temporal values into encrypted columns encrypts
    /// each one on the client and round-trips it back transparently decrypted.
    #[tokio::test]
    async fn bulk_copy_roundtrips_numeric_and_temporal_types() {
        ae_test!(|h| {
            // bigint round-trip.
            let table = h
                .bulk_copy_vals(
                    "BIGINT",
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::BigInt(9_000_000_000_000_i64)],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            assert_eq!(rows[0][0], ColumnValues::BigInt(9_000_000_000_000_i64));

            // decimal(18,4) round-trip (1234.5678).
            let decimal = DecimalParts {
                is_positive: true,
                int_parts: vec![12_345_678],
                scale: 4,
                precision: 18,
            };
            let table = h
                .bulk_copy_vals(
                    "DECIMAL(18,4)",
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::Decimal(decimal.clone())],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::Decimal(v) => assert_eq!(v, &decimal),
                other => panic!("expected decimal, got {other:?}"),
            }

            // money round-trip (123.4567 stored as value * 10^4).
            let money = SqlMoney {
                lsb_part: 1_234_567,
                msb_part: 0,
            };
            let table = h
                .bulk_copy_vals(
                    "MONEY",
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::Money(money.clone())],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::Money(v) => {
                    assert_eq!(v.lsb_part, money.lsb_part);
                    assert_eq!(v.msb_part, money.msb_part);
                }
                other => panic!("expected money, got {other:?}"),
            }

            // datetime2(7) round-trip.
            let datetime2 = SqlDateTime2 {
                days: 730_119,
                time: SqlTime {
                    time_nanoseconds: 123_456_700,
                    scale: 7,
                },
            };
            let table = h
                .bulk_copy_vals(
                    "DATETIME2(7)",
                    "DETERMINISTIC",
                    "NOT NULL",
                    vec![ColumnValues::DateTime2(datetime2.clone())],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            match &rows[0][0] {
                ColumnValues::DateTime2(v) => assert_eq!(v, &datetime2),
                other => panic!("expected datetime2, got {other:?}"),
            }
        });
    }

    /// Bulk-copying a NULL into a nullable encrypted column stores SQL NULL (not
    /// an encrypted value), and it round-trips back as `Null`.
    #[tokio::test]
    async fn bulk_copy_encrypts_null_values() {
        ae_test!(|h| {
            let table = h
                .bulk_copy_vals(
                    "INT",
                    "DETERMINISTIC",
                    "NULL",
                    vec![
                        ColumnValues::Int(5),
                        ColumnValues::Null,
                        ColumnValues::Int(7),
                    ],
                )
                .await;
            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            assert_eq!(
                rows.into_iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
                vec![
                    ColumnValues::Int(5),
                    ColumnValues::Null,
                    ColumnValues::Int(7),
                ],
                "NULL must round-trip through an encrypted bulk-copy column"
            );
        });
    }

    /// With RANDOMIZED encryption, two identical plaintext values bulk-copied
    /// into the same encrypted column still decrypt correctly, but their stored
    /// ciphertext differs (a fresh IV is used per cell).
    #[tokio::test]
    async fn bulk_copy_randomized_encryption() {
        ae_test!(|h| {
            let table = h
                .bulk_copy_vals(
                    "INT",
                    "RANDOMIZED",
                    "NOT NULL",
                    vec![ColumnValues::Int(42), ColumnValues::Int(42)],
                )
                .await;

            let rows = h
                .query_rows(&format!("SELECT val FROM {table} ORDER BY id;"), 1)
                .await;
            assert_eq!(
                rows.into_iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
                vec![ColumnValues::Int(42), ColumnValues::Int(42)],
                "randomized values must still decrypt to the same plaintext"
            );

            let first = read_ciphertext(&table, 1).await;
            let second = read_ciphertext(&table, 2).await;
            assert_ne!(
                first, second,
                "randomized encryption must produce distinct ciphertext for equal plaintext"
            );
        });
    }

    /// A table with multiple encrypted columns (plus a plaintext key) bulk-copies
    /// correctly: every encrypted column is encrypted independently and all
    /// values round-trip.
    #[tokio::test]
    async fn bulk_copy_multiple_encrypted_columns() {
        ae_test!(|h| {
            let enc = h.enc_clause("DETERMINISTIC");
            let table = h
                .create_table(&format!(
                    "id INT NOT NULL, a INT {enc} NOT NULL, \
                     b NVARCHAR(50) COLLATE {BIN2_COLLATION} {enc} NULL"
                ))
                .await;

            let rows = vec![
                GenericBulkRow {
                    values: vec![
                        ColumnValues::Int(1),
                        ColumnValues::Int(100),
                        ColumnValues::String(SqlString::from_utf8_string("alpha".to_string())),
                    ],
                },
                GenericBulkRow {
                    values: vec![
                        ColumnValues::Int(2),
                        ColumnValues::Int(200),
                        ColumnValues::Null,
                    ],
                },
            ];
            let result = BulkCopy::new(&mut h.client, table.as_str())
                .batch_size(100)
                .write_to_server_zerocopy(rows)
                .await
                .expect("bulk copy into multiple encrypted columns");
            assert_eq!(result.rows_affected, 2);

            let got = h
                .query_rows(&format!("SELECT a, b FROM {table} ORDER BY id;"), 2)
                .await;
            assert_eq!(got[0][0], ColumnValues::Int(100));
            match &got[0][1] {
                ColumnValues::String(v) => assert_eq!(v.to_utf8_string(), "alpha"),
                other => panic!("expected nvarchar, got {other:?}"),
            }
            assert_eq!(got[1][0], ColumnValues::Int(200));
            assert_eq!(got[1][1], ColumnValues::Null);
        });
    }

    /// Bulk-copying more rows than the batch size into an encrypted column copies
    /// every row across multiple batches and each value round-trips.
    #[tokio::test]
    async fn bulk_copy_batches_multiple_rows() {
        ae_test!(|h| {
            let enc = h.enc_clause("DETERMINISTIC");
            let table = h
                .create_table(&format!("id INT NOT NULL, val INT {enc} NOT NULL"))
                .await;

            const ROW_COUNT: i32 = 250;
            let rows: Vec<GenericBulkRow> = (1..=ROW_COUNT)
                .map(|i| GenericBulkRow {
                    values: vec![ColumnValues::Int(i), ColumnValues::Int(i * 7)],
                })
                .collect();
            let result = BulkCopy::new(&mut h.client, table.as_str())
                .batch_size(50)
                .write_to_server_zerocopy(rows)
                .await
                .expect("bulk copy across multiple batches");
            assert_eq!(result.rows_affected, ROW_COUNT as u64);

            let got = h
                .query_rows(&format!("SELECT id, val FROM {table} ORDER BY id;"), 2)
                .await;
            assert_eq!(got.len(), ROW_COUNT as usize);
            assert_eq!(got[0], vec![ColumnValues::Int(1), ColumnValues::Int(7)]);
            assert_eq!(
                got[ROW_COUNT as usize - 1],
                vec![
                    ColumnValues::Int(ROW_COUNT),
                    ColumnValues::Int(ROW_COUNT * 7),
                ],
            );
        });
    }

    /// Abandoning a partially-read encrypted result set and then reusing the
    /// same connection must work. `close_query()` drains (decodes) every
    /// remaining encrypted row, and the memoized cell decryptor + per-column
    /// crypto metadata must be rebuilt for the next result set. This is the
    /// read-path analogue of the encrypted-output-parameter desync concern:
    /// abandoning a partially-read encrypted stream is exactly where the parser
    /// could desync.
    #[tokio::test]
    async fn abandon_partial_read_then_reuse_connection() {
        ae_test!(|h| {
            let enc = h.enc_clause("DETERMINISTIC");
            let table = h
                .create_table(&format!("id INT NOT NULL, val INT {enc} NOT NULL"))
                .await;

            const ROW_COUNT: i32 = 20;
            let rows: Vec<GenericBulkRow> = (1..=ROW_COUNT)
                .map(|i| GenericBulkRow {
                    values: vec![ColumnValues::Int(i), ColumnValues::Int(i * 7)],
                })
                .collect();
            BulkCopy::new(&mut h.client, table.as_str())
                .batch_size(100)
                .write_to_server_zerocopy(rows)
                .await
                .expect("bulk copy encrypted rows");

            // Read only the FIRST encrypted row of a multi-row result set, then
            // abandon the rest.
            h.client
                .execute(
                    format!("SELECT id, val FROM {table} ORDER BY id;"),
                    None,
                    None,
                )
                .await
                .expect("first select");
            let first = {
                let rs = h
                    .client
                    .get_current_resultset()
                    .expect("result set present");
                let row = rs
                    .next_row()
                    .await
                    .expect("read first row")
                    .expect("at least one row");
                vec![row[0].clone(), row[1].clone()]
            };
            assert_eq!(first, vec![ColumnValues::Int(1), ColumnValues::Int(7)]);

            // Abandon the partially-read encrypted stream. `close_query()` drains
            // and decodes every remaining encrypted row; it must stay in sync.
            h.client
                .close_query()
                .await
                .expect("drain abandoned encrypted stream");

            // Reuse the SAME connection for a second encrypted query and assert
            // every value still decrypts correctly — proving the decryptor and
            // per-column crypto metadata were rebuilt for the new COLMETADATA
            // rather than left over from the abandoned stream.
            let got = h
                .query_rows(&format!("SELECT id, val FROM {table} ORDER BY id;"), 2)
                .await;
            assert_eq!(got.len(), ROW_COUNT as usize);
            assert_eq!(got[0], vec![ColumnValues::Int(1), ColumnValues::Int(7)]);
            assert_eq!(
                got[ROW_COUNT as usize - 1],
                vec![
                    ColumnValues::Int(ROW_COUNT),
                    ColumnValues::Int(ROW_COUNT * 7),
                ],
            );
        });
    }

    /// Bulk-copying into an encrypted column over an Always Encrypted-disabled
    /// connection must fail: without encryption negotiated the client cannot
    /// produce ciphertext, and the server rejects the plaintext payload.
    #[tokio::test]
    async fn bulk_copy_with_ae_disabled_fails() {
        ae_test!(|h| {
            let enc = h.enc_clause("DETERMINISTIC");
            let table = h
                .create_table(&format!("id INT NOT NULL, val INT {enc} NOT NULL"))
                .await;

            let mut plain = connect_disabled().await;
            let rows = vec![GenericBulkRow {
                values: vec![ColumnValues::Int(1), ColumnValues::Int(111)],
            }];
            let result = BulkCopy::new(&mut plain, table.as_str())
                .batch_size(100)
                .write_to_server_zerocopy(rows)
                .await;
            let _ = plain.close_query().await;
            assert!(
                result.is_err(),
                "bulk copy into an encrypted column must fail with AE disabled, got {result:?}"
            );
        });
    }

    /// Bulk-copying into an encrypted column with a provider that cannot unwrap
    /// the CEK must fail while resolving the column encryption key.
    #[tokio::test]
    async fn bulk_copy_with_unregistered_key_fails() {
        ae_test!(|h| {
            let enc = h.enc_clause("DETERMINISTIC");
            let table = h
                .create_table(&format!("id INT NOT NULL, val INT {enc} NOT NULL"))
                .await;

            // Empty provider: no master key registered for the recorded path.
            let mut client = connect_enabled(Arc::new(RsaKeyStoreProvider::new())).await;
            let rows = vec![GenericBulkRow {
                values: vec![ColumnValues::Int(1), ColumnValues::Int(111)],
            }];
            let result = BulkCopy::new(&mut client, table.as_str())
                .batch_size(100)
                .write_to_server_zerocopy(rows)
                .await;
            let _ = client.close_query().await;
            assert!(
                result.is_err(),
                "bulk copy must fail when the CEK cannot be unwrapped, got {result:?}"
            );
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

    /// `ResultSetOnly` decrypts encrypted result columns while sending
    /// parameters unencrypted. This covers the result-set half: an encrypted
    /// column read under a per-command `ResultSetOnly` override still decrypts
    /// transparently.
    #[tokio::test]
    async fn result_set_only_decrypts_result_columns() {
        ae_test!(|h| {
            let table = h.create_encrypted_table("INT", "DETERMINISTIC").await;
            h.insert_encrypted(&table, SqlType::Int(Some(4242))).await;

            h.client
                .execute_sp_executesql_with_encryption_setting(
                    format!("SELECT val FROM {table};"),
                    vec![],
                    ExecutionColumnEncryptionSetting::ResultSetOnly,
                    None,
                    None,
                )
                .await
                .expect("select under ResultSetOnly");
            let (_meta, row) = get_first_row(&mut h.client)
                .await
                .expect("read row under ResultSetOnly");
            assert!(
                matches!(row[0], ColumnValues::Int(4242)),
                "ResultSetOnly must decrypt the result column, got {:?}",
                row[0]
            );
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

            let mut wrong_provider = RsaKeyStoreProvider::new();
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
            let mut client = connect_enabled(Arc::new(RsaKeyStoreProvider::new())).await;

            let result = select_val(&mut client, &table).await;
            let _ = client.close_query().await;
            assert!(
                result.is_err(),
                "decryption must fail when no master key is registered, got {result:?}"
            );
        });
    }
}
