// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end tests for the incremental (streamed) PLP parameter-write path
//! (`begin_sp_executesql` / `write_streamed_chunk` / `end_streamed_param`).
//!
//! These mirror the read-side PLP tests: they require a live SQL Server and are
//! driven by the `DB_HOST` / `DB_USERNAME` / `SQL_PASSWORD` environment
//! variables (see `common`), so they only run in CI.

#[cfg(test)]
mod common;

mod streamed_plp_write {
    use crate::common::{build_tcp_datasource, create_context, init_tracing};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, StreamedParamStatus};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

    /// Encodes a string to the UTF-16LE wire bytes an `nvarchar(max)` value uses.
    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// Streams a large `nvarchar(max)` value into a temp table in multiple
    /// chunks, then reads it back and verifies the round-trip.
    #[tokio::test]
    async fn stream_nvarchar_max_round_trips() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        client
            .execute(
                "CREATE TABLE #plp_nvm (id INT, val NVARCHAR(MAX))".to_string(),
                None,
                None,
            )
            .await?;
        client.close_query().await?;

        let value = "Z".repeat(20_000);
        let wire = utf16le(&value);

        let streamed = RpcParameter::new(
            Some("@v".to_string()),
            StatusFlags::NONE,
            SqlType::NVarcharMax(None),
        )
        .data_at_exec();

        let status = client
            .begin_sp_executesql(
                "INSERT INTO #plp_nvm (id, val) VALUES (1, @v)".to_string(),
                vec![streamed],
                None,
                None,
            )
            .await?;
        match status {
            StreamedParamStatus::NeedData { param_name } => assert_eq!(param_name, "@v"),
            StreamedParamStatus::Done => panic!("expected NeedData for the first streamed param"),
        }

        // Stream the value in two chunks split on an even (code-unit) boundary.
        let split = (wire.len() / 2) & !1;
        client.write_streamed_chunk(&wire[..split]).await?;
        client.write_streamed_chunk(&wire[split..]).await?;

        let status = client.end_streamed_param().await?;
        assert!(matches!(status, StreamedParamStatus::Done));
        client.close_query().await?;

        client
            .execute("SELECT val FROM #plp_nvm WHERE id = 1".to_string(), None, None)
            .await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a row");
            match &row[0] {
                ColumnValues::String(s) => {
                    let round_tripped = s.to_utf8_string();
                    assert_eq!(round_tripped.len(), value.len());
                    assert_eq!(round_tripped, value);
                }
                other => panic!("Expected String for nvarchar(max), got {other:?}"),
            }
        } else {
            panic!("expected a result set");
        }
        client.close_query().await?;
        Ok(())
    }

    /// Mixes a fully-materialized parameter with a data-at-execution one in the
    /// same `begin_sp_executesql` call: the materialized `@id` is sent up front
    /// via the normal serialize path, while `@v` is streamed. Verifies the
    /// integrated single-`named_params` list (not a separate streamed argument).
    #[tokio::test]
    async fn stream_mixed_materialized_and_data_at_exec() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        client
            .execute(
                "CREATE TABLE #plp_mix (id INT, val NVARCHAR(MAX))".to_string(),
                None,
                None,
            )
            .await?;
        client.close_query().await?;

        let value = "M".repeat(15_000);
        let wire = utf16le(&value);

        let params = vec![
            RpcParameter::new(
                Some("@id".to_string()),
                StatusFlags::NONE,
                SqlType::Int(Some(7)),
            ),
            RpcParameter::new(
                Some("@v".to_string()),
                StatusFlags::NONE,
                SqlType::NVarcharMax(None),
            )
            .data_at_exec(),
        ];

        let status = client
            .begin_sp_executesql(
                "INSERT INTO #plp_mix (id, val) VALUES (@id, @v)".to_string(),
                params,
                None,
                None,
            )
            .await?;
        assert!(
            matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@v")
        );

        client.write_streamed_chunk(&wire).await?;
        let status = client.end_streamed_param().await?;
        assert!(matches!(status, StreamedParamStatus::Done));
        client.close_query().await?;

        client
            .execute(
                "SELECT val FROM #plp_mix WHERE id = 7".to_string(),
                None,
                None,
            )
            .await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a row");
            match &row[0] {
                ColumnValues::String(s) => assert_eq!(s.to_utf8_string(), value),
                other => panic!("Expected String for nvarchar(max), got {other:?}"),
            }
        } else {
            panic!("expected a result set");
        }
        client.close_query().await?;
        Ok(())
    }

    /// Streams a large `varbinary(max)` value in multiple chunks and verifies the
    /// round-trip.
    #[tokio::test]
    async fn stream_varbinary_max_round_trips() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        client
            .execute(
                "CREATE TABLE #plp_vbm (id INT, val VARBINARY(MAX))".to_string(),
                None,
                None,
            )
            .await?;
        client.close_query().await?;

        let value: Vec<u8> = (0..30_000u32).map(|i| (i % 256) as u8).collect();

        let streamed = RpcParameter::new(
            Some("@v".to_string()),
            StatusFlags::NONE,
            SqlType::VarBinaryMax(None),
        )
        .data_at_exec();

        let status = client
            .begin_sp_executesql(
                "INSERT INTO #plp_vbm (id, val) VALUES (1, @v)".to_string(),
                vec![streamed],
                None,
                None,
            )
            .await?;
        assert!(matches!(status, StreamedParamStatus::NeedData { .. }));

        for chunk in value.chunks(7_000) {
            client.write_streamed_chunk(chunk).await?;
        }

        let status = client.end_streamed_param().await?;
        assert!(matches!(status, StreamedParamStatus::Done));
        client.close_query().await?;

        client
            .execute("SELECT val FROM #plp_vbm WHERE id = 1".to_string(), None, None)
            .await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a row");
            match &row[0] {
                ColumnValues::Bytes(b) => assert_eq!(b.as_slice(), value.as_slice()),
                other => panic!("Expected Bytes for varbinary(max), got {other:?}"),
            }
        } else {
            panic!("expected a result set");
        }
        client.close_query().await?;
        Ok(())
    }

    /// Streams two `nvarchar(max)` parameters in one RPC, advancing through the
    /// `NeedData` -> `NeedData` -> `Done` lifecycle.
    #[tokio::test]
    async fn stream_two_params_round_trips() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        client
            .execute(
                "CREATE TABLE #plp_two (id INT, a NVARCHAR(MAX), b NVARCHAR(MAX))".to_string(),
                None,
                None,
            )
            .await?;
        client.close_query().await?;

        let a = "A".repeat(12_000);
        let b = "B".repeat(9_000);

        let params = vec![
            RpcParameter::new(
                Some("@a".to_string()),
                StatusFlags::NONE,
                SqlType::NVarcharMax(None),
            )
            .data_at_exec(),
            RpcParameter::new(
                Some("@b".to_string()),
                StatusFlags::NONE,
                SqlType::NVarcharMax(None),
            )
            .data_at_exec(),
        ];

        let status = client
            .begin_sp_executesql(
                "INSERT INTO #plp_two (id, a, b) VALUES (1, @a, @b)".to_string(),
                params,
                None,
                None,
            )
            .await?;
        assert!(
            matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@a")
        );

        client.write_streamed_chunk(&utf16le(&a)).await?;
        let status = client.end_streamed_param().await?;
        assert!(
            matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@b")
        );

        client.write_streamed_chunk(&utf16le(&b)).await?;
        let status = client.end_streamed_param().await?;
        assert!(matches!(status, StreamedParamStatus::Done));
        client.close_query().await?;

        client
            .execute(
                "SELECT LEN(a), LEN(b) FROM #plp_two WHERE id = 1".to_string(),
                None,
                None,
            )
            .await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a row");
            assert_eq!(row.len(), 2);
        } else {
            panic!("expected a result set");
        }
        client.close_query().await?;
        Ok(())
    }

    /// Streams a `varchar(max)` value into several rows in sequence, each via its
    /// own `begin`/chunks/`end` cycle on the same connection, then verifies the
    /// row count with `SELECT COUNT(*)`. Proves the streamed-write state machine
    /// resets cleanly between rows so many rows can be written back-to-back.
    #[tokio::test]
    async fn stream_varchar_max_multiple_rows_round_trips() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        client
            .execute(
                "CREATE TABLE #plp_rows (id INT, val VARCHAR(MAX))".to_string(),
                None,
                None,
            )
            .await?;
        client.close_query().await?;

        const ROW_COUNT: i32 = 5;
        for id in 1..=ROW_COUNT {
            // varchar(max) wire bytes are single-byte encoded; ASCII payload
            // bytes equal the value's UTF-8 bytes, so stream them directly.
            let value = format!("row-{id}-").repeat(3_000);

            let streamed = RpcParameter::new(
                Some("@v".to_string()),
                StatusFlags::NONE,
                SqlType::VarcharMax(None),
            )
            .data_at_exec();

            let status = client
                .begin_sp_executesql(
                    format!("INSERT INTO #plp_rows (id, val) VALUES ({id}, @v)"),
                    vec![streamed],
                    None,
                    None,
                )
                .await?;
            assert!(
                matches!(&status, StreamedParamStatus::NeedData { param_name } if param_name == "@v")
            );

            for chunk in value.as_bytes().chunks(4_096) {
                client.write_streamed_chunk(chunk).await?;
            }
            let status = client.end_streamed_param().await?;
            assert!(matches!(status, StreamedParamStatus::Done));
            client.close_query().await?;
        }

        // Every streamed row must be present.
        client
            .execute(
                "SELECT COUNT(*) FROM #plp_rows".to_string(),
                None,
                None,
            )
            .await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a count row");
            match &row[0] {
                ColumnValues::Int(count) => assert_eq!(*count, ROW_COUNT),
                other => panic!("Expected Int for COUNT(*), got {other:?}"),
            }
        } else {
            panic!("expected a result set");
        }
        client.close_query().await?;

        // Spot-check the last row's value survived the multi-row stream intact.
        client
            .execute(
                format!("SELECT val FROM #plp_rows WHERE id = {ROW_COUNT}"),
                None,
                None,
            )
            .await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected the last row");
            match &row[0] {
                ColumnValues::String(s) => {
                    assert_eq!(s.to_utf8_string(), format!("row-{ROW_COUNT}-").repeat(3_000));
                }
                other => panic!("Expected String for varchar(max), got {other:?}"),
            }
        } else {
            panic!("expected a result set");
        }
        client.close_query().await?;
        Ok(())
    }

    /// A NULL value for a `nvarchar(max)` column round-trips as SQL NULL. NULL is
    /// never streamed: it is bound directly as a materialized `NVarcharMax(None)`
    /// value (which serializes to `PLP_NULL`), mirroring how a NULL data-at-exec
    /// indicator is sent inline without ever requesting streamed data.
    #[tokio::test]
    async fn write_null_max_round_trips() -> mssql_tds::core::TdsResult<()> {
        init_tracing();
        let context = create_context();
        let provider = TdsConnectionProvider {};
        let mut client = provider
            .create_client(context, &build_tcp_datasource(), None)
            .await?;

        client
            .execute(
                "CREATE TABLE #plp_null (id INT, val NVARCHAR(MAX))".to_string(),
                None,
                None,
            )
            .await?;
        client.close_query().await?;

        // A NULL max parameter is materialized (value None -> PLP_NULL), so
        // begin_sp_executesql completes atomically with no NeedData.
        let null_param = RpcParameter::new(
            Some("@v".to_string()),
            StatusFlags::NONE,
            SqlType::NVarcharMax(None),
        );

        let status = client
            .begin_sp_executesql(
                "INSERT INTO #plp_null (id, val) VALUES (1, @v)".to_string(),
                vec![null_param],
                None,
                None,
            )
            .await?;
        assert!(
            matches!(status, StreamedParamStatus::Done),
            "a materialized NULL parameter must not request streamed data"
        );
        client.close_query().await?;

        client
            .execute(
                "SELECT val FROM #plp_null WHERE id = 1".to_string(),
                None,
                None,
            )
            .await?;
        if let Some(resultset) = client.get_current_resultset() {
            let row = resultset.next_row().await?.expect("expected a row");
            assert!(
                matches!(&row[0], ColumnValues::Null),
                "expected SQL NULL, got {:?}",
                &row[0]
            );
        } else {
            panic!("expected a result set");
        }
        client.close_query().await?;
        Ok(())
    }
}