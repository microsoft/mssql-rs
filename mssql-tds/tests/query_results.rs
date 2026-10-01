// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod query_result_reads {
    use crate::common::{
        ExpectedQueryResultType, begin_connection, build_tcp_datasource,
        connect_query_and_validate, run_query_and_check_results,
    };
    use mssql_tds::connection::tds_client::{
        BatchErrorMode, ExecuteOptions, ResultSet, StatementResult, TdsClient,
    };
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
            if connection.on_rows() {
                while let Some(row) = connection.next_row().await.unwrap() {
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
                if connection.on_rows() {
                    while let Some(row) = connection.next_row().await.unwrap() {
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
                if connection.on_rows() {
                    while let Some(row) = connection.next_row().await.unwrap() {
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
            if connection.on_rows() {
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
            if connection.on_rows() {
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
            if connection.on_rows() {
                // Get the first row and explicitly don't finish consuming the result set
                let row = connection.next_row().await.unwrap();
                assert!(row.is_some());
            }

            // With TdsClient, move_to_next() automatically drains remaining rows - this should succeed
            let second_result = connection.advance_to_rows().await;
            assert!(second_result.is_ok());
            assert!(second_result.unwrap()); // Should return true as there is a next result set

            // Verify we can read from the second result set
            if connection.on_rows() {
                let row = connection.next_row().await.unwrap();
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
                    if connection.on_rows() {
                        // Try to read first row
                        let first_row = connection.next_row().await;
                        match first_row {
                            Ok(Some(_)) => {
                                // First row succeeded (CAST('10' AS Int)), second row should error
                                let row_result = connection.next_row().await;
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

    /// One thing observed while walking a batch statement by statement.
    #[derive(Debug, PartialEq)]
    enum Step {
        /// A row set: the first column of each row, then its DONE count.
        Rows(Vec<i32>, Option<u64>),
        /// A row set that failed part-way: the rows read before the error, then
        /// the error's messages.
        RowsThenError(Vec<i32>, Vec<String>),
        /// A no-row statement's count.
        Count(Option<u64>),
        /// A statement error.
        Error(Vec<String>),
    }

    fn messages(diagnostics: &mssql_tds::error::SqlServerDiagnostics) -> Vec<String> {
        diagnostics
            .errors
            .iter()
            .map(|e| e.message.clone())
            .collect()
    }

    /// Walks a batch the way a sqlcmd-style tool does: every error arrives as
    /// `Err` at the statement that failed, `has_open_batch` says whether the
    /// batch goes on, and every count comes from the walk itself.
    async fn walk(connection: &mut TdsClient, sql: &str, mode: BatchErrorMode) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut result = connection
            .execute(sql.to_string(), ExecuteOptions::new().on_error(mode))
            .await;
        loop {
            match result {
                Ok(StatementResult::Rows) => {
                    let mut values = Vec::new();
                    loop {
                        match connection.next_row().await {
                            Ok(Some(row)) => {
                                if let ColumnValues::Int(v) = row[0] {
                                    values.push(v);
                                }
                            }
                            Ok(None) => {
                                steps.push(Step::Rows(values, connection.last_result_row_count()));
                                break;
                            }
                            Err(SqlServerError { diagnostics }) => {
                                steps.push(Step::RowsThenError(values, messages(&diagnostics)));
                                if !connection.has_open_batch() {
                                    return steps;
                                }
                                break;
                            }
                            Err(e) => panic!("row read failed: {e:?}"),
                        }
                    }
                }
                Ok(StatementResult::NoRows { rows_affected }) => {
                    steps.push(Step::Count(rows_affected));
                }
                Ok(StatementResult::End) => break,
                Err(SqlServerError { diagnostics }) => {
                    steps.push(Step::Error(messages(&diagnostics)));
                    if !connection.has_open_batch() {
                        break;
                    }
                }
                Err(e) => panic!("batch failed: {e:?}"),
            }
            result = connection.advance().await;
        }
        steps
    }

    /// The walk must leave the connection clean for the next command.
    async fn assert_still_usable(connection: &mut TdsClient) {
        assert!(!connection.has_open_batch());
        let expected = [ExpectedQueryResultType::Result(1)];
        run_query_and_check_results(connection, "SELECT 1".to_string(), &expected).await;
    }

    /// Variable assignment is tagged `SQLSELECT` and still carries a count; a
    /// tool must not print a row count for `SET @x = 1`. The expected sequence is
    /// what ODBC `sqlcmd` prints for this batch: one count for the INSERT and one
    /// for the real SELECT, nothing else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn variable_assignment_counts_are_not_reported() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "DECLARE @x int; SET @x = 1; CREATE TABLE #va (i int); \
             INSERT INTO #va VALUES (1),(2); SELECT @x = i FROM #va; \
             SELECT i FROM #va ORDER BY i;",
            BatchErrorMode::Abort,
        )
        .await;

        assert_eq!(
            steps,
            vec![Step::Count(Some(2)), Step::Rows(vec![1, 2], Some(2))]
        );
        assert_still_usable(&mut connection).await;
    }

    /// A DML batch reports one count per statement, in order, through the
    /// statement walk — what a tool printing "(N rows affected)" after each
    /// statement needs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn statement_counts_arrive_one_per_statement() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "CREATE TABLE #counts (i int); \
             INSERT INTO #counts VALUES (1), (2), (3); \
             UPDATE #counts SET i = i * 2 WHERE i > 1; \
             DELETE FROM #counts;",
            BatchErrorMode::Abort,
        )
        .await;

        // CREATE reports no count and is collapsed; the DML reports 3, 2 and 3.
        assert_eq!(
            steps,
            vec![
                Step::Count(Some(3)),
                Step::Count(Some(2)),
                Step::Count(Some(3))
            ]
        );
    }

    /// `SET NOCOUNT ON` suppresses the count itself. A row set still returns its
    /// rows, but its count must read as "none reported", not as the rows read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nocount_reports_no_counts() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "SET NOCOUNT ON; CREATE TABLE #nc (i int); INSERT INTO #nc VALUES (1), (2); \
             SELECT i FROM #nc ORDER BY i;",
            BatchErrorMode::Abort,
        )
        .await;

        assert_eq!(steps, vec![Step::Rows(vec![1, 2], None)]);
    }

    /// Under `Continue`, a statement that fails mid-batch no longer hides the
    /// result sets after it, and its error arrives in order between them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_exposes_results_after_the_failing_statement() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "SELECT 1 AS a; RAISERROR('boom', 16, 1); SELECT 2 AS b;",
            BatchErrorMode::Continue,
        )
        .await;

        assert_eq!(
            steps,
            vec![
                Step::Rows(vec![1], Some(1)),
                Step::Error(vec!["boom".to_string()]),
                Step::Rows(vec![2], Some(1)),
            ]
        );
        assert_still_usable(&mut connection).await;
    }

    /// `Abort` is the default and unchanged: the batch ends at the first error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_still_ends_the_batch_at_the_first_error() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "SELECT 1 AS a; RAISERROR('boom', 16, 1); SELECT 2 AS b;",
            BatchErrorMode::Abort,
        )
        .await;

        assert_eq!(
            steps,
            vec![
                Step::Rows(vec![1], Some(1)),
                Step::Error(vec!["boom".to_string()]),
            ]
        );
        assert_still_usable(&mut connection).await;
    }

    /// An error in the last statement is still returned as `Err` — it is not
    /// folded into a normal end of results.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_reports_an_error_in_the_last_statement() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "SELECT 1 AS a; RAISERROR('last', 16, 1);",
            BatchErrorMode::Continue,
        )
        .await;

        assert_eq!(
            steps,
            vec![
                Step::Rows(vec![1], Some(1)),
                Step::Error(vec!["last".to_string()]),
            ]
        );
        assert_still_usable(&mut connection).await;
    }

    /// A statement can fail with more than one ERROR token: adding a primary
    /// key over duplicate values sends the duplicate-key error (1505) and then
    /// "could not create constraint" (1750), on every server OS. Under
    /// `Continue` that is one failed statement — one `Err` carrying both. This
    /// failure also aborts the batch server-side, so the walk then reaches the
    /// end, and the connection is still usable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_returns_every_error_of_a_statement_together() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "SET NOCOUNT ON; CREATE TABLE #dup (i int NOT NULL); \
             INSERT INTO #dup VALUES (1), (1); SET NOCOUNT OFF; \
             SELECT 1 AS a; \
             ALTER TABLE #dup ADD PRIMARY KEY (i); \
             SELECT 2 AS b;",
            BatchErrorMode::Continue,
        )
        .await;

        assert_eq!(steps.len(), 2, "got {steps:?}");
        assert_eq!(steps[0], Step::Rows(vec![1], Some(1)));
        match &steps[1] {
            Step::Error(errors) => {
                assert_eq!(errors.len(), 2, "both errors in one Err, got {errors:?}");
            }
            other => panic!("expected one failed statement, got {other:?}"),
        }
        assert_still_usable(&mut connection).await;
    }

    /// An error inside a row set comes from `next_row`: the rows read before it
    /// are kept, and `advance` moves on to the next statement. Divide-by-zero
    /// ends only its statement, so the server goes on to the next one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_keeps_rows_read_before_an_error_inside_a_row_set() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "SET ANSI_WARNINGS ON; CREATE TABLE #div (id int PRIMARY KEY CLUSTERED, x int); \
             INSERT INTO #div VALUES (1, 1), (2, 2), (3, 0), (4, 5); \
             SELECT 10 / x AS v FROM #div ORDER BY id; \
             SELECT 7 AS w;",
            BatchErrorMode::Continue,
        )
        .await;

        assert_eq!(steps.len(), 3, "got {steps:?}");
        assert_eq!(steps[0], Step::Count(Some(4)));
        match &steps[1] {
            Step::RowsThenError(values, errors) => {
                assert_eq!(values, &vec![10, 5], "rows before the error are kept");
                assert_eq!(errors.len(), 1);
                assert!(errors[0].contains("Divide by zero"), "got {errors:?}");
            }
            other => panic!("expected rows then an error, got {other:?}"),
        }
        assert_eq!(steps[2], Step::Rows(vec![7], Some(1)));
        assert_still_usable(&mut connection).await;
    }

    /// `Continue` cannot resurrect statements the server never ran. A
    /// conversion error aborts the whole batch server-side, so after it the walk
    /// reaches the end of results — with the error reported and the rows before
    /// it kept — and the connection is still usable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_ends_when_the_server_aborts_the_batch() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let steps = walk(
            &mut connection,
            "CREATE TABLE #conv (id int PRIMARY KEY CLUSTERED, s varchar(10)); \
             INSERT INTO #conv VALUES (1, '10'), (2, '20'), (3, 'x'), (4, '40'); \
             SELECT CONVERT(int, s) AS v FROM #conv ORDER BY id; \
             SELECT 7 AS w;",
            BatchErrorMode::Continue,
        )
        .await;

        assert_eq!(steps.len(), 2, "got {steps:?}");
        assert_eq!(steps[0], Step::Count(Some(4)));
        match &steps[1] {
            Step::RowsThenError(values, errors) => {
                assert_eq!(values, &vec![10, 20], "rows before the error are kept");
                assert!(errors[0].contains("Conversion failed"), "got {errors:?}");
            }
            other => panic!("expected rows then an error, got {other:?}"),
        }
        assert_still_usable(&mut connection).await;
    }

    /// A failing statement inside a procedure is reflected by its DONEINPROC and
    /// by the procedure's DONEPROC. Walking past it must accept both, and must
    /// still reach the rest of the procedure and of the batch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_walks_through_a_failing_stored_procedure() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        run_ddl(
            &mut connection,
            "CREATE PROCEDURE #fails AS BEGIN RAISERROR('in proc', 16, 1); SELECT 2 AS b; END",
        )
        .await;

        let steps = walk(
            &mut connection,
            "EXEC #fails; SELECT 3 AS c;",
            BatchErrorMode::Continue,
        )
        .await;

        assert_eq!(
            steps,
            vec![
                Step::Error(vec!["in proc".to_string()]),
                Step::Rows(vec![2], Some(1)),
                Step::Rows(vec![3], Some(1)),
            ]
        );
        assert_still_usable(&mut connection).await;
    }

    /// The same through a nested call, where more than one procedure completes
    /// after the error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_walks_through_a_nested_failing_procedure() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        run_ddl(
            &mut connection,
            "CREATE PROCEDURE #inner_fails AS RAISERROR('inner', 16, 1);",
        )
        .await;
        run_ddl(
            &mut connection,
            "CREATE PROCEDURE #outer_calls AS BEGIN EXEC #inner_fails; SELECT 4 AS d; END",
        )
        .await;

        let steps = walk(
            &mut connection,
            "EXEC #outer_calls; SELECT 5 AS e;",
            BatchErrorMode::Continue,
        )
        .await;

        assert_eq!(
            steps,
            vec![
                Step::Error(vec!["inner".to_string()]),
                Step::Rows(vec![4], Some(1)),
                Step::Rows(vec![5], Some(1)),
            ]
        );
        assert_still_usable(&mut connection).await;
    }

    /// Errors unwinding nested procedures. SQL Server flags only the failing
    /// statement's DONEINPROC; the enclosing DONEINPROC/DONEPROC frames arrive
    /// unflagged, and a batch-aborting error ends the response with a single
    /// error-flagged DONE and no procedure frames. Each shape must walk to the
    /// end without a `ProtocolError` and leave the connection usable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continue_walks_through_errors_unwinding_nested_procedures() {
        let conversion =
            "Conversion failed when converting the varchar value 'x' to data type int.";
        let divide = "Divide by zero error encountered.";
        let cases: Vec<(&[&str], &str, Vec<Step>)> = vec![
            // Statement error as the last statement, three frames deep.
            (
                &[
                    "CREATE PROCEDURE #deep_inner AS RAISERROR('inner', 16, 1);",
                    "CREATE PROCEDURE #deep_middle AS EXEC #deep_inner;",
                    "CREATE PROCEDURE #deep_outer AS EXEC #deep_middle;",
                ],
                "EXEC #deep_outer; SELECT 5 AS e;",
                vec![
                    Step::Error(vec!["inner".to_string()]),
                    Step::Rows(vec![5], Some(1)),
                ],
            ),
            // Batch abort without a row set.
            (
                &[
                    "CREATE PROCEDURE #abort_inner AS BEGIN DECLARE @i int = CONVERT(int, 'x'); END",
                    "CREATE PROCEDURE #abort_outer AS BEGIN EXEC #abort_inner; SELECT 4 AS d; END",
                ],
                "EXEC #abort_outer; SELECT 5 AS e;",
                vec![Step::Error(vec![conversion.to_string()])],
            ),
            // Batch abort inside a row set.
            (
                &[
                    "CREATE PROCEDURE #rows_inner AS SELECT CONVERT(int, 'x');",
                    "CREATE PROCEDURE #rows_outer AS BEGIN EXEC #rows_inner; SELECT 4 AS d; END",
                ],
                "EXEC #rows_outer; SELECT 5 AS e;",
                vec![Step::RowsThenError(vec![], vec![conversion.to_string()])],
            ),
            // XACT_ABORT turns a statement error into a batch abort.
            (
                &[
                    "CREATE PROCEDURE #xact_inner AS BEGIN DECLARE @i int = 1/0; SELECT 3 AS c; END",
                    "CREATE PROCEDURE #xact_outer AS BEGIN EXEC #xact_inner; SELECT 4 AS d; END",
                ],
                "SET XACT_ABORT ON; EXEC #xact_outer; SELECT 5 AS e;",
                vec![Step::Error(vec![divide.to_string()])],
            ),
        ];

        for (ddl, sql, expected) in cases {
            let mut connection = begin_connection(&build_tcp_datasource()).await;
            for statement in ddl {
                run_ddl(&mut connection, statement).await;
            }
            let steps = walk(&mut connection, sql, BatchErrorMode::Continue).await;
            assert_eq!(steps, expected, "{sql}");
            assert_still_usable(&mut connection).await;
        }
    }
    async fn run_ddl(connection: &mut TdsClient, sql: &str) {
        connection.execute(sql.to_string(), ()).await.unwrap();
        connection.close_query().await.unwrap();
    }

    /// `close_query` under `Continue` drains past errors instead of stopping at
    /// the first one, which would leave unread tokens behind; the errors it
    /// skipped are returned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_query_drains_a_continue_batch_past_its_errors() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let first = connection
            .execute(
                "SELECT 1 AS a; RAISERROR('one', 16, 1); SELECT 2 AS b; \
                 RAISERROR('two', 16, 1); SELECT 3 AS c;"
                    .to_string(),
                ExecuteOptions::new().on_error(BatchErrorMode::Continue),
            )
            .await
            .unwrap();
        assert_eq!(first, StatementResult::Rows);

        match connection.close_query().await {
            Err(SqlServerError { diagnostics }) => {
                assert_eq!(messages(&diagnostics), vec!["one", "two"]);
            }
            other => panic!("expected the skipped errors, got {other:?}"),
        }
        assert_still_usable(&mut connection).await;
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
