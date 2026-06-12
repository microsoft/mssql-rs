// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for Table-Valued Parameters (TVPs).
//!
//! Each test creates a dedicated table type in the connected database,
//! round-trips rows through `sp_executesql` using a `SqlType::Table`
//! parameter, and validates the result set returned by `SELECT ... FROM @tvp`.
//!
//! Setup and teardown are handled by [`with_tvp_type`], which drops the type on
//! entry (clearing any orphan left by a prior failed run) and again on exit.
//! Cleanup is guaranteed even when the test body panics (e.g. a failed
//! assertion): the panic is caught, the type is dropped, and then the panic is
//! re-raised so the test still fails without leaving objects on the server.

#[cfg(test)]
mod common;

mod tvp_tests {
    use std::panic::AssertUnwindSafe;
    use std::str::FromStr;

    use futures::future::FutureExt;

    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
    use mssql_tds::datatypes::column_values::{
        ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
        SqlSmallMoney, SqlTime,
    };
    use mssql_tds::datatypes::decoder::DecimalParts;
    use mssql_tds::datatypes::sql_string::SqlString;
    use mssql_tds::datatypes::sql_tvp::{
        TvpColumnDef, TvpOrderFlags, TvpOrderHint, TvpTableData, TvpTypeName,
    };
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
    use uuid::Uuid;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    /// Runs a statement that returns no result set (DDL) and finishes the batch.
    async fn exec_ddl(client: &mut TdsClient, sql: String) {
        client.execute(sql, None, None).await.unwrap();
        while client.move_to_next().await.unwrap() {}
        client.close_query().await.unwrap();
    }

    /// Executes `sp_executesql` with the supplied named parameters and collects
    /// every row from every result set.
    async fn exec_tvp_query(
        client: &mut TdsClient,
        sql: &str,
        params: Vec<RpcParameter>,
    ) -> Vec<Vec<ColumnValues>> {
        client
            .execute_sp_executesql(sql.to_string(), params, None, None)
            .await
            .unwrap();

        let mut rows = Vec::new();
        loop {
            if let Some(resultset) = client.get_current_resultset() {
                while let Some(row) = resultset.next_row().await.unwrap() {
                    rows.push(row);
                }
            }
            if !client.move_to_next().await.unwrap() {
                break;
            }
        }
        client.close_query().await.unwrap();
        rows
    }

    /// Like [`exec_tvp_query`] but surfaces any server error instead of
    /// unwrapping, so a test can assert that an invalid request is rejected.
    /// The error is flattened to a `String` to avoid leaking the crate's error
    /// type into the test.
    async fn try_exec_tvp_query(
        client: &mut TdsClient,
        sql: &str,
        params: Vec<RpcParameter>,
    ) -> Result<Vec<Vec<ColumnValues>>, String> {
        client
            .execute_sp_executesql(sql.to_string(), params, None, None)
            .await
            .map_err(|e| e.to_string())?;

        let mut rows = Vec::new();
        loop {
            if let Some(resultset) = client.get_current_resultset() {
                while let Some(row) = resultset.next_row().await.map_err(|e| e.to_string())? {
                    rows.push(row);
                }
            }
            if !client.move_to_next().await.map_err(|e| e.to_string())? {
                break;
            }
        }
        client.close_query().await.map_err(|e| e.to_string())?;
        Ok(rows)
    }

    /// Builds a single named TVP parameter.
    fn tvp_param(name: &str, type_name: TvpTypeName, table: Option<TvpTableData>) -> RpcParameter {
        RpcParameter::new(
            Some(name.to_string()),
            StatusFlags::NONE,
            SqlType::Table(type_name, table),
        )
    }

    /// Best-effort `DROP TYPE`, ignoring any error.
    ///
    /// Used before a test (to clear an orphan left by a prior failed run) and
    /// after (to guarantee teardown). Errors are swallowed because the client
    /// may be in an indeterminate protocol state if the test body panicked
    /// mid-stream; surfacing that error would only mask the original failure.
    async fn try_drop_type(client: &mut TdsClient, type_name: &str) {
        if client
            .execute(format!("DROP TYPE IF EXISTS {type_name}"), None, None)
            .await
            .is_ok()
        {
            while client.move_to_next().await.unwrap_or(false) {}
            let _ = client.close_query().await;
        }
    }

