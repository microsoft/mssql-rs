// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

/// Live coverage for the RPC request text - the `@stmt` and `@params`
/// arguments of `sp_executesql`, `sp_prepexec` and `sp_execute` - that
/// `mssql-tds` writes straight into the packet. The server has to parse the
/// msodbcsql-format declaration and the UTF-16 statement whatever their
/// content, size and packet layout.
mod rpc_request_text {
    use std::panic::AssertUnwindSafe;

    use futures::future::FutureExt;

    use crate::common::{
        begin_connection, build_tcp_datasource, create_context, get_first_row, get_scalar_value,
        init_tracing,
    };
    use mssql_tds::connection::tds_client::{PreparedStatement, ResultSet, TdsClient};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sql_string::SqlString;
    use mssql_tds::datatypes::sql_tvp::{TvpColumnDef, TvpTableData, TvpTypeName};
    use mssql_tds::datatypes::sql_udt::UdtTypeName;
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::error::Error;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
    use mssql_tds::query::metadata::ColumnMetadata;
    use mssql_tds::query::result::ReturnValue;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    /// Characters outside the BMP: each one is a UTF-16 surrogate pair.
    const SUPPLEMENTARY: &str = "𐐀😀";
    const TVP_TYPE: &str = "dbo.RequestTextIds";

    fn param(name: &str, flags: StatusFlags, value: SqlType) -> RpcParameter {
        RpcParameter::new(Some(name.to_string()), flags, value)
    }

    fn int_params(count: usize) -> Vec<RpcParameter> {
        (1..=count)
            .map(|ordinal| {
                param(
                    &format!("@P{ordinal}"),
                    StatusFlags::NONE,
                    SqlType::Int(Some(1)),
                )
            })
            .collect()
    }

    fn sum_statement(count: usize) -> String {
        let terms: Vec<String> = (1..=count).map(|ordinal| format!("@P{ordinal}")).collect();
        format!("SELECT {}", terms.join("+"))
    }

    /// Error 8003: the RPC exceeds the server's 2100-parameter limit. The
    /// number, not the message, is stable across server languages.
    fn assert_too_many_parameters(error: &Error) {
        match error {
            Error::SqlServerError { diagnostics } => assert!(
                diagnostics.errors.iter().any(|e| e.number == 8003),
                "{error}"
            ),
            other => panic!("expected SQL Server error 8003, got {other:?}"),
        }
    }

