// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod query_result_reads {
    use crate::common::{
        ExpectedQueryResultType, begin_connection, build_tcp_datasource,
        connect_query_and_validate, run_query_and_check_results,
    };
    use mssql_tds::connection::tds_client::ResultSet;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::error::Error::{SqlServerError, UsageError};

    use crate::common::init_tracing;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_select_1() {
        let expected = [ExpectedQueryResultType::Result(1)];
        connect_query_and_validate("SELECT 1".to_string(), &expected).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_generate_all_types_table() {
        let expected = [
            ExpectedQueryResultType::Update(0),
            ExpectedQueryResultType::Update(2),
            ExpectedQueryResultType::Result(2),
        ];
        connect_query_and_validate(
            "
            CREATE TABLE #AllDataTypes (
                TinyIntColumn TINYINT,
                SmallIntColumn SMALLINT,
                IntColumn INT,
                BigIntColumn BIGINT,
                BitColumn BIT,
                DecimalColumn DECIMAL(18,2),
                NumericColumn NUMERIC(18,2),
                FloatColumn FLOAT,
                RealColumn REAL,
                NCharColumn NCHAR(50),
                NTextColumn NTEXT
            );

            INSERT INTO #AllDataTypes (
                TinyIntColumn, SmallIntColumn, IntColumn, BigIntColumn, BitColumn,
                DecimalColumn, NumericColumn, FloatColumn, RealColumn,
                NCharColumn, NTextColumn
            )
            VALUES (
                CAST(255 AS TINYINT), -- TinyIntColumn
                CAST(32767 AS SMALLINT), -- SmallIntColumn
                CAST(2147483647 AS INT), -- IntColumn
                CAST(9223372036854775807 AS BIGINT), -- BigIntColumn
                CAST(1 AS BIT), -- BitColumn
                CAST(272.01 AS DECIMAL(18, 2)), --DecimalColumn
                CAST(12345678901234.98 AS NUMERIC(18,2)), -- NumericColumn
                CAST(1234.22231 AS FLOAT), -- FloatColumn
                CAST(11.11 AS REAL), -- RealColumn
                N'Hello 世界 🌍', -- NCharColumn with Unicode
                CAST(N'NTEXT data with Unicode: Привет мир' AS NTEXT) -- NTextColumn
            ),
            (
                CAST(128 AS TINYINT), -- TinyIntColumn
                CAST(128 AS SMALLINT), -- SmallIntColumn
                CAST(128 AS INT), -- IntColumn
                CAST(128 AS BIGINT), -- BigIntColumn
                CAST(0 AS BIT), -- BitColumn
                CAST(19.01 AS DECIMAL(18, 2)), --DecimalColumn
                CAST(18.98 AS NUMERIC(18,2)), -- NumericColumn
                CAST(100.22231 AS FLOAT), -- FloatColumn
                CAST(5.11 AS REAL), -- RealColumn
                N'', -- NCharColumn with empty string
                CAST(N'' AS NTEXT) -- NTextColumn with empty string (tests empty string fix)
            );

            select * from #AllDataTypes;"
                .to_string(),
            &expected,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ntext_and_nchar_types() {
        // Dedicated test for NTEXT and NCHAR with edge cases including:
        // - Empty strings (tests the decoder fix for textptr_len > 0 but data_length == 0)
        // - Unicode characters (tests UTF-16LE encoding)
        // - NULL values
        // - Large text for NTEXT
        let expected = [
            ExpectedQueryResultType::Update(0),
            ExpectedQueryResultType::Update(5),
            ExpectedQueryResultType::Result(5),
        ];
        connect_query_and_validate(
            "
            CREATE TABLE #NTextNCharTest (
                id INT PRIMARY KEY,
                nchar_col NCHAR(50),
                ntext_col NTEXT,
                description VARCHAR(100)
            );

            INSERT INTO #NTextNCharTest (id, nchar_col, ntext_col, description)
            VALUES 
                (1, N'Hello 世界', CAST(N'NTEXT with Unicode: Привет мир 🌍' AS NTEXT), 'Unicode test'),
                (2, N'', CAST(N'' AS NTEXT), 'Empty string test'),
                (3, NULL, NULL, 'NULL test'),
                (4, N'Spaces   ', CAST(N'Trailing spaces   ' AS NTEXT), 'Whitespace test'),
                (5, N'emoji 🚀', CAST(N'Long NTEXT: ' + REPLICATE(N'A', 1000) AS NTEXT), 'Large text test');

            SELECT id, nchar_col, ntext_col, description FROM #NTextNCharTest ORDER BY id;"
                .to_string(),
            &expected,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_ntext_nchar_values_validation() {
        // Test that validates the actual values read from NTEXT and NCHAR columns
        // This ensures proper UTF-16LE decoding and empty string handling
        let datasource = build_tcp_datasource();
        let mut connection = begin_connection(&datasource).await;

        connection
            .execute(
                "
                CREATE TABLE #NTextValidation (
                    id INT PRIMARY KEY,
                    nchar_val NCHAR(20),
                    ntext_val NTEXT
                );

                INSERT INTO #NTextValidation (id, nchar_val, ntext_val)
                VALUES 
                    (1, N'Test', CAST(N'NTEXT Value' AS NTEXT)),
                    (2, N'', CAST(N'' AS NTEXT)),
                    (3, N'Unicode 世界', CAST(N'Мир 🌍' AS NTEXT));

                SELECT id, nchar_val, ntext_val FROM #NTextValidation ORDER BY id;"
                    .to_string(),
                (),
            )
            .await
            .unwrap();

        // Collect all result sets
        let mut all_rows = Vec::new();
        loop {
            if let Some(resultset) = connection.get_current_resultset() {
                while let Some(row) = resultset.next_row().await.unwrap() {
                    all_rows.push(row);
                }
            }
            if !connection.advance_to_rows().await.unwrap() {
                break;
            }
        }

        // Should have 3 rows from the SELECT
        assert_eq!(all_rows.len(), 3, "Should have 3 rows");

        // Row 1: Regular text
        if let ColumnValues::Int(id) = &all_rows[0][0] {
            assert_eq!(*id, 1);
        }
        if let ColumnValues::String(nchar_val) = &all_rows[0][1] {
            let s = nchar_val.to_utf8_string();
            assert!(
                s.starts_with("Test"),
                "NCHAR should start with 'Test', got: '{}'",
                s
            );
        }
        if let ColumnValues::String(ntext_val) = &all_rows[0][2] {
            assert_eq!(ntext_val.to_utf8_string(), "NTEXT Value");
        }

        // Row 2: Empty strings (tests the decoder fix)
        if let ColumnValues::Int(id) = &all_rows[1][0] {
            assert_eq!(*id, 2);
        }
        if let ColumnValues::String(nchar_val) = &all_rows[1][1] {
            let s = nchar_val.to_utf8_string();
            // NCHAR pads with spaces, so empty string becomes spaces
            assert!(
                s.chars().all(|c| c.is_whitespace() || c == '\0'),
                "NCHAR empty should be whitespace/null, got: '{}'",
                s
            );
        }
        if let ColumnValues::String(ntext_val) = &all_rows[1][2] {
            // NTEXT empty string should be truly empty (the fix we made)
            assert_eq!(
                ntext_val.to_utf8_string(),
                "",
                "NTEXT empty string should be empty, not NULL"
            );
        }

        // Row 3: Unicode text
        if let ColumnValues::Int(id) = &all_rows[2][0] {
            assert_eq!(*id, 3);
        }
        if let ColumnValues::String(nchar_val) = &all_rows[2][1] {
            let s = nchar_val.to_utf8_string();
            assert!(
                s.contains("Unicode 世界"),
                "NCHAR should contain Unicode, got: '{}'",
                s
            );
        }
        if let ColumnValues::String(ntext_val) = &all_rows[2][2] {
            let s = ntext_val.to_utf8_string();
            assert_eq!(s, "Мир 🌍", "NTEXT should contain Cyrillic and emoji");
        }

        connection.close_query().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_tds_connection_reuse() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        let expected = [
            ExpectedQueryResultType::Update(0),
            ExpectedQueryResultType::Update(2),
            ExpectedQueryResultType::Result(2),
        ];
        run_query_and_check_results(
            &mut connection,
            "
            CREATE TABLE #dummy (
                IntColumn INT
            );
            INSERT INTO #dummy VALUES(10),(20);
            SELECT * FROM #dummy;"
                .to_string(),
            &expected,
        )
        .await;

        let expected = [
            ExpectedQueryResultType::Update(0),
            ExpectedQueryResultType::Update(0),
            ExpectedQueryResultType::Update(3),
            ExpectedQueryResultType::Result(3),
        ];
        run_query_and_check_results(
            &mut connection,
            "DROP TABLE #dummy;
            CREATE TABLE #dummy (
                ShortColumn SMALLINT
            );
            INSERT INTO #dummy VALUES(0),(1),(2);
            SELECT * FROM #dummy;"
                .to_string(),
            &expected,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_result_sets_with_dml_and_selects() {
        // This test matches the JavaScript test that's currently failing.
        // It tests the pattern: DML operations (CREATE TABLE, INSERT) followed by multiple SELECTs.
        // This should return 5 result sets total.

        let mut connection = begin_connection(&build_tcp_datasource()).await;

        {
            // This is the EXACT query from the failing JavaScript test
            connection
                .execute(
                    "
                    CREATE TABLE #dummy (
                        IntColumn INT
                    );
                    INSERT INTO #dummy VALUES(10),(20);
                    SELECT * FROM #dummy;
                    SELECT 1;
                    SELECT * FROM #dummy;
                    "
                    .to_string(),
                    (),
                )
                .await
                .unwrap();

            // SAFE PATTERN: Collect ALL result sets upfront (JavaScript pattern)
            let mut all_result_sets = Vec::new();
            // `execute()` positions on the first navigable result statement-wise;
            // the leading INSERT surfaces its row count first, so collapse to the
            // first row-returning result set before collecting.
            if !connection.on_rows() {
                connection.advance_to_rows().await.unwrap();
            }
            loop {
                let mut current_result_rows = Vec::new();

                // Fully consume current result set
                if let Some(resultset) = connection.get_current_resultset() {
                    while let Some(row) = resultset.next_row().await.unwrap() {
                        current_result_rows.push(row);
                    }
                }

                all_result_sets.push(current_result_rows);

                // Try to move to next result set
                if !connection.advance_to_rows().await.unwrap() {
                    break; // No more result sets
                }
            }

            // Verify the collected data - should have 3 result sets (the 3 SELECTs)
            // Note: CREATE TABLE and INSERT are DML operations without column metadata,
            // so they don't appear as separate result sets. This matches SQL Server behavior.
            assert_eq!(
                all_result_sets.len(),
                3,
                "Should have 3 result sets (3 SELECTs only)"
            );

            // Result set 0: First SELECT * (2 rows)
            assert_eq!(
                all_result_sets[0].len(),
                2,
                "First SELECT should have 2 rows"
            );
            if let ColumnValues::Int(val) = &all_result_sets[0][0][0] {
                assert_eq!(*val, 10, "First row should be 10");
            }
            if let ColumnValues::Int(val) = &all_result_sets[0][1][0] {
                assert_eq!(*val, 20, "Second row should be 20");
            }

            // Result set 1: SELECT 1 (1 row)
            assert_eq!(all_result_sets[1].len(), 1, "SELECT 1 should have 1 row");
            if let ColumnValues::Int(val) = &all_result_sets[1][0][0] {
                assert_eq!(*val, 1, "SELECT 1 should return 1");
            }

            // Result set 2: Final SELECT * (2 rows)
            assert_eq!(
                all_result_sets[2].len(),
                2,
                "Final SELECT should have 2 rows"
            );
            if let ColumnValues::Int(val) = &all_result_sets[2][0][0] {
                assert_eq!(*val, 10, "First row should be 10");
            }
            if let ColumnValues::Int(val) = &all_result_sets[2][1][0] {
                assert_eq!(*val, 20, "Second row should be 20");
            }

            connection.close_query().await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_result_sets_selects_only() {
        // Simpler test with just SELECTs (no DML) - this should already work

        let mut connection = begin_connection(&build_tcp_datasource()).await;

        {
            // Use a query similar to JavaScript tests: multiple SELECTs only
            connection
                .execute("SELECT 1, 2; SELECT 10, 20, 30;".to_string(), ())
                .await
                .unwrap();

            // SAFE PATTERN: Collect ALL result sets upfront (JavaScript pattern)
            let mut all_result_sets = Vec::new();
            loop {
                let mut current_result_rows = Vec::new();

                // Fully consume current result set
                if let Some(resultset) = connection.get_current_resultset() {
                    while let Some(row) = resultset.next_row().await.unwrap() {
                        current_result_rows.push(row);
                    }
                }

                all_result_sets.push(current_result_rows);

                // Try to move to next result set
                if !connection.advance_to_rows().await.unwrap() {
                    break; // No more result sets
                }
            }

            // Verify the collected data
            assert_eq!(
                all_result_sets.len(),
                2,
                "Should have 2 result sets (two SELECTs)"
            );

            // Result set 0: SELECT 1, 2 (1 row with 2 columns)
            assert_eq!(
                all_result_sets[0].len(),
                1,
                "First SELECT should have 1 row"
            );
            assert_eq!(
                all_result_sets[0][0].len(),
                2,
                "First row should have 2 columns"
            );
            if let ColumnValues::Int(val) = &all_result_sets[0][0][0] {
                assert_eq!(*val, 1, "First column should be 1");
            }
            if let ColumnValues::Int(val) = &all_result_sets[0][0][1] {
                assert_eq!(*val, 2, "Second column should be 2");
            }

            // Result set 1: SELECT 10, 20, 30 (1 row with 3 columns)
            assert_eq!(
                all_result_sets[1].len(),
                1,
                "Second SELECT should have 1 row"
            );
            assert_eq!(
                all_result_sets[1][0].len(),
                3,
                "Second row should have 3 columns"
            );
            if let ColumnValues::Int(val) = &all_result_sets[1][0][0] {
                assert_eq!(*val, 10, "First column should be 10");
            }
            if let ColumnValues::Int(val) = &all_result_sets[1][0][1] {
                assert_eq!(*val, 20, "Second column should be 20");
            }
            if let ColumnValues::Int(val) = &all_result_sets[1][0][2] {
                assert_eq!(*val, 30, "Third column should be 30");
            }

            connection.close_query().await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_incomplete_result_iteration() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        {
            connection
                .execute(
                    "
                CREATE TABLE #dummy (
                    IntColumn INT
                );
                INSERT INTO #dummy VALUES(10),(20);
                SELECT * FROM #dummy;"
                        .to_string(),
                    (),
                )
                .await
                .unwrap();

            // Just get the first result set, then close
            let _result_number = 0;
            if connection.get_current_resultset().is_some() {
                // Found first result, now close without consuming
            }
            connection.close_query().await.unwrap();
        }

        // Try to reuse the connection.
        let expected = [
            ExpectedQueryResultType::Update(0),
            ExpectedQueryResultType::Update(0),
            ExpectedQueryResultType::Update(3),
            ExpectedQueryResultType::Result(3),
        ];
        run_query_and_check_results(
            &mut connection,
            "DROP TABLE #dummy;
            CREATE TABLE #dummy (
                ShortColumn SMALLINT
            );
            INSERT INTO #dummy VALUES(0),(1),(2);
            SELECT * FROM #dummy;"
                .to_string(),
            &expected,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_error_missed_close_result_iteration() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        {
            connection
                .execute(
                    "
                CREATE TABLE #dummy (
                    IntColumn INT
                );
                INSERT INTO #dummy VALUES(10),(20);
                SELECT * FROM #dummy;"
                        .to_string(),
                    (),
                )
                .await
                .unwrap();

            // Just get the first result without closing
            let _result_number = 0;
            if connection.get_current_resultset().is_some() {
                // Found first result, exit scope without closing
            }
        }

        // Try to reuse the connection - should fail because previous query wasn't closed
        let expected_error = connection.execute("SELECT 1".to_string(), ()).await;
        match expected_error {
            Ok(_) => panic!("Expected error but got success."),
            Err(UsageError(_)) => {
                // Success case - got expected UsageError
            }
            Err(err) => panic!("Expected error but got different error: {err}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_error_missed_close_result_set() {
        // NOTE: TdsClient has different behavior than the streaming QueryResult API.
        // TdsClient automatically drains (consumes) remaining rows when move_to_next() is called,
        // so there's no error for incomplete result set consumption.
        // This test verifies that TdsClient handles incomplete consumption gracefully.
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        {
            connection
                .execute(
                    "
                SELECT 1 UNION ALL SELECT 2;
                SELECT 2;"
                        .to_string(),
                    (),
                )
                .await
                .unwrap();

            // Get the first result set and read one row
            if let Some(resultset) = connection.get_current_resultset() {
                // Get the first row and explicitly don't finish consuming the result set
                let row = resultset.next_row().await.unwrap();
                assert!(row.is_some());
            }

            // With TdsClient, move_to_next() automatically drains remaining rows - this should succeed
            let second_result = connection.advance_to_rows().await;
            assert!(second_result.is_ok());
            assert!(second_result.unwrap()); // Should return true as there is a next result set

            // Verify we can read from the second result set
            if let Some(resultset) = connection.get_current_resultset() {
                let row = resultset.next_row().await.unwrap();
                assert!(row.is_some());
            }

            connection.close_query().await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_error_within_result_set() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        {
            connection
                .execute(
                    "
                CREATE TABLE #dummy (
                    StrColumn VARCHAR(100)
                );
                INSERT INTO #dummy VALUES('10'),('abcd');
                SELECT CAST(StrColumn AS Int) FROM #dummy;"
                        .to_string(),
                    (),
                )
                .await
                .unwrap();

            // Try to skip the first result (CREATE TABLE)
            // The error from the later SELECT CAST might appear at any point
            let mut error_found = false;
            let first_move = connection.advance_to_rows().await;

            if first_move.is_err() {
                // Error encountered on first move
                match first_move {
                    Err(SqlServerError { .. }) => {
                        error_found = true;
                    }
                    Err(e) => panic!("Expected SqlServerError, got: {e:?}"),
                    Ok(_) => unreachable!(),
                }
            }

            // If no error yet, try moving past INSERT
            let move_result = if !error_found {
                connection.advance_to_rows().await
            } else {
                // Already found error, skip remaining checks
                return;
            };

            match move_result {
                Err(SqlServerError { .. }) => {
                    // Expected: Error occurred during move_to_next when moving to SELECT result
                    error_found = true;
                }
                Ok(_) => {
                    // move_to_next succeeded, error should appear during row iteration
                    if let Some(resultset) = connection.get_current_resultset() {
                        // Try to read first row
                        let first_row = resultset.next_row().await;
                        match first_row {
                            Ok(Some(_)) => {
                                // First row succeeded (CAST('10' AS Int)), second row should error
                                let row_result = resultset.next_row().await;
                                match row_result {
                                    Err(SqlServerError { .. }) => {
                                        error_found = true;
                                    }
                                    _ => panic!("Expected SqlServerError on second row"),
                                }
                            }
                            Err(SqlServerError { .. }) => {
                                // Error occurred on first row attempt
                                error_found = true;
                            }
                            _ => panic!("Expected success or SqlServerError"),
                        }
                    }
                }
                Err(e) => panic!("Expected SqlServerError, got: {e:?}"),
            }

            assert!(error_found, "Expected to encounter a SqlServerError");

            connection.close_query().await.unwrap();
        }

        // Make sure the connection is still usable.
        let expected = [ExpectedQueryResultType::Result(1)];
        run_query_and_check_results(&mut connection, "SELECT 1".to_string(), &expected).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_error_within_batch() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        {
            // Note: The INSERT with 1/0 will cause a divide by zero error during execution
            let execute_result = connection
                .execute(
                    "
                CREATE TABLE #dummy (
                    IntColumn VARCHAR(100)
                );
                INSERT INTO #dummy VALUES(1/0),(10);
                SELECT CAST(StrColumn AS Int) FROM #dummy;"
                        .to_string(),
                    (),
                )
                .await;

            // The error might occur during execute() or during result iteration
            match execute_result {
                Ok(_) => {
                    // If execute succeeded, the error should appear when we try to move to results
                    // Skip the first result (CREATE TABLE)
                    let first_move = connection.advance_to_rows().await;
                    match first_move {
                        Err(SqlServerError { .. }) => {
                            // Expected error occurred on first move
                        }
                        Err(e) => panic!("Expected SqlServerError, got: {e:?}"),
                        Ok(_) => {
                            // First move succeeded, error should occur on second move (INSERT result)
                            let error_result = connection.advance_to_rows().await;
                            match error_result {
                                Err(SqlServerError { .. }) => {
                                    // Expected error
                                }
                                Err(e) => panic!("Expected SqlServerError, got: {e:?}"),
                                Ok(_) => panic!("Expected a SqlServerError but got success"),
                            }
                        }
                    }
                }
                Err(SqlServerError { .. }) => {
                    // Error occurred during execute(), which is also acceptable
                }
                Err(e) => panic!("Expected SqlServerError, got: {e:?}"),
            }

            connection.close_query().await.unwrap();
        }

        // Make sure the connection is still usable.
        let expected = [ExpectedQueryResultType::Result(1)];
        run_query_and_check_results(&mut connection, "SELECT 1".to_string(), &expected).await;
    }
}