    /// Connects, creates a fresh table type, runs `body`, and then drops the
    /// type — even if `body` panics.
    ///
    /// A failed assertion (or any other panic) inside `body` is caught, the
    /// type is dropped, and the panic is then re-raised so the test still fails
    /// while never leaving an orphaned type on the server.
    async fn with_tvp_type<F>(type_name: &str, create_sql: &str, body: F)
    where
        F: AsyncFnOnce(&mut TdsClient),
    {
        let mut client = begin_connection(&build_tcp_datasource()).await;
        // Clear any orphan from a previous failed run, then create fresh.
        try_drop_type(&mut client, type_name).await;
        exec_ddl(&mut client, create_sql.to_string()).await;

        let outcome = AssertUnwindSafe(body(&mut client)).catch_unwind().await;

        try_drop_type(&mut client, type_name).await;

        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// Asserts a cell is SQL `NULL`.
    fn assert_null(cell: &ColumnValues, ctx: &str) {
        assert!(
            matches!(cell, ColumnValues::Null),
            "{ctx} expected Null, got {cell:?}"
        );
    }

    /// Convenience: a non-NULL nvarchar cell.
    fn nvarchar(value: &str, max: u16) -> SqlType {
        SqlType::NVarchar(Some(SqlString::from_utf8_string(value.to_string())), max)
    }

    #[tokio::test]
    async fn test_tvp_int_name_roundtrip() {
        with_tvp_type(
            "dbo.TvpItIntName",
            "CREATE TYPE dbo.TvpItIntName AS TABLE (id INT, name NVARCHAR(100))",
            async |client| {
                let columns = vec![
                    TvpColumnDef::new(SqlType::Int(None)),
                    TvpColumnDef::new(SqlType::NVarchar(None, 100)),
                ];
                let rows = vec![
                    vec![
                        SqlType::Int(Some(1)),
                        SqlType::NVarchar(
                            Some(SqlString::from_utf8_string("alpha".to_string())),
                            100,
                        ),
                    ],
                    vec![
                        SqlType::Int(Some(2)),
                        SqlType::NVarchar(
                            Some(SqlString::from_utf8_string("beta".to_string())),
                            100,
                        ),
                    ],
                    vec![
                        SqlType::Int(Some(3)),
                        SqlType::NVarchar(
                            Some(SqlString::from_utf8_string("gamma".to_string())),
                            100,
                        ),
                    ],
                ];
                let table = TvpTableData::new(columns, rows);
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItIntName".to_string()),
                    Some(table),
                );

                let result =
                    exec_tvp_query(client, "SELECT id, name FROM @tvp ORDER BY id", vec![param])
                        .await;

                assert_eq!(result.len(), 3, "expected 3 rows back");
                let expected_names = ["alpha", "beta", "gamma"];
                for (i, row) in result.iter().enumerate() {
                    assert_eq!(row.len(), 2);
                    match &row[0] {
                        ColumnValues::Int(v) => assert_eq!(*v, (i as i32) + 1),
                        other => panic!("row {i} col 0 expected Int, got {other:?}"),
                    }
                    match &row[1] {
                        ColumnValues::String(v) => assert_eq!(
                            v,
                            &SqlString::from_utf8_string(expected_names[i].to_string())
                        ),
                        other => panic!("row {i} col 1 expected String, got {other:?}"),
                    }
                }
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_tvp_empty_table() {
        with_tvp_type(
            "dbo.TvpItEmpty",
            "CREATE TYPE dbo.TvpItEmpty AS TABLE (id INT)",
            async |client| {
                let table =
                    TvpTableData::new(vec![TvpColumnDef::new(SqlType::Int(None))], Vec::new());
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItEmpty".to_string()),
                    Some(table),
                );

                let result = exec_tvp_query(client, "SELECT id FROM @tvp", vec![param]).await;
                assert_eq!(result.len(), 0, "empty TVP should yield no rows");
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_tvp_null_yields_empty() {
        with_tvp_type(
            "dbo.TvpItNull",
            "CREATE TYPE dbo.TvpItNull AS TABLE (id INT)",
            async |client| {
                // A NULL TVP (no metadata) must be sent as a *default* parameter: SQL
                // Server rejects a null table-valued parameter unless the RPC status
                // byte sets DEFAULT_VALUE. The server then treats it as an empty table.
                let param = RpcParameter::new(
                    Some("@tvp".to_string()),
                    StatusFlags::DEFAULT_VALUE,
                    SqlType::Table(
                        TvpTypeName::new(Some("dbo".to_string()), "TvpItNull".to_string()),
                        None,
                    ),
                );

                let result = exec_tvp_query(client, "SELECT id FROM @tvp", vec![param]).await;
                assert_eq!(result.len(), 0, "NULL TVP should yield no rows");
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_tvp_multi_type_roundtrip() {
        with_tvp_type(
            "dbo.TvpItMultiType",
            "CREATE TYPE dbo.TvpItMultiType AS TABLE (\
             id INT, \
             name NVARCHAR(50), \
             big BIGINT, \
             flag BIT, \
             amount DECIMAL(18,4), \
             when2 DATETIME2(6), \
             data VARBINARY(64), \
             guid UNIQUEIDENTIFIER)",
            async |client| {
                // Column definitions: decimal and datetime2 carry precision/scale overrides.
                let mut amount_col = TvpColumnDef::new(SqlType::Decimal(None));
                amount_col.precision = Some(18);
                amount_col.scale = Some(4);
                let mut when_col = TvpColumnDef::new(SqlType::DateTime2(None));
                when_col.scale = Some(6);

                let columns = vec![
                    TvpColumnDef::new(SqlType::Int(None)),
                    TvpColumnDef::new(SqlType::NVarchar(None, 50)),
                    TvpColumnDef::new(SqlType::BigInt(None)),
                    TvpColumnDef::new(SqlType::Bit(None)),
                    amount_col,
                    when_col,
                    TvpColumnDef::new(SqlType::VarBinary(None, 64)),
                    TvpColumnDef::new(SqlType::Uuid(None)),
                ];

                let guid = Uuid::from_str("123e4567-e89b-12d3-a456-426614174000").unwrap();
                // 1.2345 as DECIMAL(18,4): mantissa 12345 with scale 4.
                let amount = DecimalParts::from_i64(12345, 18, 4).unwrap();
                let when = SqlDateTime2 {
                    days: 730_000,
                    time: SqlTime {
                        time_nanoseconds: 12_345_678_900,
                        scale: 6,
                    },
                };
                let data = vec![0xDE_u8, 0xAD, 0xBE, 0xEF];

                let rows = vec![vec![
                    SqlType::Int(Some(42)),
                    SqlType::NVarchar(Some(SqlString::from_utf8_string("widget".to_string())), 50),
                    SqlType::BigInt(Some(9_000_000_000)),
                    SqlType::Bit(Some(true)),
                    SqlType::Decimal(Some(amount.clone())),
                    SqlType::DateTime2(Some(when.clone())),
                    SqlType::VarBinary(Some(data.clone()), 64),
                    SqlType::Uuid(Some(guid)),
                ]];

                let table = TvpTableData::new(columns, rows);
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItMultiType".to_string()),
                    Some(table),
                );

                let result = exec_tvp_query(
                    client,
                    "SELECT id, name, big, flag, amount, when2, data, guid FROM @tvp",
                    vec![param],
                )
                .await;

                assert_eq!(result.len(), 1, "expected 1 row back");
                let row = &result[0];
                assert_eq!(row.len(), 8);

                match &row[0] {
                    ColumnValues::Int(v) => assert_eq!(*v, 42),
                    other => panic!("col id expected Int, got {other:?}"),
                }
                match &row[1] {
                    ColumnValues::String(v) => {
                        assert_eq!(v, &SqlString::from_utf8_string("widget".to_string()))
                    }
                    other => panic!("col name expected String, got {other:?}"),
                }
                match &row[2] {
                    ColumnValues::BigInt(v) => assert_eq!(*v, 9_000_000_000),
                    other => panic!("col big expected BigInt, got {other:?}"),
                }
                match &row[3] {
                    ColumnValues::Bit(v) => assert!(*v),
                    other => panic!("col flag expected Bit, got {other:?}"),
                }
                match &row[4] {
                    ColumnValues::Decimal(v) => {
                        assert_eq!(v.scale, 4, "decimal scale");
                        assert_eq!(v, &amount, "decimal value round-trip");
                    }
                    other => panic!("col amount expected Decimal, got {other:?}"),
                }
                match &row[5] {
                    ColumnValues::DateTime2(v) => {
                        assert_eq!(v.days, when.days, "datetime2 days");
                        assert_eq!(
                            v.time.time_nanoseconds, when.time.time_nanoseconds,
                            "datetime2 time"
                        );
                    }
                    other => panic!("col when2 expected DateTime2, got {other:?}"),
                }
                match &row[6] {
                    ColumnValues::Bytes(v) => assert_eq!(v, &data),
                    other => panic!("col data expected Bytes, got {other:?}"),
                }
                match &row[7] {
                    ColumnValues::Uuid(v) => assert_eq!(*v, guid),
                    other => panic!("col guid expected Uuid, got {other:?}"),
                }
            },
        )
        .await;
    }

    /// Mixed NULL and non-NULL cells across both fixed-length (int, bigint,
    /// bit, datetime2, uniqueidentifier, decimal) and variable-length
    /// (nvarchar, varbinary) columns. Also covers the empty-string-vs-NULL
    /// distinction for nvarchar. This exercises the per-cell NULL marker paths
    /// that the other tests never hit (every other test sends only non-NULL
    /// cells).
    #[tokio::test]
    async fn test_tvp_null_and_nonnull_cells() {
        with_tvp_type(
            "dbo.TvpItNulls",
            "CREATE TYPE dbo.TvpItNulls AS TABLE (\
             id INT, \
             n_int INT, \
             n_big BIGINT, \
             n_bit BIT, \
             n_dt2 DATETIME2(3), \
             n_guid UNIQUEIDENTIFIER, \
             n_dec DECIMAL(10,2), \
             n_str NVARCHAR(50), \
             n_bin VARBINARY(32))",
            async |client| {
                let mut dt2_col = TvpColumnDef::new(SqlType::DateTime2(None));
                dt2_col.scale = Some(3);
                let mut dec_col = TvpColumnDef::new(SqlType::Decimal(None));
                dec_col.precision = Some(10);
                dec_col.scale = Some(2);

                let columns = vec![
                    TvpColumnDef::new(SqlType::Int(None)),
                    TvpColumnDef::new(SqlType::Int(None)),
                    TvpColumnDef::new(SqlType::BigInt(None)),
                    TvpColumnDef::new(SqlType::Bit(None)),
                    dt2_col,
                    TvpColumnDef::new(SqlType::Uuid(None)),
                    dec_col,
                    TvpColumnDef::new(SqlType::NVarchar(None, 50)),
                    TvpColumnDef::new(SqlType::VarBinary(None, 32)),
                ];

                let guid1 = Uuid::from_str("11111111-1111-1111-1111-111111111111").unwrap();
                let guid3 = Uuid::from_str("33333333-3333-3333-3333-333333333333").unwrap();
                // DATETIME2(3): millisecond resolution, so time must be a
                // multiple of 10_000 in the internal 100ns-unit field.
                let dt2 = SqlDateTime2 {
                    days: 730_000,
                    time: SqlTime {
                        time_nanoseconds: 12_340_000,
                        scale: 3,
                    },
                };
                // 123.45 as DECIMAL(10,2).
                let dec = DecimalParts::from_i64(12345, 10, 2).unwrap();

                let rows = vec![
                    // Row id=1: every column non-NULL.
                    vec![
                        SqlType::Int(Some(1)),
                        SqlType::Int(Some(100)),
                        SqlType::BigInt(Some(10_000_000_000)),
                        SqlType::Bit(Some(true)),
                        SqlType::DateTime2(Some(dt2.clone())),
                        SqlType::Uuid(Some(guid1)),
                        SqlType::Decimal(Some(dec.clone())),
                        nvarchar("hello", 50),
                        SqlType::VarBinary(Some(vec![0x01, 0x02, 0x03]), 32),
                    ],
                    // Row id=2: every nullable column NULL.
                    vec![
                        SqlType::Int(Some(2)),
                        SqlType::Int(None),
                        SqlType::BigInt(None),
                        SqlType::Bit(None),
                        SqlType::DateTime2(None),
                        SqlType::Uuid(None),
                        SqlType::Decimal(None),
                        SqlType::NVarchar(None, 50),
                        SqlType::VarBinary(None, 32),
                    ],
                    // Row id=3: a mix, including an *empty* (non-NULL) nvarchar.
                    vec![
                        SqlType::Int(Some(3)),
                        SqlType::Int(Some(-5)),
                        SqlType::BigInt(None),
                        SqlType::Bit(Some(false)),
                        SqlType::DateTime2(None),
                        SqlType::Uuid(Some(guid3)),
                        SqlType::Decimal(None),
                        nvarchar("", 50),
                        SqlType::VarBinary(Some(vec![0xAA]), 32),
                    ],
                ];

                let table = TvpTableData::new(columns, rows);
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItNulls".to_string()),
                    Some(table),
                );

                let result = exec_tvp_query(
                    client,
                    "SELECT id, n_int, n_big, n_bit, n_dt2, n_guid, n_dec, n_str, n_bin \
                     FROM @tvp ORDER BY id",
                    vec![param],
                )
                .await;

                assert_eq!(result.len(), 3, "expected 3 rows back");

                // Row id=1: all non-NULL.
                let r1 = &result[0];
                assert!(matches!(&r1[0], ColumnValues::Int(1)));
                assert!(matches!(&r1[1], ColumnValues::Int(100)));
                assert!(matches!(&r1[2], ColumnValues::BigInt(10_000_000_000)));
                assert!(matches!(&r1[3], ColumnValues::Bit(true)));
                match &r1[4] {
                    ColumnValues::DateTime2(v) => {
                        assert_eq!(v.days, dt2.days);
                        assert_eq!(v.time.time_nanoseconds, dt2.time.time_nanoseconds);
                    }
                    other => panic!("r1 n_dt2 expected DateTime2, got {other:?}"),
                }
                match &r1[5] {
                    ColumnValues::Uuid(v) => assert_eq!(*v, guid1),
                    other => panic!("r1 n_guid expected Uuid, got {other:?}"),
                }
                match &r1[6] {
                    ColumnValues::Decimal(v) => assert_eq!(v, &dec),
                    other => panic!("r1 n_dec expected Decimal, got {other:?}"),
                }
                match &r1[7] {
                    ColumnValues::String(v) => {
                        assert_eq!(v, &SqlString::from_utf8_string("hello".to_string()))
                    }
                    other => panic!("r1 n_str expected String, got {other:?}"),
                }
                match &r1[8] {
                    ColumnValues::Bytes(v) => assert_eq!(v, &vec![0x01, 0x02, 0x03]),
                    other => panic!("r1 n_bin expected Bytes, got {other:?}"),
                }

                // Row id=2: all nullable columns NULL.
                let r2 = &result[1];
                assert!(matches!(&r2[0], ColumnValues::Int(2)));
                for (i, cell) in r2.iter().enumerate().skip(1) {
                    assert_null(cell, &format!("r2 col {i}"));
                }

                // Row id=3: mix.
                let r3 = &result[2];
                assert!(matches!(&r3[0], ColumnValues::Int(3)));
                assert!(matches!(&r3[1], ColumnValues::Int(-5)));
                assert_null(&r3[2], "r3 n_big");
                assert!(matches!(&r3[3], ColumnValues::Bit(false)));
                assert_null(&r3[4], "r3 n_dt2");
                match &r3[5] {
                    ColumnValues::Uuid(v) => assert_eq!(*v, guid3),
                    other => panic!("r3 n_guid expected Uuid, got {other:?}"),
                }
                assert_null(&r3[6], "r3 n_dec");
                match &r3[7] {
                    // Empty string must round-trip as a non-NULL empty string,
                    // distinct from the NULL in row id=2.
                    ColumnValues::String(v) => {
                        assert_eq!(v, &SqlString::from_utf8_string(String::new()))
                    }
                    other => panic!("r3 n_str expected empty String, got {other:?}"),
                }
                match &r3[8] {
                    ColumnValues::Bytes(v) => assert_eq!(v, &vec![0xAA]),
                    other => panic!("r3 n_bin expected Bytes, got {other:?}"),
                }
            },
        )
        .await;
    }

    /// End-to-end smoke test for the order/unique optional metadata.
    ///
    /// Note on what this can and cannot prove: SQL Server treats
    /// `TVP_ORDER_UNIQUE` as a *trust-based optimizer hint*. It does not
    /// validate the hint against the data for an ordinary `SELECT ... FROM @tvp`
    /// (verified empirically: sending duplicate, out-of-order rows with an
    /// `ASC | UNIQUE` hint is accepted without error), so a failure-based test
    /// here would be unreliable. What this test *does* prove is that the server
    /// accepts our serialized order-unique token as well-formed — malformed
    /// metadata would cause it to reject the entire TVP. The exact wire-byte
    /// layout is locked down separately by the unit tests in `sql_tvp.rs`
    /// (`test_write_tvp_order_unique_bytes`), which match the .NET SqlClient
    /// reference implementation.
    #[tokio::test]
    async fn test_tvp_order_hint() {
        with_tvp_type(
            "dbo.TvpItOrder",
            "CREATE TYPE dbo.TvpItOrder AS TABLE (id INT, label NVARCHAR(20))",
            async |client| {
                let columns = vec![
                    TvpColumnDef::new(SqlType::Int(None)),
                    TvpColumnDef::new(SqlType::NVarchar(None, 20)),
                ];
                // Data that genuinely satisfies the declared ASC | UNIQUE order.
                let rows = vec![
                    vec![SqlType::Int(Some(1)), nvarchar("a", 20)],
                    vec![SqlType::Int(Some(2)), nvarchar("b", 20)],
                    vec![SqlType::Int(Some(3)), nvarchar("c", 20)],
                ];

                let mut table = TvpTableData::new(columns, rows);
                table.order_hints = vec![TvpOrderHint {
                    column_ordinal: 1,
                    flags: TvpOrderFlags::ASC | TvpOrderFlags::UNIQUE,
                }];

                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItOrder".to_string()),
                    Some(table),
                );

                let result = exec_tvp_query(
                    client,
                    "SELECT id, label FROM @tvp ORDER BY id",
                    vec![param],
                )
                .await;

                assert_eq!(
                    result.len(),
                    3,
                    "server should accept a TVP carrying an order-unique token and return all rows"
                );
                let expected = ["a", "b", "c"];
                for (i, row) in result.iter().enumerate() {
                    match &row[0] {
                        ColumnValues::Int(v) => assert_eq!(*v, (i as i32) + 1),
                        other => panic!("row {i} id expected Int, got {other:?}"),
                    }
                    match &row[1] {
                        ColumnValues::String(v) => {
                            assert_eq!(v, &SqlString::from_utf8_string(expected[i].to_string()))
                        }
                        other => panic!("row {i} label expected String, got {other:?}"),
                    }
                }
            },
        )
        .await;
    }

    /// Multiple rows of mixed (fixed- and variable-length) types, validating
    /// per-row TVP_ROW token iteration over complex values rather than the
    /// single row the broad type test sends.
    #[tokio::test]
    async fn test_tvp_multi_row_multi_type() {
        with_tvp_type(
            "dbo.TvpItMultiRow",
            "CREATE TYPE dbo.TvpItMultiRow AS TABLE (\
             id INT, name NVARCHAR(50), amount DECIMAL(18,4), data VARBINARY(64))",
            async |client| {
                let mut amount_col = TvpColumnDef::new(SqlType::Decimal(None));
                amount_col.precision = Some(18);
                amount_col.scale = Some(4);

                let columns = vec![
                    TvpColumnDef::new(SqlType::Int(None)),
                    TvpColumnDef::new(SqlType::NVarchar(None, 50)),
                    amount_col,
                    TvpColumnDef::new(SqlType::VarBinary(None, 64)),
                ];

                let amounts = [
                    DecimalParts::from_i64(11_111, 18, 4).unwrap(),
                    DecimalParts::from_i64(222_222, 18, 4).unwrap(),
                    DecimalParts::from_i64(3_333_333, 18, 4).unwrap(),
                ];
                let names = ["first", "second", "third"];
                let blobs = [vec![0x01u8], vec![0x02, 0x03], vec![0x04, 0x05, 0x06]];

                let rows = (0..3)
                    .map(|i| {
                        vec![
                            SqlType::Int(Some((i as i32) + 1)),
                            nvarchar(names[i], 50),
                            SqlType::Decimal(Some(amounts[i].clone())),
                            SqlType::VarBinary(Some(blobs[i].clone()), 64),
                        ]
                    })
                    .collect();

                let table = TvpTableData::new(columns, rows);
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItMultiRow".to_string()),
                    Some(table),
                );

                let result = exec_tvp_query(
                    client,
                    "SELECT id, name, amount, data FROM @tvp ORDER BY id",
                    vec![param],
                )
                .await;

                assert_eq!(result.len(), 3, "expected 3 rows back");
                for (i, row) in result.iter().enumerate() {
                    match &row[0] {
                        ColumnValues::Int(v) => assert_eq!(*v, (i as i32) + 1),
                        other => panic!("row {i} id expected Int, got {other:?}"),
                    }
                    match &row[1] {
                        ColumnValues::String(v) => {
                            assert_eq!(v, &SqlString::from_utf8_string(names[i].to_string()))
                        }
                        other => panic!("row {i} name expected String, got {other:?}"),
                    }
                    match &row[2] {
                        ColumnValues::Decimal(v) => assert_eq!(v, &amounts[i]),
                        other => panic!("row {i} amount expected Decimal, got {other:?}"),
                    }
                    match &row[3] {
                        ColumnValues::Bytes(v) => assert_eq!(v, &blobs[i]),
                        other => panic!("row {i} data expected Bytes, got {other:?}"),
                    }
                }
            },
        )
        .await;
    }

    /// Broad type coverage: small integers, floating point, money, the full
    /// date/time family, numeric, and non-Unicode char/varchar columns that the
    /// other tests do not exercise.
    #[tokio::test]
    async fn test_tvp_type_breadth() {
        with_tvp_type(
            "dbo.TvpItTypes",
            "CREATE TYPE dbo.TvpItTypes AS TABLE (\
             c_tiny TINYINT, \
             c_small SMALLINT, \
             c_real REAL, \
             c_float FLOAT, \
             c_money MONEY, \
             c_smallmoney SMALLMONEY, \
             c_date DATE, \
             c_time TIME(7), \
             c_dto DATETIMEOFFSET(7), \
             c_dt DATETIME, \
             c_numeric NUMERIC(10,3), \
             c_varchar VARCHAR(20), \
             c_char CHAR(5))",
            async |client| {
                let mut time_col = TvpColumnDef::new(SqlType::Time(None));
                time_col.scale = Some(7);
                let mut dto_col = TvpColumnDef::new(SqlType::DateTimeOffset(None));
                dto_col.scale = Some(7);
                let mut numeric_col = TvpColumnDef::new(SqlType::Numeric(None));
                numeric_col.precision = Some(10);
                numeric_col.scale = Some(3);

                let columns = vec![
                    TvpColumnDef::new(SqlType::TinyInt(None)),
                    TvpColumnDef::new(SqlType::SmallInt(None)),
                    TvpColumnDef::new(SqlType::Real(None)),
                    TvpColumnDef::new(SqlType::Float(None)),
                    TvpColumnDef::new(SqlType::Money(None)),
                    TvpColumnDef::new(SqlType::SmallMoney(None)),
                    TvpColumnDef::new(SqlType::Date(None)),
                    time_col,
                    dto_col,
                    TvpColumnDef::new(SqlType::DateTime(None)),
                    numeric_col,
                    TvpColumnDef::new(SqlType::Varchar(None, 20)),
                    TvpColumnDef::new(SqlType::Char(None, 5)),
                ];

                let time = SqlTime {
                    time_nanoseconds: 12_345_678_900,
                    scale: 7,
                };
                let dto = SqlDateTimeOffset {
                    datetime2: SqlDateTime2 {
                        days: 730_000,
                        time: time.clone(),
                    },
                    offset: 60,
                };
                let dt = SqlDateTime {
                    days: 45_000,
                    time: 1_000,
                };
                // 12.345 as NUMERIC(10,3).
                let numeric = DecimalParts::from_i64(12_345, 10, 3).unwrap();

                let rows = vec![vec![
                    SqlType::TinyInt(Some(7)),
                    SqlType::SmallInt(Some(-1234)),
                    SqlType::Real(Some(1.5)),
                    SqlType::Float(Some(2.25)),
                    SqlType::Money(Some(SqlMoney {
                        lsb_part: 1234,
                        msb_part: 0,
                    })),
                    SqlType::SmallMoney(Some(SqlSmallMoney { int_val: 9999 })),
                    SqlType::Date(Some(SqlDate::create(730_000).unwrap())),
                    SqlType::Time(Some(time.clone())),
                    SqlType::DateTimeOffset(Some(dto.clone())),
                    SqlType::DateTime(Some(dt.clone())),
                    SqlType::Numeric(Some(numeric.clone())),
                    SqlType::Varchar(Some(SqlString::from_utf8_string("hello".to_string())), 20),
                    SqlType::Char(Some(SqlString::from_utf8_string("world".to_string())), 5),
                ]];

                let table = TvpTableData::new(columns, rows);
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItTypes".to_string()),
                    Some(table),
                );

                let result = exec_tvp_query(
                    client,
                    "SELECT c_tiny, c_small, c_real, c_float, c_money, c_smallmoney, c_date, \
                     c_time, c_dto, c_dt, c_numeric, c_varchar, c_char FROM @tvp",
                    vec![param],
                )
                .await;

                assert_eq!(result.len(), 1, "expected 1 row back");
                let row = &result[0];
                assert_eq!(row.len(), 13);

                match &row[0] {
                    ColumnValues::TinyInt(v) => assert_eq!(*v, 7),
                    other => panic!("c_tiny expected TinyInt, got {other:?}"),
                }
                match &row[1] {
                    ColumnValues::SmallInt(v) => assert_eq!(*v, -1234),
                    other => panic!("c_small expected SmallInt, got {other:?}"),
                }
                match &row[2] {
                    ColumnValues::Real(v) => assert_eq!(*v, 1.5),
                    other => panic!("c_real expected Real, got {other:?}"),
                }
                match &row[3] {
                    ColumnValues::Float(v) => assert_eq!(*v, 2.25),
                    other => panic!("c_float expected Float, got {other:?}"),
                }
                match &row[4] {
                    ColumnValues::Money(v) => {
                        assert_eq!(v.lsb_part, 1234);
                        assert_eq!(v.msb_part, 0);
                    }
                    other => panic!("c_money expected Money, got {other:?}"),
                }
                match &row[5] {
                    ColumnValues::SmallMoney(v) => assert_eq!(v.int_val, 9999),
                    other => panic!("c_smallmoney expected SmallMoney, got {other:?}"),
                }
                match &row[6] {
                    ColumnValues::Date(v) => assert_eq!(v.get_days(), 730_000),
                    other => panic!("c_date expected Date, got {other:?}"),
                }
                match &row[7] {
                    ColumnValues::Time(v) => {
                        assert_eq!(v.scale, 7);
                        assert_eq!(v.time_nanoseconds, time.time_nanoseconds);
                    }
                    other => panic!("c_time expected Time, got {other:?}"),
                }
                match &row[8] {
                    ColumnValues::DateTimeOffset(v) => {
                        assert_eq!(v.datetime2.days, dto.datetime2.days);
                        assert_eq!(
                            v.datetime2.time.time_nanoseconds,
                            dto.datetime2.time.time_nanoseconds
                        );
                        assert_eq!(v.offset, dto.offset);
                    }
                    other => panic!("c_dto expected DateTimeOffset, got {other:?}"),
                }
                match &row[9] {
                    ColumnValues::DateTime(v) => {
                        assert_eq!(v.days, dt.days);
                        assert_eq!(v.time, dt.time);
                    }
                    other => panic!("c_dt expected DateTime, got {other:?}"),
                }
                match &row[10] {
                    ColumnValues::Numeric(v) => assert_eq!(v, &numeric),
                    other => panic!("c_numeric expected Numeric, got {other:?}"),
                }
                match &row[11] {
                    // Non-Unicode columns decode as LcidBased raw bytes, so
                    // compare the decoded text rather than the SqlString itself.
                    ColumnValues::String(v) => assert_eq!(v.to_utf8_string(), "hello"),
                    other => panic!("c_varchar expected String, got {other:?}"),
                }
                match &row[12] {
                    // CHAR(5) with an exactly-5-char value: no padding ambiguity.
                    ColumnValues::String(v) => assert_eq!(v.to_utf8_string(), "world"),
                    other => panic!("c_char expected String, got {other:?}"),
                }
            },
        )
        .await;
    }

    /// Large row count to force the TVP payload across multiple TDS packets.
    /// Validates aggregate results rather than every row, and proves the
    /// packet-splitting path in the row writer.
    #[tokio::test]
    async fn test_tvp_large_row_count() {
        with_tvp_type(
            "dbo.TvpItLarge",
            "CREATE TYPE dbo.TvpItLarge AS TABLE (id INT, payload NVARCHAR(100))",
            async |client| {
                const N: i32 = 5000;
                let columns = vec![
                    TvpColumnDef::new(SqlType::Int(None)),
                    TvpColumnDef::new(SqlType::NVarchar(None, 100)),
                ];
                let rows = (0..N)
                    .map(|i| {
                        vec![
                            SqlType::Int(Some(i)),
                            nvarchar(&format!("payload-value-row-{i:08}"), 100),
                        ]
                    })
                    .collect();

                let table = TvpTableData::new(columns, rows);
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItLarge".to_string()),
                    Some(table),
                );

                let result = exec_tvp_query(
                    client,
                    "SELECT COUNT(*) AS c, SUM(CAST(id AS BIGINT)) AS s FROM @tvp",
                    vec![param],
                )
                .await;

                assert_eq!(result.len(), 1, "aggregate query returns one row");
                let row = &result[0];
                match &row[0] {
                    ColumnValues::Int(v) => assert_eq!(*v, N),
                    other => panic!("count expected Int, got {other:?}"),
                }
                // Sum of 0..N-1 = N*(N-1)/2.
                let expected_sum = (N as i64) * ((N as i64) - 1) / 2;
                match &row[1] {
                    ColumnValues::BigInt(v) => assert_eq!(*v, expected_sum),
                    other => panic!("sum expected BigInt, got {other:?}"),
                }
            },
        )
        .await;
    }

    /// Negative test: referencing a table type that does not exist must be
    /// rejected by the server. This exercises the full error-propagation path
    /// through `execute_sp_executesql` for a server-side TVP failure, with no
    /// ambiguity about attribution. No type is created (the whole point is that
    /// it is absent), so this test does not use `with_tvp_type`.
    #[tokio::test]
    async fn test_tvp_unknown_type_rejected() {
        let mut client = begin_connection(&build_tcp_datasource()).await;
        // Make sure no orphan from a prior run masks the "does not exist" path.
        try_drop_type(&mut client, "dbo.TvpItDoesNotExist").await;

        let table = TvpTableData::new(
            vec![TvpColumnDef::new(SqlType::Int(None))],
            vec![vec![SqlType::Int(Some(1))]],
        );
        let param = tvp_param(
            "@tvp",
            TvpTypeName::new(Some("dbo".to_string()), "TvpItDoesNotExist".to_string()),
            Some(table),
        );

        let result = try_exec_tvp_query(&mut client, "SELECT * FROM @tvp", vec![param]).await;

        let err = result.expect_err("a TVP referencing a non-existent table type must be rejected");
        // Attribute the failure to the missing type: the server echoes the
        // type name we passed ("Cannot find data type dbo.TvpItDoesNotExist").
        assert!(
            err.contains("TvpItDoesNotExist"),
            "expected a 'cannot find data type' error naming the missing type, got: {err}"
        );
    }

    /// Negative test: the column metadata we serialize is cross-checked by the
    /// server against the registered table type. Here the type has two columns
    /// but we send TVP metadata describing only one, so the server must reject
    /// the request. This is the most valuable structural negative test: a
    /// regression in column-metadata serialization would surface here.
    ///
    /// Note the mismatch is *server-side*: the client's `TvpTableData::validate`
    /// only checks rows against the metadata we send (1 column, 1 value each),
    /// which is internally consistent and therefore passes; it has no knowledge
    /// of the real type's shape.
    #[tokio::test]
    async fn test_tvp_column_count_mismatch_rejected() {
        with_tvp_type(
            "dbo.TvpItColCount",
            "CREATE TYPE dbo.TvpItColCount AS TABLE (id INT, label NVARCHAR(20))",
            async |client| {
                // Send metadata for ONE column against a TWO-column type.
                let table = TvpTableData::new(
                    vec![TvpColumnDef::new(SqlType::Int(None))],
                    vec![vec![SqlType::Int(Some(1))]],
                );
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItColCount".to_string()),
                    Some(table),
                );

                let result = try_exec_tvp_query(client, "SELECT * FROM @tvp", vec![param]).await;

                let err = result.expect_err(
                    "sending column metadata that does not match the registered table \
                     type's column count must be rejected",
                );
                // Attribute the failure to the column-count mismatch (server
                // error 500: "... 1 column(s) where the corresponding
                // user-defined table type requires 2 column(s)").
                assert!(
                    err.to_lowercase().contains("column"),
                    "expected a column-count mismatch error, got: {err}"
                );
            },
        )
        .await;
    }

    /// Negative test: a value longer than the column's declared length is
    /// rejected. Verified empirically, this rejection happens **client-side**
    /// in our serializer (`Usage Error: String length (10 characters) exceeds
    /// schema size (5 characters)`) — the oversized value never reaches the
    /// wire — rather than as a server truncation error. The test pins that
    /// length-guard behavior.
    #[tokio::test]
    async fn test_tvp_string_truncation_rejected() {
        with_tvp_type(
            "dbo.TvpItTrunc",
            "CREATE TYPE dbo.TvpItTrunc AS TABLE (s NVARCHAR(5))",
            async |client| {
                // 10 characters into an NVARCHAR(5) column.
                let table = TvpTableData::new(
                    vec![TvpColumnDef::new(SqlType::NVarchar(None, 5))],
                    vec![vec![nvarchar("0123456789", 5)]],
                );
                let param = tvp_param(
                    "@tvp",
                    TvpTypeName::new(Some("dbo".to_string()), "TvpItTrunc".to_string()),
                    Some(table),
                );

                let result = try_exec_tvp_query(client, "SELECT s FROM @tvp", vec![param]).await;

                let err = result
                    .expect_err("a value exceeding the column's declared length must be rejected");
                // Attribute the failure to the length guard (our serializer's
                // "String length ... exceeds schema size ..." check).
                assert!(
                    err.to_lowercase().contains("length"),
                    "expected a length/truncation error, got: {err}"
                );
            },
        )
        .await;
    }
}
