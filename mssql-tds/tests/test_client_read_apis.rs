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

    /// Verifies that a custom [`RowWriter`] can intercept PLP columns via
    /// `begin_plp` / `finalize_plp` while leaving non-PLP columns buffered
    /// normally through `DefaultRowWriter`'s write methods.
    ///
    /// This is the canonical usage pattern for callers that want to process a
    /// specific PLP column (e.g. stream to disk, compute a hash, etc.) without
    /// buffering the entire value.
    #[tokio::test]
    async fn custom_row_writer_intercepts_plp_column() -> mssql_tds::core::TdsResult<()> {
        use mssql_tds::datatypes::column_values::ColumnValues;
        use mssql_tds::datatypes::row_writer::{BufferingPlpSink, PlpStreamingSink, RowWriter};
        use mssql_tds::query::metadata::ColumnMetadata;

        init_tracing();

        /// A RowWriter that captures chunks pushed for column 1 (VARBINARY(MAX))
        /// into a side buffer, and falls back to DefaultRowWriter for everything else.
        struct ChunkCapture {
            inner: mssql_tds::datatypes::row_writer::DefaultRowWriter,
            captured_chunks: Vec<u8>,
        }

        /// Owned PLP sink that accumulates chunks independently.
        /// On finalize, signals via a special sentinel value so `finalize_plp`
        /// can move the bytes out and store them on the writer.
        struct CapturingSink {
            chunks: Vec<u8>,
        }

        impl PlpStreamingSink for CapturingSink {
            fn write_chunk(&mut self, data: &[u8]) -> mssql_tds::core::TdsResult<()> {
                self.chunks.extend_from_slice(data);
                Ok(())
            }

            fn finalize(self: Box<Self>) -> mssql_tds::core::TdsResult<ColumnValues> {
                Ok(ColumnValues::Bytes(self.chunks))
            }
        }

        impl RowWriter for ChunkCapture {
            fn begin_plp(
                &mut self,
                col: usize,
                _metadata: &ColumnMetadata,
            ) -> Box<dyn PlpStreamingSink> {
                if col == 1 {
                    Box::new(CapturingSink { chunks: Vec::new() })
                } else {
                    Box::new(BufferingPlpSink::new())
                }
            }

            fn finalize_plp(
                &mut self,
                col: usize,
                sink: Box<dyn PlpStreamingSink>,
            ) -> mssql_tds::core::TdsResult<()> {
                let value = sink.finalize()?;
                if col == 1 {
                    // Move the bytes out of the finalized value into captured_chunks.
                    if let ColumnValues::Bytes(bytes) = value {
                        self.captured_chunks = bytes;
                    }
                    // Write Null to the row slot so the row vector length stays consistent.
                    self.inner.write_null(col);
                } else {
                    mssql_tds::datatypes::row_writer::write_column_value(self, col, value);
                }
                Ok(())
            }

            // All other write_* methods delegate to the inner DefaultRowWriter.
            fn write_null(&mut self, col: usize) {
                self.inner.write_null(col);
            }
            fn write_bool(&mut self, col: usize, val: bool) {
                self.inner.write_bool(col, val);
            }
            fn write_u8(&mut self, col: usize, val: u8) {
                self.inner.write_u8(col, val);
            }
            fn write_i16(&mut self, col: usize, val: i16) {
                self.inner.write_i16(col, val);
            }
            fn write_i32(&mut self, col: usize, val: i32) {
                self.inner.write_i32(col, val);
            }
            fn write_i64(&mut self, col: usize, val: i64) {
                self.inner.write_i64(col, val);
            }
            fn write_f32(&mut self, col: usize, val: f32) {
                self.inner.write_f32(col, val);
            }
            fn write_f64(&mut self, col: usize, val: f64) {
                self.inner.write_f64(col, val);
            }
            fn write_string(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::sql_string::SqlString,
            ) {
                self.inner.write_string(col, val);
            }
            fn write_bytes(&mut self, col: usize, val: Vec<u8>) {
                self.inner.write_bytes(col, val);
            }
            fn write_decimal(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::decoder::DecimalParts,
            ) {
                self.inner.write_decimal(col, val);
            }
            fn write_numeric(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::decoder::DecimalParts,
            ) {
                self.inner.write_numeric(col, val);
            }
            fn write_date(&mut self, col: usize, val: mssql_tds::datatypes::column_values::SqlDate) {
                self.inner.write_date(col, val);
            }
            fn write_time(&mut self, col: usize, val: mssql_tds::datatypes::column_values::SqlTime) {
                self.inner.write_time(col, val);
            }
            fn write_datetime(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::column_values::SqlDateTime,
            ) {
                self.inner.write_datetime(col, val);
            }
            fn write_smalldatetime(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::column_values::SqlSmallDateTime,
            ) {
                self.inner.write_smalldatetime(col, val);
            }
            fn write_datetime2(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::column_values::SqlDateTime2,
            ) {
                self.inner.write_datetime2(col, val);
            }
            fn write_datetimeoffset(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::column_values::SqlDateTimeOffset,
            ) {
                self.inner.write_datetimeoffset(col, val);
            }
            fn write_money(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::column_values::SqlMoney,
            ) {
                self.inner.write_money(col, val);
            }
            fn write_smallmoney(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::column_values::SqlSmallMoney,
            ) {
                self.inner.write_smallmoney(col, val);
            }
            fn write_uuid(&mut self, col: usize, val: uuid::Uuid) {
                self.inner.write_uuid(col, val);
            }
            fn write_xml(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::column_values::SqlXml,
            ) {
                self.inner.write_xml(col, val);
            }
            fn write_json(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::sql_json::SqlJson,
            ) {
                self.inner.write_json(col, val);
            }
            fn write_vector(
                &mut self,
                col: usize,
                val: mssql_tds::datatypes::sql_vector::SqlVector,
            ) {
                self.inner.write_vector(col, val);
            }
            fn end_row(&mut self) {
                self.inner.end_row();
            }
        }

        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        // col 0: INT — handled by default write_i32
        // col 1: VARBINARY(MAX) — intercepted by CapturingSink → captured_chunks
        // col 2: VARBINARY(MAX) — buffered normally → ColumnValues::Bytes
        let query =
            "SELECT CAST(42 AS INT), CAST(0x68656C6C6F AS VARBINARY(MAX)), CAST(0x776F726C64 AS VARBINARY(MAX))"
                .to_string();
        client.execute(query, None, None).await?;

        let col_count = client.get_metadata().len();
        assert_eq!(col_count, 3);

        let mut writer = ChunkCapture {
            inner: mssql_tds::datatypes::row_writer::DefaultRowWriter::new(col_count),
            captured_chunks: Vec::new(),
        };

        if let Some(rs) = client.get_current_resultset() {
            let has_row = rs.next_row_into(&mut writer).await?;
            assert!(has_row, "Expected a row");
        }

        // Column 1 chunks were streamed to captured_chunks — not in the row vector.
        assert_eq!(writer.captured_chunks, b"hello");

        // Column 2 was buffered normally.
        let row = writer.inner.take_row();
        assert!(matches!(row[0], ColumnValues::Int(42)));
        // col 1 slot is Null because CapturingSink::finalize() returns Null.
        assert!(matches!(row[1], ColumnValues::Null));
        assert!(matches!(row[2], ColumnValues::Bytes(ref b) if b == b"world"));

        client.close_query().await?;
        Ok(())
    }

    /// Verifies PLP columns (VARBINARY(MAX), NVARCHAR(MAX)) across multiple rows
    /// are decoded correctly end-to-end using `next_row()`.
    #[tokio::test]
    async fn plp_columns_across_multiple_rows() -> mssql_tds::core::TdsResult<()> {
        use mssql_tds::datatypes::column_values::ColumnValues;
        init_tracing();

        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        let query = "
            DECLARE @t TABLE (id INT, payload VARBINARY(MAX), label NVARCHAR(MAX));
            INSERT INTO @t VALUES
                (1, 0x616263, N'alpha'),
                (2, 0x646566, N'beta'),
                (3, NULL,    N'gamma');
            SELECT id, payload, label FROM @t
        "
        .to_string();
        client.execute(query, None, None).await?;

        if let Some(rs) = client.get_current_resultset() {
            let row1 = rs.next_row().await?.expect("row 1");
            assert!(matches!(row1[0], ColumnValues::Int(1)));
            assert!(matches!(&row1[1], ColumnValues::Bytes(b) if b == b"abc"));
            assert!(matches!(&row1[2], ColumnValues::String(s) if s.to_utf8_string() == "alpha"));

            let row2 = rs.next_row().await?.expect("row 2");
            assert!(matches!(row2[0], ColumnValues::Int(2)));
            assert!(matches!(&row2[1], ColumnValues::Bytes(b) if b == b"def"));
            assert!(matches!(&row2[2], ColumnValues::String(s) if s.to_utf8_string() == "beta"));

            let row3 = rs.next_row().await?.expect("row 3");
            assert!(matches!(row3[0], ColumnValues::Int(3)));
            assert!(matches!(row3[1], ColumnValues::Null));
            assert!(matches!(&row3[2], ColumnValues::String(s) if s.to_utf8_string() == "gamma"));

            assert!(rs.next_row().await?.is_none());
        }

        client.close_query().await?;
        Ok(())
    }

    /// Verifies that a caller can keep only columns 2 and 4 from a 5-column
    /// result set by using `next_row_into()` with a custom `RowWriter`.
    ///
    /// The wire decode still walks columns left-to-right, but the writer can
    /// ignore columns 1, 3, and 5 while retaining just the selected columns.
    #[tokio::test]
    async fn custom_row_writer_keeps_only_columns_2_and_4() -> mssql_tds::core::TdsResult<()> {
        use mssql_tds::datatypes::column_values::{SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml};
        use mssql_tds::datatypes::decoder::DecimalParts;
        use mssql_tds::datatypes::row_writer::RowWriter;
        use mssql_tds::datatypes::sql_json::SqlJson;
        use mssql_tds::datatypes::sql_string::SqlString;
        use mssql_tds::datatypes::sql_vector::SqlVector;

        init_tracing();

        #[derive(Default)]
        struct SelectedColumnsWriter {
            selected_col_2: Option<String>,
            selected_col_4: Option<Vec<u8>>,
            row_complete: bool,
        }

        impl RowWriter for SelectedColumnsWriter {
            fn write_null(&mut self, col: usize) {
                if col == 1 {
                    self.selected_col_2 = None;
                } else if col == 3 {
                    self.selected_col_4 = None;
                }
            }

            fn write_bool(&mut self, _col: usize, _val: bool) {}
            fn write_u8(&mut self, _col: usize, _val: u8) {}
            fn write_i16(&mut self, _col: usize, _val: i16) {}
            fn write_i32(&mut self, _col: usize, _val: i32) {}
            fn write_i64(&mut self, _col: usize, _val: i64) {}
            fn write_f32(&mut self, _col: usize, _val: f32) {}
            fn write_f64(&mut self, _col: usize, _val: f64) {}

            fn write_string(&mut self, col: usize, val: SqlString) {
                if col == 1 {
                    self.selected_col_2 = Some(val.to_utf8_string());
                }
            }

            fn write_bytes(&mut self, col: usize, val: Vec<u8>) {
                if col == 3 {
                    self.selected_col_4 = Some(val);
                }
            }

            fn write_decimal(&mut self, _col: usize, _val: DecimalParts) {}
            fn write_numeric(&mut self, _col: usize, _val: DecimalParts) {}
            fn write_date(&mut self, _col: usize, _val: SqlDate) {}
            fn write_time(&mut self, _col: usize, _val: SqlTime) {}
            fn write_datetime(&mut self, _col: usize, _val: SqlDateTime) {}
            fn write_smalldatetime(&mut self, _col: usize, _val: SqlSmallDateTime) {}
            fn write_datetime2(&mut self, _col: usize, _val: SqlDateTime2) {}
            fn write_datetimeoffset(&mut self, _col: usize, _val: SqlDateTimeOffset) {}
            fn write_money(&mut self, _col: usize, _val: SqlMoney) {}
            fn write_smallmoney(&mut self, _col: usize, _val: SqlSmallMoney) {}
            fn write_uuid(&mut self, _col: usize, _val: uuid::Uuid) {}
            fn write_xml(&mut self, _col: usize, _val: SqlXml) {}
            fn write_json(&mut self, _col: usize, _val: SqlJson) {}
            fn write_vector(&mut self, _col: usize, _val: SqlVector) {}

            fn end_row(&mut self) {
                self.row_complete = true;
            }
        }

        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        let query = "
            SELECT
                CAST(10 AS INT) AS c1,
                CAST(N'second-column' AS NVARCHAR(MAX)) AS c2,
                CAST(30 AS INT) AS c3,
                CAST(0xDEADBEEF AS VARBINARY(MAX)) AS c4,
                CAST(50 AS INT) AS c5
        "
        .to_string();
        client.execute(query, None, None).await?;

        let mut writer = SelectedColumnsWriter::default();
        if let Some(rs) = client.get_current_resultset() {
            let has_row = rs.next_row_into(&mut writer).await?;
            assert!(has_row, "Expected one row from the 5-column select");
            assert!(rs.next_row().await?.is_none());
        }

        assert!(writer.row_complete, "Writer should observe end_row() after the row is fully decoded");
        assert_eq!(writer.selected_col_2.as_deref(), Some("second-column"));
        assert_eq!(writer.selected_col_4.as_deref(), Some(&[0xDE, 0xAD, 0xBE, 0xEF][..]));

        client.close_query().await?;
        Ok(())
    }
}
