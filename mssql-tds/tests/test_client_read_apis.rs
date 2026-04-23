// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod client_based_iterators {
    use crate::common::{build_tcp_datasource, create_context, init_tracing};
    use futures::lock::Mutex;
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
    use std::sync::Arc;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[tokio::test]
    async fn test_multiquery_iteration() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;
        let query = "SELECT TOP(2) * FROM sys.databases; SELECT 1";

        client.execute(query.to_string(), None, None).await?;
        let mut row_count = 0;
        loop {
            while client.next_row().await?.is_some() {
                row_count += 1;
            }

            if !client.move_to_next().await? {
                break;
            }
        }
        assert_eq!(
            row_count, 3,
            "Expected 3 rows from the multi-query execution"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_orderby_token_in_query() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;
        let query = "SELECT TOP 1 
            name, 
            database_id, 
            create_date 
            FROM sys.databases 
            ORDER BY name;";

        client.execute(query.to_string(), None, None).await?;
        let mut row_count = 0;
        loop {
            while client.next_row().await?.is_some() {
                row_count += 1;
            }

            if !client.move_to_next().await? {
                break;
            }
        }
        assert_eq!(
            row_count, 1,
            "Expected 3 rows from the multi-query execution"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_incomplete_resultset_iteration() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;
        let query = "SELECT TOP(2) * FROM sys.databases; SELECT 1";

        client.execute(query.to_string(), None, None).await?;
        let mut row_count = 0;

        if client.next_row().await?.is_some() {
            row_count += 1;
        }
        client.close_query().await?;

        assert_eq!(
            row_count, 1,
            "Expected 1 row from the incomplete result set execution"
        );
        let mut row_count = 0;
        client.execute(query.to_string(), None, None).await?;
        loop {
            while client.next_row().await?.is_some() {
                row_count += 1;
            }
            if !client.move_to_next().await? {
                break;
            }
        }

        client.close_query().await?;
        assert_eq!(
            row_count, 3,
            "Expected 3 rows from the multi-query execution on connection reuse."
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_bad_query_error_followed_by_valid_query() -> Result<(), Box<dyn std::error::Error>>
    {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;
        let query = "bad bad query";

        let err = client.execute(query.to_string(), None, None).await;
        assert!(err.is_err(), "Expected error for bad query");

        let query = "SELECT TOP(2) * FROM sys.databases; SELECT 1";
        client.execute(query.to_string(), None, None).await?;
        let mut row_count = 0;
        loop {
            while client.next_row().await?.is_some() {
                row_count += 1;
            }
            if !client.move_to_next().await? {
                break;
            }
        }
        assert_eq!(
            row_count, 3,
            "Expected 3 rows from the valid query execution after bad query"
        );
        Ok(())
    }

    // This test will fail in Azure since DB creation from TSQL as well as USE statements are not allowed.
    #[tokio::test]
    async fn test_use_database_statement() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;
        let create_database_query = "IF DB_ID('TestDB') IS NULL CREATE DATABASE TestDB";

        client
            .execute(create_database_query.to_string(), None, None)
            .await?;
        let use_database_query = "USE TestDB";
        client
            .execute(use_database_query.to_string(), None, None)
            .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_stored_proc_with_query_and_output() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;
        let client = Arc::new(Mutex::new(client));

        // Create a stored procedure with an output parameter
        let create_proc = "CREATE PROCEDURE #test_proc        
             @paramIn int,
            @paramOut int output
         AS
         BEGIN
            select 1
           set @paramOut = @paramIn
         END";
        client
            .lock()
            .await
            .execute(create_proc.to_string(), None, None)
            .await?;
        client.lock().await.close_query().await?;

        let proc_name = "#test_proc".to_string();
        let named_parameters = vec![
            RpcParameter::new(
                Some("@paramIn".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(42)),
            ),
            RpcParameter::new(
                Some("@paramOut".to_string()),
                StatusFlags::BY_REF_VALUE,
                SqlType::Int(None),
            ),
        ];
        client
            .lock()
            .await
            .execute_stored_procedure(proc_name, None, Some(named_parameters), None, None)
            .await?;
        let mut binding = client.lock().await;
        let result_set = binding.get_current_resultset();
        if let Some(result_set) = result_set {
            let _ = result_set.get_metadata();
            let mut row_count = 0;

            while (result_set.next_row().await?).is_some() {
                row_count += 1;
            }
            assert_eq!(
                row_count, 1,
                "Expected 1 row from the stored procedure execution with output parameter"
            );
        } else {
            panic!("Expected a result set from stored procedure execution, but got None");
        }

        // Move once more till we read the return values.
        while binding.move_to_next().await? {
            // Continue to next result set if available
        }

        let output_param = binding.get_return_values();

        assert!(output_param.len() == 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_query_date_time_types_metadata() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        // Query that returns various date/time types with explicit scales
        let query = r#"
            SELECT 
                CAST('14:30:45.1234567' AS TIME(7)) AS time_col,
                CAST('2024-03-15' AS DATE) AS date_col,
                CAST('2024-03-15 14:30:45.123' AS DATETIME) AS datetime_col,
                CAST('2024-03-15 14:30:45.1234567' AS DATETIME2(7)) AS datetime2_col,
                CAST('2024-03-15 14:30:00' AS SMALLDATETIME) AS smalldatetime_col,
                CAST('2024-03-15 14:30:45.1234567 +05:30' AS DATETIMEOFFSET(7)) AS datetimeoffset_col
        "#;

        client.execute(query.to_string(), None, None).await?;

        // Get metadata and verify it was parsed correctly
        let resultset = client
            .get_current_resultset()
            .expect("Expected a resultset");
        let metadata = resultset.get_metadata();

        // Verify we have 6 columns
        assert_eq!(metadata.len(), 6, "Expected 6 date/time columns");

        // Verify TIME(7) metadata - should have length 5 and scale 7
        let time_col = &metadata[0];
        assert_eq!(time_col.column_name, "time_col");
        assert_eq!(time_col.type_info.length, 5, "TIME(7) should have length 5");
        let time_scale = time_col.get_scale();
        assert_eq!(time_scale, Some(7), "TIME(7) should have scale 7");

        // Verify DATE metadata - should have length 3
        let date_col = &metadata[1];
        assert_eq!(date_col.column_name, "date_col");
        assert_eq!(date_col.type_info.length, 3, "DATE should have length 3");

        // Verify DATETIME metadata - should have length 8
        let datetime_col = &metadata[2];
        assert_eq!(datetime_col.column_name, "datetime_col");
        assert_eq!(
            datetime_col.type_info.length, 8,
            "DATETIME should have length 8"
        );

        // Verify DATETIME2(7) metadata - should have length 8 (5 for time + 3 for date) and scale 7
        let datetime2_col = &metadata[3];
        assert_eq!(datetime2_col.column_name, "datetime2_col");
        assert_eq!(
            datetime2_col.type_info.length, 8,
            "DATETIME2(7) should have length 8"
        );
        let datetime2_scale = datetime2_col.get_scale();
        assert_eq!(datetime2_scale, Some(7), "DATETIME2(7) should have scale 7");

        // Verify SMALLDATETIME metadata - should have length 4
        let smalldatetime_col = &metadata[4];
        assert_eq!(smalldatetime_col.column_name, "smalldatetime_col");
        assert_eq!(
            smalldatetime_col.type_info.length, 4,
            "SMALLDATETIME should have length 4"
        );

        // Verify DATETIMEOFFSET(7) metadata - should have length 10 (5 for time + 3 for date + 2 for offset) and scale 7
        let datetimeoffset_col = &metadata[5];
        assert_eq!(datetimeoffset_col.column_name, "datetimeoffset_col");
        assert_eq!(
            datetimeoffset_col.type_info.length, 10,
            "DATETIMEOFFSET(7) should have length 10"
        );
        let datetimeoffset_scale = datetimeoffset_col.get_scale();
        assert_eq!(
            datetimeoffset_scale,
            Some(7),
            "DATETIMEOFFSET(7) should have scale 7"
        );

        // Also verify we can read the actual values
        let row = resultset.next_row().await?.expect("Expected a row");

        // Just verify we got values of the right types
        match &row[0] {
            mssql_tds::datatypes::column_values::ColumnValues::Time(_) => {}
            _ => panic!("Expected Time value"),
        }

        match &row[1] {
            mssql_tds::datatypes::column_values::ColumnValues::Date(_) => {}
            _ => panic!("Expected Date value"),
        }

        match &row[2] {
            mssql_tds::datatypes::column_values::ColumnValues::DateTime(_) => {}
            _ => panic!("Expected DateTime value"),
        }

        match &row[3] {
            mssql_tds::datatypes::column_values::ColumnValues::DateTime2(_) => {}
            _ => panic!("Expected DateTime2 value"),
        }

        match &row[4] {
            mssql_tds::datatypes::column_values::ColumnValues::SmallDateTime(_) => {}
            _ => panic!("Expected SmallDateTime value"),
        }

        match &row[5] {
            mssql_tds::datatypes::column_values::ColumnValues::DateTimeOffset(_) => {}
            _ => panic!("Expected DateTimeOffset value"),
        }

        Ok(())
    }

    /// Test that verifies packet size negotiation works correctly.
    ///
    /// This test reproduces the bug where `notify_session_setting_change` only updated
    /// `self.packet_size` but NOT `self.tds_read_buffer.max_packet_size`. This caused
    /// the validation check to reject valid packets that exceeded the initial 4096-byte
    /// limit but were within the negotiated size (e.g., 8000 bytes).
    ///
    /// The test executes a query that returns enough data to require the negotiated
    /// packet size, which would fail with "TDS packet length 8000 exceeds negotiated
    /// max packet size 4096" if the buffer's max_packet_size wasn't properly updated.
    #[tokio::test]
    async fn test_query_with_negotiated_packet_size() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        // Query that returns a large result set to trigger the negotiated packet size.
        // The REPLICATE function creates a string large enough to potentially span
        // multiple TDS packets at the negotiated size (typically 8000 bytes).
        // This would fail with "TDS packet length 8000 exceeds negotiated max packet size 4096"
        // if the read buffer's max_packet_size wasn't updated after login negotiation.
        let query = "SELECT REPLICATE('X', 5000) AS LargeColumn, 
                            REPLICATE('Y', 5000) AS AnotherLargeColumn,
                            1 AS SmallColumn";

        client.execute(query.to_string(), None, None).await?;

        let mut row_count = 0;
        while let Some(row) = client.next_row().await? {
            row_count += 1;

            // Verify we got the expected data
            match &row[0] {
                mssql_tds::datatypes::column_values::ColumnValues::String(s) => {
                    assert_eq!(s.to_utf8_string().len(), 5000, "Expected 5000 X characters");
                }
                _ => panic!("Expected String value for LargeColumn"),
            }

            match &row[1] {
                mssql_tds::datatypes::column_values::ColumnValues::String(s) => {
                    assert_eq!(s.to_utf8_string().len(), 5000, "Expected 5000 Y characters");
                }
                _ => panic!("Expected String value for AnotherLargeColumn"),
            }

            match &row[2] {
                mssql_tds::datatypes::column_values::ColumnValues::Int(v) => {
                    assert_eq!(*v, 1, "Expected SmallColumn to be 1");
                }
                _ => panic!("Expected Int value for SmallColumn"),
            }
        }

        assert_eq!(row_count, 1, "Expected exactly 1 row");

        client.close_query().await?;
        Ok(())
    }

    /// Test that verifies multiple queries work after packet size negotiation.
    /// This ensures the buffer state remains consistent across multiple query executions.
    #[tokio::test]
    async fn test_multiple_queries_with_negotiated_packet_size()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        // First query: large data
        let query1 = "SELECT REPLICATE('A', 6000) AS Col1";
        client.execute(query1.to_string(), None, None).await?;

        let mut count = 0;
        while let Some(row) = client.next_row().await? {
            count += 1;
            match &row[0] {
                mssql_tds::datatypes::column_values::ColumnValues::String(s) => {
                    assert_eq!(s.to_utf8_string().len(), 6000);
                }
                _ => panic!("Expected String value"),
            }
        }
        assert_eq!(count, 1);
        client.close_query().await?;

        // Second query: even larger data
        let query2 = "SELECT REPLICATE('B', 7000) AS Col1, REPLICATE('C', 7000) AS Col2";
        client.execute(query2.to_string(), None, None).await?;

        count = 0;
        while let Some(row) = client.next_row().await? {
            count += 1;
            match &row[0] {
                mssql_tds::datatypes::column_values::ColumnValues::String(s) => {
                    assert_eq!(s.to_utf8_string().len(), 7000);
                }
                _ => panic!("Expected String value"),
            }
            match &row[1] {
                mssql_tds::datatypes::column_values::ColumnValues::String(s) => {
                    assert_eq!(s.to_utf8_string().len(), 7000);
                }
                _ => panic!("Expected String value"),
            }
        }
        assert_eq!(count, 1);
        client.close_query().await?;

        // Third query: small data (verifies buffer works correctly after large data)
        let query3 = "SELECT 42 AS SmallValue";
        client.execute(query3.to_string(), None, None).await?;

        count = 0;
        while let Some(row) = client.next_row().await? {
            count += 1;
            match &row[0] {
                mssql_tds::datatypes::column_values::ColumnValues::Int(v) => {
                    assert_eq!(*v, 42);
                }
                _ => panic!("Expected Int value"),
            }
        }
        assert_eq!(count, 1);
        client.close_query().await?;

        Ok(())
    }

    /// SQL Server can return multiple ERROR tokens in a single batch execution.
    /// For example, `RAISERROR` at severity <= 18 doesn't abort the batch, so
    /// two consecutive RAISERRORs produce two ERROR tokens in the stream.
    /// This test verifies that:
    /// 1. The first error is properly surfaced to the caller
    /// 2. The remaining error tokens and DONE(ERROR) tokens are fully drained
    /// 3. The connection remains usable for subsequent queries
    #[tokio::test]
    async fn test_multiple_errors_in_single_batch() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        // Two RAISERRORs at severity 16 — SQL Server sends:
        //   ERROR("First error") → DONE(ERROR,MORE) → ERROR("Second error") → DONE(ERROR)
        let query = "RAISERROR('First error', 16, 1); RAISERROR('Second error', 16, 1)";

        let result = client.execute(query.to_string(), None, None).await;
        assert!(
            result.is_err(),
            "Expected error from batch with multiple RAISERRORs"
        );
        let err = result.unwrap_err();
        let err_msg = format!("{err}");
        assert!(
            err_msg.contains("First error"),
            "Expected first error to be surfaced, got: {err_msg}"
        );
        assert!(
            err_msg.contains("Second error"),
            "Expected second error to be surfaced, got: {err_msg}"
        );

        // Verify multiple errors are collected in the error variant
        if let mssql_tds::error::Error::SqlServerError { errors } = &err {
            assert_eq!(
                errors.len(),
                2,
                "Expected 2 errors in collection, got {}",
                errors.len()
            );
            assert!(errors[0].message.contains("First error"));
            assert!(errors[1].message.contains("Second error"));
        } else {
            panic!("Expected SqlServerError variant, got: {err:?}");
        }

        // Connection must remain usable after multiple errors
        client.execute("SELECT 1".to_string(), None, None).await?;
        let mut row_count = 0;
        while client.next_row().await?.is_some() {
            row_count += 1;
        }
        client.close_query().await?;
        assert_eq!(
            row_count, 1,
            "Expected 1 row from SELECT 1 after error recovery"
        );

        Ok(())
    }

    /// Referencing multiple nonexistent tables in a batch produces multiple errors.
    /// Verifies the stream is properly drained and the connection survives.
    #[tokio::test]
    async fn test_multiple_invalid_object_errors() -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        let query = "SELECT * FROM nonexistent_table_abc_1; SELECT * FROM nonexistent_table_abc_2";

        let result = client.execute(query.to_string(), None, None).await;
        assert!(
            result.is_err(),
            "Expected error from referencing nonexistent tables"
        );

        // SQL Server may abort batch after first object-resolution failure,
        // so we may get 1 or 2 errors depending on server behavior.
        if let mssql_tds::error::Error::SqlServerError { errors } = result.unwrap_err() {
            assert!(!errors.is_empty(), "Expected at least one error");
            assert!(
                errors[0].message.contains("nonexistent_table_abc_1"),
                "Expected first error to reference table_abc_1, got: {}",
                errors[0].message
            );
        } else {
            panic!("Expected SqlServerError variant");
        }

        // Connection must remain usable
        client
            .execute("SELECT 42 AS val".to_string(), None, None)
            .await?;
        let mut row_count = 0;
        while let Some(row) = client.next_row().await? {
            row_count += 1;
            match &row[0] {
                mssql_tds::datatypes::column_values::ColumnValues::Int(v) => {
                    assert_eq!(*v, 42);
                }
                _ => panic!("Expected Int value"),
            }
        }
        client.close_query().await?;
        assert_eq!(
            row_count, 1,
            "Expected 1 row from SELECT 42 after error recovery"
        );

        Ok(())
    }

    /// A batch mixing valid DML with errors: the error must be surfaced and
    /// the connection must survive for a follow-up query.
    #[tokio::test]
    async fn test_error_after_successful_statement_in_batch()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = create_context();

        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        // First statement succeeds (SELECT 1 produces a result set),
        // second statement fails with an error
        let query = "SELECT 1; RAISERROR('Batch error after success', 16, 1)";

        client.execute(query.to_string(), None, None).await?;

        // Consume the first result set
        let mut row_count = 0;
        while client.next_row().await?.is_some() {
            row_count += 1;
        }
        assert_eq!(row_count, 1, "Expected 1 row from SELECT 1");

        // Advancing to the next result should hit the error
        let next_result = client.move_to_next().await;
        assert!(
            next_result.is_err(),
            "Expected error from RAISERROR after SELECT"
        );

        // Connection must remain usable
        client
            .execute("SELECT 99 AS val".to_string(), None, None)
            .await?;
        let mut row_count2 = 0;
        while client.next_row().await?.is_some() {
            row_count2 += 1;
        }
        client.close_query().await?;
        assert_eq!(
            row_count2, 1,
            "Expected 1 row from SELECT 99 after error recovery"
        );

        Ok(())
    }

    #[tokio::test]
    async fn decode_diverse_server_types() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        let query = "SELECT
            CAST(1 AS TINYINT) AS ti,
            CAST(2 AS SMALLINT) AS si,
            CAST(100 AS BIGINT) AS bi,
            CAST(3.14 AS REAL) AS r,
            CAST(2.718 AS FLOAT) AS f,
            CAST(1 AS BIT) AS b,
            CAST('2024-01-15' AS DATE) AS d,
            CAST('12:30:45.1234' AS TIME(4)) AS t4,
            CAST('2024-01-15 12:30:45.12' AS DATETIME2(2)) AS dt2,
            CAST('2024-01-15 12:30:45.1 +05:30' AS DATETIMEOFFSET(1)) AS dto1,
            CAST('2024-01-15 12:30:00' AS SMALLDATETIME) AS sdt,
            CAST(99.95 AS SMALLMONEY) AS sm,
            CAST(12345.6789 AS MONEY) AS m,
            CAST(0xDEAD AS VARBINARY(10)) AS vb,
            CAST(NEWID() AS UNIQUEIDENTIFIER) AS uid"
            .to_string();

        client.execute(query, None, None).await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a row");
            assert_eq!(row.len(), 15);

            use mssql_tds::datatypes::column_values::ColumnValues;
            assert!(matches!(row[0], ColumnValues::TinyInt(1)));
            assert!(matches!(row[1], ColumnValues::SmallInt(2)));
            assert!(matches!(row[2], ColumnValues::BigInt(100)));
            assert!(matches!(row[3], ColumnValues::Real(_)));
            assert!(matches!(row[4], ColumnValues::Float(_)));
            assert!(matches!(row[5], ColumnValues::Bit(true)));
            assert!(matches!(row[6], ColumnValues::Date(_)));
            assert!(matches!(row[7], ColumnValues::Time(_)));
            assert!(matches!(row[8], ColumnValues::DateTime2(_)));
            assert!(matches!(row[9], ColumnValues::DateTimeOffset(_)));
            assert!(matches!(row[10], ColumnValues::SmallDateTime(_)));
            assert!(matches!(row[11], ColumnValues::SmallMoney(_)));
            assert!(matches!(row[12], ColumnValues::Money(_)));
            assert!(matches!(row[13], ColumnValues::Bytes(_)));
            assert!(matches!(row[14], ColumnValues::Uuid(_)));

            if let ColumnValues::Time(t) = &row[7] {
                assert_eq!(t.scale, 4);
            }
            if let ColumnValues::DateTime2(dt2) = &row[8] {
                assert_eq!(dt2.time.scale, 2);
            }
            if let ColumnValues::DateTimeOffset(dto) = &row[9] {
                assert_eq!(dto.datetime2.time.scale, 1);
                assert_eq!(dto.offset, 330);
            }
        }
        client.close_query().await?;
        Ok(())
    }

    #[tokio::test]
    async fn decode_string_types_with_collation() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        let query = "SELECT
            CAST('hello' AS VARCHAR(50)) AS vc,
            CAST(N'world' AS NVARCHAR(50)) AS nvc,
            CAST('fixed' AS CHAR(10)) AS c,
            CAST(N'fixed' AS NCHAR(10)) AS nc"
            .to_string();

        client.execute(query, None, None).await?;
        if let Some(resultset) = client.get_current_resultset() {
            let meta = resultset.get_metadata().clone();
            let row = resultset.next_row().await?.expect("expected a row");
            assert_eq!(row.len(), 4);
            for col in &row {
                assert!(matches!(
                    col,
                    mssql_tds::datatypes::column_values::ColumnValues::String(_)
                ));
            }
            for m in &meta {
                if m.column_name == "vc" || m.column_name == "nvc" {
                    assert!(
                        m.get_collation().is_some(),
                        "collation missing for {}",
                        m.column_name
                    );
                }
            }
        }
        client.close_query().await?;
        Ok(())
    }

    #[tokio::test]
    async fn decode_decimal_precision_scale() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        let query = "SELECT
            CAST(123.456 AS DECIMAL(10,3)) AS d1,
            CAST(99999.99 AS NUMERIC(18,2)) AS n1,
            CAST(0.000001 AS DECIMAL(38,6)) AS d2"
            .to_string();

        client.execute(query, None, None).await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a row");
            assert_eq!(row.len(), 3);
            for col in &row {
                use mssql_tds::datatypes::column_values::ColumnValues;
                assert!(
                    matches!(col, ColumnValues::Decimal(_) | ColumnValues::Numeric(_)),
                    "Expected Decimal/Numeric, got {col:?}"
                );
            }
        }
        client.close_query().await?;
        Ok(())
    }

    #[tokio::test]
    async fn decode_plp_types() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        let large = "Z".repeat(20_000);
        let query = format!(
            "SELECT
            CAST('{large}' AS NVARCHAR(MAX)) AS nvm,
            CAST('{large}' AS VARCHAR(MAX)) AS vm,
            CAST(0xDEADBEEF AS VARBINARY(MAX)) AS vbm,
            CAST('<r>1</r>' AS XML) AS x"
        );

        client.execute(query, None, None).await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a row");
            assert_eq!(row.len(), 4);
            use mssql_tds::datatypes::column_values::ColumnValues;
            match &row[0] {
                ColumnValues::String(s) => assert_eq!(s.to_utf8_string().len(), 20_000),
                other => panic!("Expected String for nvarchar(max), got {other:?}"),
            }
            match &row[1] {
                ColumnValues::String(s) => assert_eq!(s.to_utf8_string().len(), 20_000),
                other => panic!("Expected String for varchar(max), got {other:?}"),
            }
            assert!(matches!(row[2], ColumnValues::Bytes(_)));
            assert!(matches!(row[3], ColumnValues::Xml(_)));
        }
        client.close_query().await?;
        Ok(())
    }
}
