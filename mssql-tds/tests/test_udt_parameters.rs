// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

/// Integration coverage for the UDT parameter path added to `mssql-tds`.
///
/// These exercise `SqlType::Udt` through `TdsClient` directly, which is how the
/// JS and Python bindings reach it. The ODBC e2e suite covers the same wire
/// path from above, but only through bindings ODBC happens to expose - it
/// cannot reach a caller that builds an `RpcParameter` itself, and it cannot
/// reach the send preflight's rejection arms at all, because `SQLBindParameter`
/// refuses those inputs before `mssql-tds` sees them.
mod udt_parameters {
    use crate::common::{begin_connection, build_tcp_datasource, get_first_row, init_tracing};
    use mssql_tds::connection::tds_client::TdsClient;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sql_udt::UdtTypeName;
    use mssql_tds::datatypes::sqltypes::SqlType;
    use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    /// `hierarchyid` is a system UDT, so it needs no CREATE ASSEMBLY and is
    /// available on any server the suite can reach.
    async fn serialized_hierarchyid(client: &mut TdsClient, path: &str) -> Vec<u8> {
        client
            .execute(
                format!("SELECT CAST(hierarchyid::Parse('{path}') AS VARBINARY(892))"),
                (),
            )
            .await
            .unwrap();
        let (_, row) = get_first_row(client).await.unwrap();
        match &row[0] {
            ColumnValues::Bytes(bytes) => bytes.clone(),
            other => panic!("expected the serialized form, got {other:?}"),
        }
    }

    fn udt_param(name: &str, type_name: UdtTypeName, payload: Option<Vec<u8>>) -> RpcParameter {
        RpcParameter::new(
            Some(name.to_string()),
            StatusFlags::NONE,
            SqlType::Udt(type_name, payload),
        )
    }

    /// The whole round trip: the payload is passed through untouched and the
    /// server resolves the parameter from the three-part name in the header.
    /// A wrong name or a mangled header fails here rather than silently.
    #[tokio::test]
    async fn a_udt_parameter_round_trips_through_sp_executesql() {
        let mut client = begin_connection(&build_tcp_datasource()).await;
        let payload = serialized_hierarchyid(&mut client, "/4/").await;

        client
            .execute_sp_executesql(
                "SELECT @h.ToString()".to_string(),
                vec![udt_param(
                    "@h",
                    UdtTypeName::new(None, None, "hierarchyid".to_string()),
                    Some(payload),
                )],
                (),
            )
            .await
            .unwrap();

        let (_, row) = get_first_row(&mut client).await.unwrap();
        match &row[0] {
            ColumnValues::String(value) => assert_eq!(value.to_string(), "/4/"),
            other => panic!("expected the round-tripped path, got {other:?}"),
        }
    }

    /// A NULL UDT still carries its name: the header names the type whether or
    /// not a payload follows, so the server can type the parameter.
    #[tokio::test]
    async fn a_null_udt_parameter_carries_its_type_name() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        client
            .execute_sp_executesql(
                "SELECT CASE WHEN @h IS NULL THEN 1 ELSE 0 END".to_string(),
                vec![udt_param(
                    "@h",
                    UdtTypeName::new(None, None, "hierarchyid".to_string()),
                    None,
                )],
                (),
            )
            .await
            .unwrap();

        let (_, row) = get_first_row(&mut client).await.unwrap();
        assert_eq!(row[0], ColumnValues::Int(1));
    }

    /// A schema-qualified name has to reach the server as two parts rather
    /// than being flattened or defaulted: `sys.hierarchyid` resolves, and it is
    /// the two-part branch of both the header and the `@params` declaration.
    #[tokio::test]
    async fn a_schema_qualified_udt_name_resolves() {
        let mut client = begin_connection(&build_tcp_datasource()).await;
        let payload = serialized_hierarchyid(&mut client, "/5/").await;

        client
            .execute_sp_executesql(
                "SELECT @h.ToString()".to_string(),
                vec![udt_param(
                    "@h",
                    UdtTypeName::new(None, Some("sys".to_string()), "hierarchyid".to_string()),
                    Some(payload),
                )],
                (),
            )
            .await
            .unwrap();

        let (_, row) = get_first_row(&mut client).await.unwrap();
        match &row[0] {
            ColumnValues::String(value) => assert_eq!(value.to_string(), "/5/"),
            other => panic!("expected the round-tripped path, got {other:?}"),
        }
    }

    /// The send preflight's contract is that locally-invalid input fails
    /// *before* anything reaches the wire, leaving the connection usable. An
    /// empty type name cannot be resolved by the server, so it must be refused
    /// here - and the connection must still serve the next query, which is the
    /// half a unit test on `validate` cannot show.
    ///
    /// Not reachable from the ODBC suite: `SQLBindParameter` rejects a UDT
    /// binding with no `SQL_CA_SS_UDT_TYPE_NAME` before `mssql-tds` is called.
    #[tokio::test]
    async fn an_unnamed_udt_is_refused_before_the_connection_is_used() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let result = client
            .execute_sp_executesql(
                "SELECT @h".to_string(),
                vec![udt_param(
                    "@h",
                    UdtTypeName::new(None, None, String::new()),
                    Some(vec![0x01]),
                )],
                (),
            )
            .await;
        assert!(result.is_err(), "an empty UDT type name must be refused");

        // The RPC never started, so the connection is still good.
        client.execute("SELECT 1".to_string(), ()).await.unwrap();
        let (_, row) = get_first_row(&mut client).await.unwrap();
        assert_eq!(row[0], ColumnValues::Int(1));
    }

    /// The same contract for the other rejection arm: a name part longer than
    /// a B_VARCHAR's `u8` count can express. The preflight exists so this is
    /// caught before the earlier parts fill a packet and flush it, which would
    /// need a cancel-and-drain instead of a local failure.
    #[tokio::test]
    async fn an_overlong_udt_name_part_is_refused_before_the_connection_is_used() {
        let mut client = begin_connection(&build_tcp_datasource()).await;

        let result = client
            .execute_sp_executesql(
                "SELECT @h".to_string(),
                vec![udt_param(
                    "@h",
                    UdtTypeName::new(None, None, "a".repeat(256)),
                    Some(vec![0x01]),
                )],
                (),
            )
            .await;
        assert!(result.is_err(), "a 256-unit name part must be refused");

        client.execute("SELECT 1".to_string(), ()).await.unwrap();
        let (_, row) = get_first_row(&mut client).await.unwrap();
        assert_eq!(row[0], ColumnValues::Int(1));
    }
}
