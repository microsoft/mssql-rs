// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod bulk_copy_variant_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use async_trait::async_trait;
    use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow};
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::core::TdsResult;
    use mssql_tds::datatypes::column_values::{
        ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
        SqlSmallDateTime, SqlSmallMoney, SqlTime,
    };
    use mssql_tds::datatypes::decoder::DecimalParts;
    use mssql_tds::datatypes::sql_string::SqlString;
    use mssql_tds::datatypes::sql_vector::SqlVector;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    /// Test row with various data types for sql_variant column
    #[derive(Debug, Clone)]
    struct VariantRow {
        id: i32,
        variant_col: ColumnValues,
    }

    #[async_trait]
    impl BulkLoadRow for VariantRow {
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
                .write_column_value(*column_index, &self.variant_col)
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    #[async_trait]
    impl BulkLoadRow for &VariantRow {
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
                .write_column_value(*column_index, &self.variant_col)
                .await?;
            *column_index += 1;
            Ok(())
        }
    }

    /// Test integer types and bit in sql_variant
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_integers_and_bit() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #VariantIntTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(),
                (),
            )
            .await
            .unwrap();

        const TINYINT_VAL: u8 = 255;
        const SMALLINT_VAL: i16 = 32767;
        const INT_VAL: i32 = 2147483647;
        const BIGINT_VAL: i64 = 9223372036854775807;
        const BIT_TRUE: bool = true;
        const BIT_FALSE: bool = false;

        let rows = vec![
            VariantRow {
                id: 1,
                variant_col: ColumnValues::TinyInt(TINYINT_VAL),
            },
            VariantRow {
                id: 2,
                variant_col: ColumnValues::SmallInt(SMALLINT_VAL),
            },
            VariantRow {
                id: 3,
                variant_col: ColumnValues::Int(INT_VAL),
            },
            VariantRow {
                id: 4,
                variant_col: ColumnValues::BigInt(BIGINT_VAL),
            },
            VariantRow {
                id: 5,
                variant_col: ColumnValues::Bit(BIT_TRUE),
            },
            VariantRow {
                id: 6,
                variant_col: ColumnValues::Bit(BIT_FALSE),
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#VariantIntTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 6);

        // Verify the data
        client
            .execute(
                "SELECT id, variant_col FROM #VariantIntTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                assert_eq!(row[0], ColumnValues::Int(row_count));

                match row_count {
                    1 => assert_eq!(row[1], ColumnValues::TinyInt(TINYINT_VAL)),
                    2 => assert_eq!(row[1], ColumnValues::SmallInt(SMALLINT_VAL)),
                    3 => assert_eq!(row[1], ColumnValues::Int(INT_VAL)),
                    4 => assert_eq!(row[1], ColumnValues::BigInt(BIGINT_VAL)),
                    5 => assert_eq!(row[1], ColumnValues::Bit(BIT_TRUE)),
                    6 => assert_eq!(row[1], ColumnValues::Bit(BIT_FALSE)),
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 6);
    }

    /// Test float and money types in sql_variant
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_floats_and_money() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute("CREATE TABLE #VariantFloatMoneyTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(), ())
            .await
            .unwrap();

        const REAL_VAL: f32 = std::f32::consts::PI;
        const FLOAT_VAL: f64 = std::f64::consts::E;
        const MONEY_LSB: i32 = 1234567890;
        const MONEY_MSB: i32 = 0;
        const SMALLMONEY_VAL: i32 = 123456;

        let rows = vec![
            VariantRow {
                id: 1,
                variant_col: ColumnValues::Real(REAL_VAL),
            },
            VariantRow {
                id: 2,
                variant_col: ColumnValues::Float(FLOAT_VAL),
            },
            VariantRow {
                id: 3,
                variant_col: ColumnValues::Money(SqlMoney {
                    lsb_part: MONEY_LSB,
                    msb_part: MONEY_MSB,
                }),
            },
            VariantRow {
                id: 4,
                variant_col: ColumnValues::SmallMoney(SqlSmallMoney {
                    int_val: SMALLMONEY_VAL,
                }),
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#VariantFloatMoneyTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 4);

        // Verify the data
        client
            .execute(
                "SELECT id, variant_col FROM #VariantFloatMoneyTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                assert_eq!(row[0], ColumnValues::Int(row_count));

                match row_count {
                    1 => {
                        if let ColumnValues::Real(val) = row[1] {
                            assert!((val - REAL_VAL).abs() < 0.001);
                        } else {
                            panic!("Expected Real, got {:?}", row[1]);
                        }
                    }
                    2 => {
                        if let ColumnValues::Float(val) = row[1] {
                            assert!((val - FLOAT_VAL).abs() < 0.00001);
                        } else {
                            panic!("Expected Float, got {:?}", row[1]);
                        }
                    }
                    3 => {
                        if let ColumnValues::Money(money) = &row[1] {
                            assert_eq!(money.lsb_part, MONEY_LSB);
                            assert_eq!(money.msb_part, MONEY_MSB);
                        } else {
                            panic!("Expected Money, got {:?}", row[1]);
                        }
                    }
                    4 => {
                        if let ColumnValues::SmallMoney(sm) = &row[1] {
                            assert_eq!(sm.int_val, SMALLMONEY_VAL);
                        } else {
                            panic!("Expected SmallMoney, got {:?}", row[1]);
                        }
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 4);
    }

    /// Test decimal and numeric types in sql_variant
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_decimals() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #VariantDecimalTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(),
                (),
            )
            .await
            .unwrap();

        let decimal_val = DecimalParts {
            precision: 18,
            scale: 2,
            is_positive: true,
            int_parts: vec![123456],
        };

        let numeric_val = DecimalParts {
            precision: 10,
            scale: 3,
            is_positive: false,
            int_parts: vec![987654],
        };

        let rows = vec![
            VariantRow {
                id: 1,
                variant_col: ColumnValues::Decimal(decimal_val.clone()),
            },
            VariantRow {
                id: 2,
                variant_col: ColumnValues::Numeric(numeric_val.clone()),
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#VariantDecimalTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 2);

        // Verify the data
        client
            .execute(
                "SELECT id, variant_col FROM #VariantDecimalTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                assert_eq!(row[0], ColumnValues::Int(row_count));

                match row_count {
                    1 => {
                        if let ColumnValues::Decimal(dec) = &row[1] {
                            assert_eq!(dec.precision, decimal_val.precision);
                            assert_eq!(dec.scale, decimal_val.scale);
                            assert_eq!(dec.is_positive, decimal_val.is_positive);
                            // SQL Server may pad with zeros, so just check first element
                            assert_eq!(dec.int_parts[0], decimal_val.int_parts[0]);
                        } else {
                            panic!("Expected Decimal, got {:?}", row[1]);
                        }
                    }
                    2 => {
                        if let ColumnValues::Numeric(num) = &row[1] {
                            assert_eq!(num.precision, numeric_val.precision);
                            assert_eq!(num.scale, numeric_val.scale);
                            assert_eq!(num.is_positive, numeric_val.is_positive);
                            // SQL Server may pad with zeros, so just check first element
                            assert_eq!(num.int_parts[0], numeric_val.int_parts[0]);
                        } else {
                            panic!("Expected Numeric, got {:?}", row[1]);
                        }
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 2);
    }

    /// Test string and binary types in sql_variant
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_strings_and_bytes() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute("CREATE TABLE #VariantStringBytesTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(), ())
            .await
            .unwrap();

        const STRING_VAL: &str = "Hello, SQL_VARIANT!";
        const BYTES_VAL: &[u8] = &[0x48, 0x65, 0x6C, 0x6C, 0x6F];

        let rows = vec![
            VariantRow {
                id: 1,
                variant_col: ColumnValues::String(SqlString::from_utf8_string(
                    STRING_VAL.to_string(),
                )),
            },
            VariantRow {
                id: 2,
                variant_col: ColumnValues::Bytes(BYTES_VAL.to_vec()),
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#VariantStringBytesTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 2);

        // Verify the data
        client
            .execute(
                "SELECT id, variant_col FROM #VariantStringBytesTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                assert_eq!(row[0], ColumnValues::Int(row_count));

                match row_count {
                    1 => {
                        if let ColumnValues::String(s) = &row[1] {
                            assert_eq!(s.to_utf8_string(), STRING_VAL);
                        } else {
                            panic!("Expected String, got {:?}", row[1]);
                        }
                    }
                    2 => assert_eq!(row[1], ColumnValues::Bytes(BYTES_VAL.to_vec())),
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 2);
    }

    /// Test date and time types in sql_variant
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_datetime_types() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #VariantDateTimeTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(),
                (),
            )
            .await
            .unwrap();

        const DATE_DAYS: u32 = 738887;
        const TIME_SCALE: u8 = 7;
        const TIME_NANOS: u64 = 522000000000;
        const DATETIME_DAYS: i32 = 45655;
        const DATETIME_TIME: u32 = 15660000;
        const SMALLDT_DAYS: u16 = 45655;
        const SMALLDT_TIME: u16 = 870;
        const DTO_OFFSET: i16 = 0;

        let rows = vec![
            VariantRow {
                id: 1,
                variant_col: ColumnValues::Date(SqlDate::create(DATE_DAYS).unwrap()),
            },
            VariantRow {
                id: 2,
                variant_col: ColumnValues::Time(SqlTime {
                    scale: TIME_SCALE,
                    time_nanoseconds: TIME_NANOS,
                }),
            },
            VariantRow {
                id: 3,
                variant_col: ColumnValues::DateTime2(SqlDateTime2 {
                    days: DATE_DAYS,
                    time: SqlTime {
                        scale: TIME_SCALE,
                        time_nanoseconds: TIME_NANOS,
                    },
                }),
            },
            VariantRow {
                id: 4,
                variant_col: ColumnValues::DateTime(SqlDateTime {
                    days: DATETIME_DAYS,
                    time: DATETIME_TIME,
                }),
            },
            VariantRow {
                id: 5,
                variant_col: ColumnValues::SmallDateTime(SqlSmallDateTime {
                    days: SMALLDT_DAYS,
                    time: SMALLDT_TIME,
                }),
            },
            VariantRow {
                id: 6,
                variant_col: ColumnValues::DateTimeOffset(SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: DATE_DAYS,
                        time: SqlTime {
                            scale: TIME_SCALE,
                            time_nanoseconds: TIME_NANOS,
                        },
                    },
                    offset: DTO_OFFSET,
                }),
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#VariantDateTimeTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 6);

        // Verify the data
        client
            .execute(
                "SELECT id, variant_col FROM #VariantDateTimeTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                assert_eq!(row[0], ColumnValues::Int(row_count));

                match row_count {
                    1 => {
                        if let ColumnValues::Date(d) = &row[1] {
                            assert_eq!(d.get_days(), DATE_DAYS);
                        } else {
                            panic!("Expected Date, got {:?}", row[1]);
                        }
                    }
                    2 => {
                        if let ColumnValues::Time(t) = &row[1] {
                            assert_eq!(t.scale, TIME_SCALE);
                            assert_eq!(t.time_nanoseconds, TIME_NANOS);
                        } else {
                            panic!("Expected Time, got {:?}", row[1]);
                        }
                    }
                    3 => {
                        if let ColumnValues::DateTime2(dt2) = &row[1] {
                            assert_eq!(dt2.days, DATE_DAYS);
                            assert_eq!(dt2.time.scale, TIME_SCALE);
                            assert_eq!(dt2.time.time_nanoseconds, TIME_NANOS);
                        } else {
                            panic!("Expected DateTime2, got {:?}", row[1]);
                        }
                    }
                    4 => {
                        if let ColumnValues::DateTime(dt) = &row[1] {
                            assert_eq!(dt.days, DATETIME_DAYS);
                            assert_eq!(dt.time, DATETIME_TIME);
                        } else {
                            panic!("Expected DateTime, got {:?}", row[1]);
                        }
                    }
                    5 => {
                        if let ColumnValues::SmallDateTime(sdt) = &row[1] {
                            assert_eq!(sdt.days, SMALLDT_DAYS);
                            assert_eq!(sdt.time, SMALLDT_TIME);
                        } else {
                            panic!("Expected SmallDateTime, got {:?}", row[1]);
                        }
                    }
                    6 => {
                        if let ColumnValues::DateTimeOffset(dto) = &row[1] {
                            assert_eq!(dto.datetime2.days, DATE_DAYS);
                            assert_eq!(dto.datetime2.time.scale, TIME_SCALE);
                            assert_eq!(dto.datetime2.time.time_nanoseconds, TIME_NANOS);
                            assert_eq!(dto.offset, DTO_OFFSET);
                        } else {
                            panic!("Expected DateTimeOffset, got {:?}", row[1]);
                        }
                    }
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 6);
    }

    /// Test UUID and NULL in sql_variant
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_uuid_and_null() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute(
                "CREATE TABLE #VariantUuidNullTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(),
                (),
            )
            .await
            .unwrap();

        const UUID_STR: &str = "550e8400-e29b-41d4-a716-446655440000";
        let uuid_val = uuid::Uuid::parse_str(UUID_STR).unwrap();

        let rows = vec![
            VariantRow {
                id: 1,
                variant_col: ColumnValues::Uuid(uuid_val),
            },
            VariantRow {
                id: 2,
                variant_col: ColumnValues::Null,
            },
        ];

        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#VariantUuidNullTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 2);

        // Verify the data
        client
            .execute(
                "SELECT id, variant_col FROM #VariantUuidNullTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;
                assert_eq!(row[0], ColumnValues::Int(row_count));

                match row_count {
                    1 => {
                        if let ColumnValues::Uuid(uuid) = &row[1] {
                            assert_eq!(uuid.to_string(), UUID_STR);
                        } else {
                            panic!("Expected Uuid, got {:?}", row[1]);
                        }
                    }
                    2 => assert_eq!(row[1], ColumnValues::Null),
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 2);
    }

    /// Test bulk copy with unsupported type (Vector) - should fail
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_unsupported_type() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with sql_variant column
        client
            .execute("CREATE TABLE #BulkCopyVariantUnsupportedTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(), ())
            .await
            .unwrap();

        // Try to insert Vector type (not supported in sql_variant)
        let rows = vec![VariantRow {
            id: 1,
            variant_col: ColumnValues::Vector(
                SqlVector::try_from_f32(vec![1.0, 2.0, 3.0]).unwrap(),
            ),
        }];

        // Execute bulk copy - should fail
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVariantUnsupportedTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
        };

        // Expect error
        assert!(
            result.is_err(),
            "Expected error when inserting Vector into sql_variant"
        );

        if let Err(e) = result {
            let error_msg = e.to_string();
            assert!(
                error_msg.contains("Unsupported data type"),
                "Expected error message about unsupported type, got: {}",
                error_msg
            );
        }
    }

    /// Test bulk copy with edge cases and complex values
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_edge_cases() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        // Create temp table with sql_variant column
        client
            .execute("CREATE TABLE #BulkCopyVariantEdgeCaseTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(), ())
            .await
            .unwrap();

        // Create test data with edge cases
        let rows = vec![
            // Min/Max integer values
            VariantRow {
                id: 1,
                variant_col: ColumnValues::TinyInt(0), // Min TinyInt
            },
            VariantRow {
                id: 2,
                variant_col: ColumnValues::SmallInt(-32768), // Min SmallInt
            },
            VariantRow {
                id: 3,
                variant_col: ColumnValues::Int(-2147483648), // Min Int
            },
            VariantRow {
                id: 4,
                variant_col: ColumnValues::BigInt(-9223372036854775808), // Min BigInt
            },
            // Special float values
            VariantRow {
                id: 5,
                variant_col: ColumnValues::Real(0.0), // Zero
            },
            VariantRow {
                id: 6,
                variant_col: ColumnValues::Real(-0.0), // Negative zero
            },
            VariantRow {
                id: 7,
                variant_col: ColumnValues::Float(f64::MIN_POSITIVE), // Smallest positive
            },
            VariantRow {
                id: 8,
                variant_col: ColumnValues::Float(f64::MAX), // Maximum value
            },
            // Empty and special strings
            VariantRow {
                id: 9,
                variant_col: ColumnValues::String(SqlString::from_utf8_string(String::new())), // Empty string
            },
            VariantRow {
                id: 10,
                variant_col: ColumnValues::String(SqlString::from_utf8_string(
                    "Unicode: 你好世界 🌍".to_string(),
                )), // Unicode
            },
            VariantRow {
                id: 11,
                variant_col: ColumnValues::String(SqlString::from_utf8_string(
                    "Special: \t\n\r".to_string(),
                )), // Special chars
            },
            // Empty binary
            VariantRow {
                id: 12,
                variant_col: ColumnValues::Bytes(vec![]),
            },
            // Large binary (1KB)
            VariantRow {
                id: 13,
                variant_col: ColumnValues::Bytes(vec![0xAA; 1024]),
            },
            // Decimal with zero
            VariantRow {
                id: 14,
                variant_col: ColumnValues::Decimal(DecimalParts {
                    precision: 18,
                    scale: 2,
                    is_positive: true,
                    int_parts: vec![0],
                }),
            },
            // Negative decimal
            VariantRow {
                id: 15,
                variant_col: ColumnValues::Decimal(DecimalParts {
                    precision: 18,
                    scale: 4,
                    is_positive: false,
                    int_parts: vec![999999],
                }),
            },
            // High precision decimal
            VariantRow {
                id: 16,
                variant_col: ColumnValues::Decimal(DecimalParts {
                    precision: 38,
                    scale: 10,
                    is_positive: true,
                    int_parts: vec![i32::MAX, i32::MAX],
                }),
            },
            // Date edge cases
            VariantRow {
                id: 17,
                variant_col: ColumnValues::Date(SqlDate::create(0).unwrap()), // Min date 0001-01-01
            },
            VariantRow {
                id: 18,
                variant_col: ColumnValues::Date(SqlDate::create(3652058).unwrap()), // Max date 9999-12-31
            },
            // Time edge cases
            VariantRow {
                id: 19,
                variant_col: ColumnValues::Time(SqlTime {
                    scale: 7,
                    time_nanoseconds: 0, // Midnight 00:00:00
                }),
            },
            VariantRow {
                id: 20,
                variant_col: ColumnValues::Time(SqlTime {
                    scale: 7,
                    time_nanoseconds: 863999999999, // 23:59:59.9999999
                }),
            },
            // Bit false
            VariantRow {
                id: 21,
                variant_col: ColumnValues::Bit(false),
            },
        ];

        // Execute bulk copy
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#BulkCopyVariantEdgeCaseTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
                .expect("Bulk copy failed")
        };
        assert_eq!(result.rows_affected, 21, "Expected 21 rows to be inserted");

        // Verify the data
        client
            .execute(
                "SELECT id, variant_col FROM #BulkCopyVariantEdgeCaseTest ORDER BY id".to_string(),
                (),
            )
            .await
            .expect("Failed to select data");

        let mut row_count = 0;
        if let Some(resultset) = client.get_current_resultset() {
            while let Some(row) = resultset.next_row().await.expect("Failed to read row") {
                row_count += 1;

                // Verify ID
                assert_eq!(row[0], ColumnValues::Int(row_count));

                // Verify variant values
                match row_count {
                    1 => assert_eq!(row[1], ColumnValues::TinyInt(0)),
                    2 => assert_eq!(row[1], ColumnValues::SmallInt(-32768)),
                    3 => assert_eq!(row[1], ColumnValues::Int(-2147483648)),
                    4 => assert_eq!(row[1], ColumnValues::BigInt(-9223372036854775808)),
                    5 => {
                        if let ColumnValues::Real(val) = row[1] {
                            assert_eq!(val, 0.0);
                        } else {
                            panic!("Expected Real(0.0), got {:?}", row[1]);
                        }
                    }
                    6 => {
                        // Negative zero might be normalized to positive zero
                        if let ColumnValues::Real(val) = row[1] {
                            assert_eq!(val.abs(), 0.0);
                        } else {
                            panic!("Expected Real, got {:?}", row[1]);
                        }
                    }
                    7 | 8 => {
                        // Just verify it's a float
                        assert!(matches!(row[1], ColumnValues::Float(_)));
                    }
                    9 => {
                        if let ColumnValues::String(s) = &row[1] {
                            assert_eq!(s.to_utf8_string(), "");
                        } else {
                            panic!("Expected empty String, got {:?}", row[1]);
                        }
                    }
                    10 => {
                        if let ColumnValues::String(s) = &row[1] {
                            assert_eq!(s.to_utf8_string(), "Unicode: 你好世界 🌍");
                        } else {
                            panic!("Expected Unicode String, got {:?}", row[1]);
                        }
                    }
                    11 => {
                        if let ColumnValues::String(s) = &row[1] {
                            assert_eq!(s.to_utf8_string(), "Special: \t\n\r");
                        } else {
                            panic!("Expected String with special chars, got {:?}", row[1]);
                        }
                    }
                    12 => {
                        assert_eq!(row[1], ColumnValues::Bytes(vec![]));
                    }
                    13 => {
                        assert_eq!(row[1], ColumnValues::Bytes(vec![0xAA; 1024]));
                    }
                    14..=16 => {
                        // Just verify it's a decimal
                        assert!(matches!(row[1], ColumnValues::Decimal(_)));
                    }
                    17 | 18 => {
                        // Just verify it's a date
                        assert!(matches!(row[1], ColumnValues::Date(_)));
                    }
                    19 | 20 => {
                        // Just verify it's a time
                        assert!(matches!(row[1], ColumnValues::Time(_)));
                    }
                    21 => assert_eq!(row[1], ColumnValues::Bit(false)),
                    _ => panic!("Unexpected row count: {}", row_count),
                }
            }
        }
        assert_eq!(row_count, 21, "Expected 21 rows in result set");
    }

    /// Test that oversized strings are rejected for SQL_VARIANT
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bulk_copy_variant_oversized_string() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute("CREATE TABLE #VariantOversizedTest (id INT NOT NULL, variant_col SQL_VARIANT NULL)"
                    .to_string(), ())
            .await
            .unwrap();

        // Create a string that exceeds 8000 bytes (4001 characters * 2 bytes = 8002 bytes)
        let large_string = "A".repeat(4001);

        let rows = vec![VariantRow {
            id: 1,
            variant_col: ColumnValues::String(SqlString::from_utf8_string(large_string)),
        }];

        // This should fail with an error about exceeding SQL_VARIANT maximum size
        let result = {
            let bulk_copy = BulkCopy::new(&mut client, "#VariantOversizedTest");
            bulk_copy
                .batch_size(1000)
                .write_to_server_zerocopy(&rows)
                .await
        };

        assert!(
            result.is_err(),
            "Expected error for oversized string in SQL_VARIANT"
        );

        let error_message = result.unwrap_err().to_string();
        assert!(
            error_message.contains("Sql Variant")
                || error_message.contains("SQL_VARIANT")
                || error_message.contains("exceeds"),
            "Expected error message about SQL_VARIANT validation, got: {}",
            error_message
        );
    }
}