    fn string_value(value: &ColumnValues) -> String {
        match value {
            ColumnValues::String(value) => value.to_string(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    /// The single row of the batch's only result set, with its metadata and
    /// the output values, which arrive after the rows.
    async fn row_and_outputs(
        client: &mut TdsClient,
    ) -> (Vec<ColumnMetadata>, Vec<ColumnValues>, Vec<ReturnValue>) {
        assert!(client.on_rows() || client.advance_to_rows().await.unwrap());
        let metadata = client.get_metadata().clone();
        let row = client.next_row().await.unwrap().expect("one row");
        assert!(client.next_row().await.unwrap().is_none());
        while client.advance_to_rows().await.unwrap() {}
        let outputs = client.get_return_values();
        client.close_query().await.unwrap();
        (metadata, row, outputs)
    }

    fn output_value(outputs: &[ReturnValue], name: &str) -> ColumnValues {
        outputs
            .iter()
            .find(|value| value.param_name == name)
            .unwrap_or_else(|| panic!("no output value for {name}: {outputs:?}"))
            .value
            .clone()
    }

    async fn exec_ddl(client: &mut TdsClient, sql: &str) {
        client.execute(sql.to_string(), ()).await.unwrap();
        while client.advance_to_rows().await.unwrap() {}
        client.close_query().await.unwrap();
    }

    async fn serialized_hierarchyid(client: &mut TdsClient, path: &str) -> Vec<u8> {
        client
            .execute(
                format!("SELECT CAST(hierarchyid::Parse('{path}') AS VARBINARY(892))"),
                (),
            )
            .await
            .unwrap();
        let (_, mut row) = get_first_row(client).await.unwrap();
        match row.remove(0) {
            ColumnValues::Bytes(bytes) => bytes,
            other => panic!("expected the serialized form, got {other:?}"),
        }
    }

    /// Every declaration shape in one `@params`: OUTPUT, a typed-NULL decimal
    /// and `datetime2` declared from their defaults, `nvarchar(max)` and
    /// `varbinary(max)`, a system UDT and a TVP. Sent through `sp_executesql`,
    /// then prepared (`sp_prepexec`) and executed again (`sp_execute`).
    #[tokio::test]
    async fn every_declaration_shape_round_trips() {
        let mut client = begin_connection(&build_tcp_datasource()).await;
        exec_ddl(&mut client, &format!("DROP TYPE IF EXISTS {TVP_TYPE}")).await;
        exec_ddl(
            &mut client,
            &format!("CREATE TYPE {TVP_TYPE} AS TABLE (id INT)"),
        )
        .await;

        let outcome = AssertUnwindSafe(assert_every_declaration_shape(&mut client))
            .catch_unwind()
            .await;
        exec_ddl(&mut client, &format!("DROP TYPE IF EXISTS {TVP_TYPE}")).await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    async fn assert_every_declaration_shape(client: &mut TdsClient) {
        let hierarchyid = serialized_hierarchyid(client, "/4/").await;
        let text = format!("{SUPPLEMENTARY}é");
        let params = || {
            vec![
                param("@P1", StatusFlags::BY_REF_VALUE, SqlType::Int(Some(41))),
                param("@P2", StatusFlags::NONE, SqlType::Decimal(None)),
                param("@P3", StatusFlags::NONE, SqlType::DateTime2(None)),
                param(
                    "@P4",
                    StatusFlags::NONE,
                    SqlType::NVarcharMax(Some(SqlString::from_utf8_string(text.clone()))),
                ),
                param(
                    "@P5",
                    StatusFlags::NONE,
                    SqlType::VarBinaryMax(Some(vec![1, 2, 3])),
                ),
                param(
                    "@P6",
                    StatusFlags::NONE,
                    SqlType::Udt(
                        UdtTypeName::new(None, None, "hierarchyid".to_string()),
                        Some(hierarchyid.clone()),
                    ),
                ),
                param(
                    "@P7",
                    StatusFlags::NONE,
                    SqlType::Table(
                        TvpTypeName::new(Some("dbo".to_string()), "RequestTextIds".to_string()),
                        Some(TvpTableData::new(
                            vec![TvpColumnDef::new(SqlType::Int(None))],
                            (1..=3).map(|id| vec![SqlType::Int(Some(id))]).collect(),
                        )),
                    ),
                ),
            ]
        };
        let sql = format!(
            "SET @P1 = @P1 + 1; SELECT @P2 AS d, @P3 AS t, @P4 AS s, @P5 AS b, \
             @P6.ToString() AS h, (SELECT SUM(id) FROM @P7) AS n, N'{SUPPLEMENTARY}' AS [{SUPPLEMENTARY}]"
        );

        let assert_result = |metadata: &[ColumnMetadata], row: &[ColumnValues], route: &str| {
            assert_eq!(
                (metadata[0].get_precision(), metadata[0].get_scale()),
                (Some(18), Some(10)),
                "{route}: decimal declared from its default"
            );
            assert_eq!(metadata[1].get_scale(), Some(7), "{route}: datetime2 scale");
            assert_eq!(row[0], ColumnValues::Null, "{route}");
            assert_eq!(row[1], ColumnValues::Null, "{route}");
            assert_eq!(string_value(&row[2]), text, "{route}");
            assert_eq!(row[3], ColumnValues::Bytes(vec![1, 2, 3]), "{route}");
            assert_eq!(string_value(&row[4]), "/4/", "{route}");
            assert_eq!(row[5], ColumnValues::Int(6), "{route}");
            assert_eq!(string_value(&row[6]), SUPPLEMENTARY, "{route}");
            assert_eq!(metadata[6].column_name, SUPPLEMENTARY, "{route}");
        };

        client
            .execute_sp_executesql(sql.clone(), params(), ())
            .await
            .unwrap();
        let (metadata, row, outputs) = row_and_outputs(client).await;
        assert_result(&metadata, &row, "sp_executesql");
        assert_eq!(output_value(&outputs, "@P1"), ColumnValues::Int(42));

        let mut statement = PreparedStatement::new(sql);
        let mut orphaned = None;
        for route in ["sp_prepexec", "sp_execute"] {
            client
                .execute_prepared(&mut statement, params(), &mut orphaned, ())
                .await
                .unwrap();
            let (metadata, row, outputs) = row_and_outputs(client).await;
            assert_result(&metadata, &row, route);
            assert_eq!(
                output_value(&outputs, "@P1"),
                ColumnValues::Int(42),
                "{route}"
            );
            assert!(statement.id().is_some(), "{route}: the plan is reusable");
        }
    }

    /// At the 512-byte minimum packet size, the statement and declaration
    /// each span several packets, so packet edges fall inside surrogate pairs
    /// and inside PLP chunk headers.
    #[tokio::test]
    async fn supplementary_text_spans_minimum_size_packets() {
        let mut context = create_context();
        context.packet_size = 512;
        let mut client = TdsConnectionProvider {}
            .create_client(context, &build_tcp_datasource(), None)
            .await
            .unwrap();
        assert_eq!(client.packet_size(), 512);

        let literal = format!("{SUPPLEMENTARY}a").repeat(300);
        let sql = format!(
            "SELECT N'{literal}' AS v, {} AS n",
            sum_statement(150).replacen("SELECT ", "", 1)
        );

        client
            .execute_sp_executesql(sql.clone(), int_params(150), ())
            .await
            .unwrap();
        let (_, row) = get_first_row(&mut client).await.unwrap();
        assert_eq!(string_value(&row[0]), literal);
        assert_eq!(row[1], ColumnValues::Int(150));

        let mut statement = PreparedStatement::new(sql);
        let mut orphaned = None;
        for _ in 0..2 {
            client
                .execute_prepared(&mut statement, int_params(150), &mut orphaned, ())
                .await
                .unwrap();
            let (_, row) = get_first_row(&mut client).await.unwrap();
            assert_eq!(string_value(&row[0]), literal);
            assert_eq!(row[1], ColumnValues::Int(150));
        }
    }

    /// SQL Server caps an RPC at 2100 parameters, and `@stmt`, `@params` and
    /// `@handle` count toward it. `sp_executesql` therefore takes at most 2098
    /// values and `sp_prepexec` 2097, as msodbcsql also finds. One more is
    /// rejected by the server (error 8003) without breaking the connection.
    #[tokio::test]
    async fn parameter_count_limit_counts_the_request_text() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute_sp_executesql(sum_statement(2098), int_params(2098), ())
            .await
            .unwrap();
        assert_eq!(
            get_scalar_value(&mut client).await.unwrap(),
            Some(ColumnValues::Int(2098))
        );
        let error = client
            .execute_sp_executesql(sum_statement(2099), int_params(2099), ())
            .await
            .expect_err("2101 RPC parameters exceed the server limit");
        assert_too_many_parameters(&error);
        client.close_query().await.unwrap();

        let mut statement = PreparedStatement::new(sum_statement(2097));
        let mut orphaned = None;
        for _ in 0..2 {
            client
                .execute_prepared(&mut statement, int_params(2097), &mut orphaned, ())
                .await
                .unwrap();
            assert_eq!(
                get_scalar_value(&mut client).await.unwrap(),
                Some(ColumnValues::Int(2097))
            );
        }
        let mut statement = PreparedStatement::new(sum_statement(2098));
        let error = client
            .execute_prepared(&mut statement, int_params(2098), &mut orphaned, ())
            .await
            .expect_err("2101 RPC parameters exceed the server limit");
        assert_too_many_parameters(&error);
        client.close_query().await.unwrap();

        client.execute("SELECT 1".to_string(), ()).await.unwrap();
        assert_eq!(
            get_scalar_value(&mut client).await.unwrap(),
            Some(ColumnValues::Int(1))
        );
    }
}
