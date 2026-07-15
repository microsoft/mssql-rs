// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod rpc_results {
    use crate::common::{begin_connection, build_tcp_datasource, get_scalar_value, init_tracing};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient, TdsClient};
    use mssql_tds::datatypes::column_values::{ColumnValues, SqlDateTime2, SqlTime};
    use mssql_tds::datatypes::decoder::DecimalParts;
    use mssql_tds::datatypes::sql_string::SqlString;
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::error::Error;
    use mssql_tds::{
        core::TdsResult,
        message::parameters::rpc_parameters::{RpcParameter, StatusFlags},
        token::tokenitems::ReturnValueStatus,
    };
    use uuid::Uuid;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    #[tokio::test]
    async fn test_stored_proc() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Create a query to setup the stored procedure. This will be a Sql Batch execution.
        let stored_procedure_setup_query = "CREATE PROCEDURE #TempScrollProc
                @InputInt INT,
                @OutputInt INT OUTPUT
            AS
            BEGIN
                SET @OutputInt = @InputInt;
            END;";

        // This should setup the temp stored procedure on this connection.
        execute_non_query(&mut connection, stored_procedure_setup_query.to_string())
            .await
            .unwrap();

        // Do the actual test of the stored procedure.
        let param_value = SqlType::Int(Some(45612));
        let param1 = RpcParameter::new(
            Some("@InputInt".to_string()),
            StatusFlags::NONE,
            param_value,
        );

        let param2 = RpcParameter::new(
            Some("@OutputInt".to_string()),
            StatusFlags::BY_REF_VALUE, // Output parameter
            SqlType::Int(None),        // This is an output parameter. Set to null.
        );

        let named_parameters = vec![param1, param2];

        let stored_procedure_query = "#TempScrollProc";

        connection
            .execute_stored_procedure(
                stored_procedure_query.to_string(),
                None,
                Some(named_parameters),
                None,
                None,
            )
            .await
            .unwrap();

        let returned_parameters = connection.get_return_values();
        assert_eq!(returned_parameters.len(), 1);
        let returned_parameter = returned_parameters.first().unwrap();
        assert_eq!(returned_parameter.param_name, "@OutputInt".to_string());
        assert_eq!(returned_parameter.value, ColumnValues::Int(45612));
        assert_eq!(returned_parameter.status, ReturnValueStatus::OutputParam);
    }

    #[tokio::test]
    async fn test_stored_proc_stream_results() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Create a query to setup the stored procedure. This will be a Sql Batch execution.
        let stored_procedure_setup_query = "CREATE PROCEDURE #TempScrollProc
                @InputInt INT,
                @OutputInt INT OUTPUT
            AS
            BEGIN
                SET @OutputInt = @InputInt;
            END;";

        // This should setup the temp stored procedure on this connection.
        execute_non_query(&mut connection, stored_procedure_setup_query.to_string())
            .await
            .unwrap();

        // Do the actual test of the stored procedure.
        let param_value = SqlType::Int(Some(45612));
        let param1 = RpcParameter::new(
            Some("@InputInt".to_string()),
            StatusFlags::NONE,
            param_value,
        );

        let param2 = RpcParameter::new(
            Some("@OutputInt".to_string()),
            StatusFlags::BY_REF_VALUE, // Output parameter
            SqlType::Int(None),        // This is an output parameter. Set to null.
        );

        let named_parameters = vec![param1, param2];

        let stored_procedure_query = "#TempScrollProc";

        connection
            .execute_stored_procedure(
                stored_procedure_query.to_string(),
                None,
                Some(named_parameters),
                None,
                None,
            )
            .await
            .unwrap();

        // Drain all result sets
        loop {
            if let Some(resultset) = connection.get_current_resultset() {
                while resultset.next_row().await.unwrap().is_some() {}
            }
            if !connection.move_to_next().await.unwrap() {
                break;
            }
        }

        let return_values = connection.get_return_values();
        assert_eq!(return_values.len(), 1);
        let returned_parameter = return_values.first().unwrap();
        assert_eq!(returned_parameter.param_name, "@OutputInt".to_string());
        assert_eq!(returned_parameter.value, ColumnValues::Int(45612));
        assert_eq!(returned_parameter.status, ReturnValueStatus::OutputParam);
    }

    #[tokio::test]
    async fn test_sp_execute_sql_multi_param() {
        let query = "select name from sys.databases where database_id = @database_id and compatibility_level > @compat_level";
        let database_id_param = RpcParameter::new(
            Some("@database_id".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(1)),
        );

        let compat_level_param = RpcParameter::new(
            Some("@compat_level".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(100)),
        );

        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let named_parameters = vec![database_id_param, compat_level_param];

        connection
            .execute_sp_executesql(query.to_string(), named_parameters, None, None)
            .await
            .unwrap();

        let scalar_value = get_scalar_value(&mut connection).await.unwrap();

        if let Some(ColumnValues::String(value)) = scalar_value {
            assert_eq!(value.to_utf8_string(), "master".to_string());
        } else {
            unreachable!("Expected a string value");
        }
    }

    #[tokio::test]
    async fn test_sp_execute_sql_single_param() {
        let query = "select name from sys.databases where database_id = @database_id";
        let database_id_param = RpcParameter::new(
            Some("@database_id".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(1)),
        );

        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let named_parameters = vec![database_id_param];

        connection
            .execute_sp_executesql(query.to_string(), named_parameters, None, None)
            .await
            .unwrap();

        let scalar_value = get_scalar_value(&mut connection).await.unwrap();

        if let Some(ColumnValues::String(value)) = scalar_value {
            assert_eq!(value.to_utf8_string(), "master".to_string());
        } else {
            unreachable!("Expected a string value");
        }
    }

    #[tokio::test]
    async fn test_sp_prepare_and_unprepare_multi_param() {
        let query = "select name from sys.databases where database_id = @database_id and compatibility_level > @compat_level";
        let database_id_param = RpcParameter::new(
            Some("@database_id".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(1)),
        );

        let compat_level_param = RpcParameter::new(
            Some("@compat_level".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(100)),
        );

        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let named_parameters = vec![database_id_param, compat_level_param];

        let handle = connection
            .execute_sp_prepare(query.to_string(), named_parameters, None, None)
            .await
            .unwrap();

        assert!(handle > 0);

        // This should simply complete and be successful.
        let result = connection.execute_sp_unprepare(handle, None, None).await;
        assert!(result.is_ok());
    }

    /// Issue #38 regression: preparing a statement that references a non-int
    /// named parameter (here `@db_name nvarchar(6)`) used to fail with
    /// `ProtocolError("Expected an integer value")` because
    /// `execute_sp_prepare` was forwarding user values onto an RPC whose
    /// signature does not accept them. After the fix the prepare succeeds
    /// and a positive handle is returned.
    #[tokio::test]
    async fn test_sp_prepare_with_named_nvarchar_param_succeeds() {
        let query = "select name from sys.databases where name = @db_name";
        let db_name_param = RpcParameter::new(
            Some("@db_name".to_string()),
            StatusFlags::NONE,
            SqlType::NVarchar(
                Some(SqlString::from_utf8_string("master".to_string())),
                6_u16,
            ),
        );

        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let handle = connection
            .execute_sp_prepare(query.to_string(), vec![db_name_param], None, None)
            .await
            .expect("sp_prepare should succeed for an NVARCHAR-parameterized statement");

        assert!(handle > 0, "expected a positive prepared statement handle");

        connection
            .execute_sp_unprepare(handle, None, None)
            .await
            .expect("sp_unprepare should succeed");
    }

    /// Mirrors `test_sp_prepare_surfaces_server_error_on_invalid_sql` for the
    /// `sp_unprepare` path: passing a handle that was never produced by
    /// `sp_prepare` makes the server return msg 8179 ("Could not find prepared
    /// statement with handle ..."). After the Issue #38 fix sweep, those
    /// drained errors are surfaced as `Error::SqlServerError` instead of being
    /// silently discarded.
    #[tokio::test]
    async fn test_sp_unprepare_surfaces_server_error_on_invalid_handle() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let res = connection.execute_sp_unprepare(0, None, None).await;

        match res {
            Ok(()) => panic!("Expected sp_unprepare to fail for handle 0"),
            Err(Error::SqlServerError { diagnostics }) => {
                assert!(
                    diagnostics
                        .errors
                        .iter()
                        .any(|e| e.number != 0 && !e.message.is_empty()),
                    "expected at least one populated server error, got: {:?}",
                    diagnostics.errors,
                );
            }
            Err(other) => panic!(
                "Expected SqlServerError carrying the server diagnostic, got: {}",
                other
            ),
        }
    }

    /// Verifies the diagnostics improvement that landed with the Issue #38
    /// fix: when the server returns an ERROR token during `sp_prepare`, the
    /// call surfaces it as `Error::SqlServerError` carrying the actual server
    /// message instead of a generic `ProtocolError`.
    #[tokio::test]
    async fn test_sp_prepare_surfaces_server_error_on_invalid_sql() {
        // References an undeclared parameter; the server rejects this during
        // prepare with msg 137 ("Must declare the scalar variable...").
        let invalid_sql = "select * from sys.databases where database_id = @missing";

        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let res = connection
            .execute_sp_prepare(invalid_sql.to_string(), vec![], None, None)
            .await;

        match res {
            Ok(handle) => panic!("Expected sp_prepare to fail; got handle {}", handle),
            Err(Error::SqlServerError { diagnostics }) => {
                assert!(
                    diagnostics
                        .errors
                        .iter()
                        .any(|e| e.number != 0 && !e.message.is_empty()),
                    "expected at least one populated server error, got: {:?}",
                    diagnostics.errors,
                );
            }
            Err(other) => panic!(
                "Expected SqlServerError carrying the server diagnostic, got: {}",
                other
            ),
        }
    }

    /// End-to-end coverage for `sp_prepare` + `sp_execute` across a mix of
    /// parameter types. Creates a session-scoped temp table, populates a
    /// single row whose values are sent through `sp_executesql` (so the wire
    /// encoding round-trips), then for each column runs a
    /// `sp_prepare` -> `sp_execute` -> `sp_unprepare` cycle that filters by
    /// that column with a parameter of the matching `SqlType`. Each cycle
    /// asserts the row's id is returned, proving both the prepare-side
    /// `@params` declaration and the execute-side parameter encoding work
    /// for that type.
    ///
    /// Types deliberately not covered here:
    /// - `Char(_, N)` and `NChar(_, N)`: the parameter-declaration string
    ///   currently drops the length suffix; tracked separately as the
    ///   parameter-declaration alignment work.
    #[tokio::test]
    async fn test_sp_prepare_then_execute_against_temp_table() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let create_sql = "create table #prepare_param_tests (
            id        int             not null,
            nvc       nvarchar(50)    null,
            nvm       nvarchar(max)   null,
            amt       decimal(18, 4)  null,
            dt2       datetime2(7)    null,
            t         time(7)         null,
            uid       uniqueidentifier null,
            raw       varbinary(16)   null,
            rawmax    varbinary(max)  null,
            big       bigint          null,
            sml       smallint        null,
            tny       tinyint         null,
            flag      bit             null,
            flt       float           null
        )";
        execute_non_query(&mut connection, create_sql.to_string())
            .await
            .unwrap();

        let nvc_val = SqlString::from_utf8_string("alpha".to_string());
        let nvm_val = SqlString::from_utf8_string(
            "long unicode value that exceeds typical inline length thresholds".to_string(),
        );
        let amt_val = DecimalParts::from_string("12345.6789", 18, 4).unwrap();
        let dt2_val = SqlDateTime2 {
            days: 737_438,
            time: SqlTime {
                time_nanoseconds: 372_300_000_000,
                scale: 7,
            },
        };
        let t_val = SqlTime {
            time_nanoseconds: 432_000_000_000,
            scale: 7,
        };
        let uid_val = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let raw_val: Vec<u8> = vec![0x00, 0x01, 0xFE, 0xFF];
        let rawmax_val: Vec<u8> = vec![0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80];
        let big_val: i64 = 9_876_543_210;
        let sml_val: i16 = -1234;
        let tny_val: u8 = 200;
        let flag_val: bool = true;
        let flt_val: f64 = 12_345.678_901_234_567;

        let make_param = |name: &str, value: SqlType| -> RpcParameter {
            RpcParameter::new(Some(name.to_string()), StatusFlags::NONE, value)
        };

        let insert_sql = "insert into #prepare_param_tests
            (id, nvc, nvm, amt, dt2, t, uid, raw, rawmax, big, sml, tny, flag, flt)
            values
            (1, @nvc, @nvm, @amt, @dt2, @t, @uid, @raw, @rawmax, @big, @sml, @tny, @flag, @flt)";

        let insert_params = vec![
            make_param("@nvc", SqlType::NVarchar(Some(nvc_val.clone()), 50)),
            make_param("@nvm", SqlType::NVarcharMax(Some(nvm_val.clone()))),
            make_param("@amt", SqlType::Decimal(Some(amt_val.clone()))),
            make_param("@dt2", SqlType::DateTime2(Some(dt2_val.clone()))),
            make_param("@t", SqlType::Time(Some(t_val.clone()))),
            make_param("@uid", SqlType::Uuid(Some(uid_val))),
            make_param("@raw", SqlType::VarBinary(Some(raw_val.clone()), 16)),
            make_param("@rawmax", SqlType::VarBinaryMax(Some(rawmax_val.clone()))),
            make_param("@big", SqlType::BigInt(Some(big_val))),
            make_param("@sml", SqlType::SmallInt(Some(sml_val))),
            make_param("@tny", SqlType::TinyInt(Some(tny_val))),
            make_param("@flag", SqlType::Bit(Some(flag_val))),
            make_param("@flt", SqlType::Float(Some(flt_val))),
        ];

        connection
            .execute_sp_executesql(insert_sql.to_string(), insert_params, None, None)
            .await
            .expect("seed row insert via sp_executesql");

        let cases: Vec<(&str, &str, SqlType)> = vec![
            ("nvc", "@nvc", SqlType::NVarchar(Some(nvc_val.clone()), 50)),
            ("nvm", "@nvm", SqlType::NVarcharMax(Some(nvm_val.clone()))),
            ("amt", "@amt", SqlType::Decimal(Some(amt_val.clone()))),
            ("dt2", "@dt2", SqlType::DateTime2(Some(dt2_val.clone()))),
            ("t", "@t", SqlType::Time(Some(t_val.clone()))),
            ("uid", "@uid", SqlType::Uuid(Some(uid_val))),
            ("raw", "@raw", SqlType::VarBinary(Some(raw_val.clone()), 16)),
            (
                "rawmax",
                "@rawmax",
                SqlType::VarBinaryMax(Some(rawmax_val.clone())),
            ),
            ("big", "@big", SqlType::BigInt(Some(big_val))),
            ("sml", "@sml", SqlType::SmallInt(Some(sml_val))),
            ("tny", "@tny", SqlType::TinyInt(Some(tny_val))),
            ("flag", "@flag", SqlType::Bit(Some(flag_val))),
            ("flt", "@flt", SqlType::Float(Some(flt_val))),
        ];

        for (column, param_name, value) in cases {
            let id = prepare_select_and_fetch_id(&mut connection, column, param_name, value).await;
            assert_eq!(
                id, 1,
                "filter on column `{column}` via sp_prepare/sp_execute should have returned id=1"
            );
        }
    }

    /// Runs the full prepare/execute/unprepare cycle for a single
    /// `WHERE <column> = <param>` filter against `#prepare_param_tests`
    /// and returns the scalar id from the matched row.
    async fn prepare_select_and_fetch_id(
        connection: &mut TdsClient,
        column: &str,
        param_name: &str,
        value: SqlType,
    ) -> i32 {
        let sql = format!(
            "select id from #prepare_param_tests where {column} = {param_name}",
            column = column,
            param_name = param_name,
        );
        let param = RpcParameter::new(Some(param_name.to_string()), StatusFlags::NONE, value);

        let handle = connection
            .execute_sp_prepare(sql.clone(), vec![param.clone()], None, None)
            .await
            .unwrap_or_else(|err| {
                panic!("sp_prepare failed for column `{column}`: {err}");
            });
        assert!(
            handle > 0,
            "expected positive prepared handle for column `{column}`"
        );

        connection
            .execute_sp_execute(handle, None, Some(vec![param]), None, None)
            .await
            .unwrap_or_else(|err| {
                panic!("sp_execute failed for column `{column}`: {err}");
            });

        let scalar = get_scalar_value(connection)
            .await
            .unwrap_or_else(|err| panic!("reading scalar for column `{column}` failed: {err}"));

        let id = match scalar {
            Some(ColumnValues::Int(value)) => value,
            other => panic!("expected Int scalar for column `{column}`, got {:?}", other),
        };

        connection
            .execute_sp_unprepare(handle, None, None)
            .await
            .unwrap_or_else(|err| {
                panic!("sp_unprepare failed for column `{column}`: {err}");
            });

        id
    }

    #[tokio::test]
    async fn test_sp_prepareexec_and_unprepare_multi_param() {
        let query = "select name from sys.databases where database_id = @database_id and compatibility_level > @compat_level";
        let database_id_param = RpcParameter::new(
            Some("@database_id".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(1)),
        );

        let compat_level_param = RpcParameter::new(
            Some("@compat_level".to_string()),
            StatusFlags::NONE,
            SqlType::Int(Some(100)),
        );

        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let named_parameters = vec![database_id_param, compat_level_param];

        connection
            .execute_sp_prepexec(
                query.to_string(),
                named_parameters.clone(),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        // Read the result set
        if let Some(resultset) = connection.get_current_resultset() {
            while resultset.next_row().await.unwrap().is_some() {}
        }

        // Move to next result set to consume remaining tokens (including return values)
        connection.move_to_next().await.unwrap();

        // Get the prepared handle from output params
        let out_params = connection.retrieve_output_params().unwrap();
        assert!(out_params.is_some());
        let out_params = out_params.unwrap();
        assert_eq!(out_params.len(), 1);

        let handle_param = out_params.first().unwrap();
        let retrieved_handle = if let ColumnValues::Int(handle) = handle_param.value {
            assert!(handle > 0);
            handle
        } else {
            unreachable!("Expected a handle value");
        };
        assert_eq!(handle_param.status, ReturnValueStatus::OutputParam);

        // Execute the prepared statement again
        connection
            .execute_sp_execute(retrieved_handle, None, Some(named_parameters), None, None)
            .await
            .unwrap();

        let scalar_value = get_scalar_value(&mut connection).await.unwrap();
        if let Some(ColumnValues::String(value)) = scalar_value {
            assert_eq!(value.to_utf8_string(), "master".to_string());
        } else {
            unreachable!("Expected a string value");
        }

        let result = connection
            .execute_sp_unprepare(retrieved_handle, None, None)
            .await;
        assert!(result.is_ok());
    }

    // Prepares and executes `sql` via sp_prepexec, drains the result set, and
    // returns the server-assigned prepared-statement handle. `drop_handle`
    // piggybacks a release of a prior handle onto the prepare (sent as the
    // `@handle` input).
    async fn prepexec_and_get_handle(
        connection: &mut mssql_tds::connection::tds_client::TdsClient,
        sql: &str,
        drop_handle: Option<i32>,
    ) -> i32 {
        connection
            .execute_sp_prepexec(sql.to_string(), vec![], drop_handle, None, None)
            .await
            .unwrap();

        if let Some(resultset) = connection.get_current_resultset() {
            while resultset.next_row().await.unwrap().is_some() {}
        }
        connection.move_to_next().await.unwrap();

        let out_params = connection.retrieve_output_params().unwrap().unwrap();
        match out_params.first().unwrap().value {
            ColumnValues::Int(handle) => handle,
            ref other => unreachable!("expected Int handle, got {other:?}"),
        }
    }

    // The sp_prepexec `@handle` piggyback: passing a prior prepared handle as
    // the `@handle` input makes the server drop that plan and re-prepare in the
    // same round trip, replacing a separate sp_unprepare. This validates that
    // the server accepts the input handle (i.e. treats `@handle` as in/out, not
    // pure output) and that the returned handle runs the new statement.
    // (Whether the freed plan number is reused is server-internal, so the test
    // does not assume `h1 != h2`.)
    #[tokio::test]
    async fn test_sp_prepexec_piggyback_reprepares_with_prior_handle() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // First prepare+execute → handle h1.
        let h1 = prepexec_and_get_handle(&mut connection, "SELECT 1 AS v", None).await;
        assert!(h1 > 0);

        // Re-prepare with new text, passing h1 as the `@handle` input so the
        // server drops h1's plan and prepares "SELECT 2" in one RPC.
        let h2 = prepexec_and_get_handle(&mut connection, "SELECT 2 AS v", Some(h1)).await;
        assert!(h2 > 0);

        // The returned handle runs the NEW statement.
        connection
            .execute_sp_execute(h2, None, None, None, None)
            .await
            .unwrap();
        let scalar = get_scalar_value(&mut connection).await.unwrap();
        assert!(
            matches!(scalar, Some(ColumnValues::Int(2))),
            "re-prepared handle should run SELECT 2, got {scalar:?}"
        );

        connection
            .execute_sp_unprepare(h2, None, None)
            .await
            .unwrap();
    }

    // Executes the query and reads till the end of the result.
    async fn execute_non_query(
        connection: &mut mssql_tds::connection::tds_client::TdsClient,
        query: String,
    ) -> TdsResult<()> {
        connection.execute(query, None, None).await?;

        // Drain all result sets
        loop {
            if let Some(resultset) = connection.get_current_resultset() {
                while resultset.next_row().await?.is_some() {}
            }
            if !connection.move_to_next().await? {
                break;
            }
        }

        Ok(())
    }
}
