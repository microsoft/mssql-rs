// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Tiberius public data API contract from saurabh500/tiberius@e01b6a4.
//!
//! Connection construction is intentionally bridge-native. Every other live
//! assertion states data behavior from `tests/data_api.rs`, `tests/query.rs`,
//! or `tests/bulk.rs` at that commit.

pub mod common;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use mssql_rs::bulk::BulkLoadRow;
use mssql_rs::compat::FromSql as CompatFromSql;
use mssql_rs::{
    Client, ColumnData, ColumnType, Error, ExecuteResult, FromSql, FromSqlOwned, IntoRow, IntoSql,
    Query, QueryItem, Row, ToSql,
};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Status {
    UnchangedPass,
    AdaptedPass,
    CompileGap,
    BehavioralGap,
    IntentionalImprovement,
}

struct Trace {
    behavior: &'static str,
    source: &'static str,
    status: Status,
    evidence: &'static str,
}

const TRACEABILITY: &[Trace] = &[
    Trace {
        behavior: "primitive value and NULL conversions",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::AdaptedPass,
        evidence: "tiberius_compat::{bool_type,u8_token,i16_token,i32_token,i64_token,f32_token,f64_token}",
    },
    Trace {
        behavior: "owned and borrowed strings",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::AdaptedPass,
        evidence: "tiberius_compat::{string_roundtrip,str_borrow_from_row,str_borrow_null}",
    },
    Trace {
        behavior: "owned and borrowed binary",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::AdaptedPass,
        evidence: "tiberius_compat::{varbinary_roundtrip,vec_u8_param_roundtrip}",
    },
    Trace {
        behavior: "UUID conversion",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::AdaptedPass,
        evidence: "tiberius_compat::uuid_roundtrip",
    },
    Trace {
        behavior: "numeric conversion",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::AdaptedPass,
        evidence: "tiberius_compat::{decimal_roundtrip,numeric_large}",
    },
    Trace {
        behavior: "schema-bearing XmlData conversion",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::CompileGap,
        evidence: "ColumnData::Xml supports raw row/parameter values, but no Tiberius-compatible XmlData wrapper",
    },
    Trace {
        behavior: "chrono conversions",
        source: "data_api.rs:chrono_values_round_trip_through_public_conversions",
        status: Status::AdaptedPass,
        evidence: "tiberius_compat::{naive_date_time,naive_date,naive_time,datetime_offset}",
    },
    Trace {
        behavior: "time crate conversions",
        source: "data_api.rs:time_values_round_trip_through_public_conversions",
        status: Status::AdaptedPass,
        evidence: "query::tests::time_temporals_roundtrip_with_fractional_seconds_and_offset",
    },
    Trace {
        behavior: "typed NULL parameter preservation",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::AdaptedPass,
        evidence: "typed_null_parameter_preserves_column_type",
    },
    Trace {
        behavior: "FromSqlOwned conversions",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::AdaptedPass,
        evidence: "compat_conversion_traits_cover_values_nulls_and_errors; pass/data_api_conversions.rs (#128)",
    },
    Trace {
        behavior: "FromSql conversion error channel",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::AdaptedPass,
        evidence: "compat_conversion_traits_cover_values_nulls_and_errors; pass/data_api_conversion_errors.rs (#128)",
    },
    Trace {
        behavior: "ColumnData and IntoSql",
        source: "data_api.rs:public_value_conversions_cover_values_nulls_and_errors",
        status: Status::AdaptedPass,
        evidence: "compat_conversion_traits_cover_values_nulls_and_errors; pass/data_api_conversions.rs (#128)",
    },
    Trace {
        behavior: "Column name and type metadata",
        source: "data_api.rs:public_numeric_time_xml_and_token_row_helpers",
        status: Status::UnchangedPass,
        evidence: "row_metadata_access_and_errors",
    },
    Trace {
        behavior: "Row named access",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::UnchangedPass,
        evidence: "row_metadata_access_and_errors",
    },
    Trace {
        behavior: "Row numeric index access",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::UnchangedPass,
        evidence: "row_metadata_access_and_errors",
    },
    Trace {
        behavior: "duplicate names select first",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::UnchangedPass,
        evidence: "row_metadata_access_and_errors",
    },
    Trace {
        behavior: "missing named try_get is recoverable",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::UnchangedPass,
        evidence: "row_metadata_access_and_errors",
    },
    Trace {
        behavior: "out-of-range numeric try_get is recoverable",
        source: "DATA_API_COVERAGE.md:Compatibility observations",
        status: Status::IntentionalImprovement,
        evidence: "row_metadata_access_and_errors",
    },
    Trace {
        behavior: "NULL row retrieval",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::UnchangedPass,
        evidence: "row_metadata_access_and_errors",
    },
    Trace {
        behavior: "wrong-type retrieval reports conversion error",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::AdaptedPass,
        evidence: "row_metadata_access_and_errors through Row::try_get_compat",
    },
    Trace {
        behavior: "Row cells and consuming iterator",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::UnchangedPass,
        evidence: "row_iteration_preserves_column_order_and_nulls; pass/data_api_row_iteration.rs (#130)",
    },
    Trace {
        behavior: "Row result_index",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::AdaptedPass,
        evidence: "query_stream_metadata_order_and_result_indexes",
    },
    Trace {
        behavior: "QueryItem metadata and row variants",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::AdaptedPass,
        evidence: "query_stream_metadata_order_and_result_indexes",
    },
    Trace {
        behavior: "QueryStream columns metadata",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::AdaptedPass,
        evidence: "query_stream_metadata_order_and_result_indexes",
    },
    Trace {
        behavior: "into_row_stream collection",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::AdaptedPass,
        evidence: "row_stream_collects_all_sets",
    },
    Trace {
        behavior: "into_results non-empty sets",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::AdaptedPass,
        evidence: "collected_results_preserve_empty_middle_set",
    },
    Trace {
        behavior: "empty result-set boundaries",
        source: "DATA_API_COVERAGE.md:Compatibility observations",
        status: Status::IntentionalImprovement,
        evidence: "collected_results_preserve_empty_middle_set",
    },
    Trace {
        behavior: "into_first_result",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::AdaptedPass,
        evidence: "tiberius_compat::multiple_result_sets",
    },
    Trace {
        behavior: "async into_row helper",
        source: "data_api.rs:row_metadata_stream_and_collection_behavior",
        status: Status::AdaptedPass,
        evidence: "query_stream_collectors_match_tiberius_shape",
    },
    Trace {
        behavior: "ExecuteResult total",
        source: "tests/query.rs:execute_multiple_count_total",
        status: Status::AdaptedPass,
        evidence: "execute_counts_iterate_and_total",
    },
    Trace {
        behavior: "ExecuteResult inherent into_iter",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::UnchangedPass,
        evidence: "execute_counts_iterate_and_total",
    },
    Trace {
        behavior: "ExecuteResult rows_affected",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::UnchangedPass,
        evidence: "execute_result_preserves_order_zero_counts_and_iteration; pass/data_api_execute_rows_affected.rs (#131)",
    },
    Trace {
        behavior: "ExecuteResult IntoIterator trait",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::UnchangedPass,
        evidence: "execute_result_preserves_order_zero_counts_and_iteration; pass/data_api_execute_into_iterator.rs (#131)",
    },
    Trace {
        behavior: "dynamic Query binding",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::UnchangedPass,
        evidence: "dynamic_query_builder_*; pass/data_api_query_builder.rs (#127)",
    },
    Trace {
        behavior: "dynamic NULL binding through Client::query",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::AdaptedPass,
        evidence: "typed_null_parameter_preserves_column_type",
    },
    Trace {
        behavior: "bulk nullable-row success and count",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::AdaptedPass,
        evidence: "bulk_nullable_rows_succeed",
    },
    Trace {
        behavior: "TokenRow helpers",
        source: "data_api.rs:public_numeric_time_xml_and_token_row_helpers",
        status: Status::UnchangedPass,
        evidence: "bulk::tests::token_row_helpers_and_tuple_arities_preserve_values; pass/data_api_bulk_row.rs (#129)",
    },
    Trace {
        behavior: "IntoRow tuple arities 1 through 10",
        source: "data_api.rs:public_numeric_time_xml_and_token_row_helpers",
        status: Status::UnchangedPass,
        evidence: "compat_bulk_tuple_arities_finalize_and_reset; pass/data_api_bulk_row.rs (#129)",
    },
    Trace {
        behavior: "bulk send/finalize lifecycle",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::UnchangedPass,
        evidence: "compat_bulk_incremental_lifecycle_validates_and_runs_twice; pass/data_api_bulk_lifecycle.rs (#129)",
    },
    Trace {
        behavior: "bulk oversized value returns BulkInput",
        source: "data_api.rs:execute_dynamic_query_and_bulk_behavior",
        status: Status::AdaptedPass,
        evidence: "compat_bulk_oversized_value_is_explicit_input_error (#129)",
    },
];

async fn connect() -> Option<Client> {
    let config = common::live_config()?;
    Some(
        Client::connect(&config)
            .await
            .expect("connect to compatibility SQL Server"),
    )
}

#[test]
fn phase_1_traceability_is_complete_and_stable() {
    assert_eq!(TRACEABILITY.len(), 40);
    assert_eq!(
        TRACEABILITY
            .iter()
            .filter(|trace| trace.status == Status::UnchangedPass)
            .count(),
        14
    );
    assert_eq!(
        TRACEABILITY
            .iter()
            .filter(|trace| trace.status == Status::AdaptedPass)
            .count(),
        23
    );
    assert_eq!(
        TRACEABILITY
            .iter()
            .filter(|trace| trace.status == Status::CompileGap)
            .count(),
        1
    );
    assert_eq!(
        TRACEABILITY
            .iter()
            .filter(|trace| trace.status == Status::BehavioralGap)
            .count(),
        0
    );
    assert_eq!(
        TRACEABILITY
            .iter()
            .filter(|trace| trace.status == Status::IntentionalImprovement)
            .count(),
        2
    );
    for trace in TRACEABILITY {
        assert!(!trace.behavior.is_empty());
        assert!(!trace.source.is_empty());
        assert!(!trace.evidence.is_empty());
    }
}

#[tokio::test]
async fn row_metadata_access_and_errors() {
    let Some(mut client) = connect().await else {
        return;
    };
    let rows = client
        .simple_query(
            "SELECT CAST(7 AS int) AS duplicate, CAST(NULL AS int) AS nullable, \
             CAST(8 AS int) AS duplicate, CAST(9 AS int) AS unique_name",
        )
        .await
        .expect("query row contract")
        .into_first_result();
    let row = rows.first().expect("one result row");
    let first_column = row.columns().first().expect("first column metadata");

    assert_eq!(first_column.name(), "duplicate");
    assert_eq!(first_column.column_type(), ColumnType::Int4);
    assert_eq!(row.get::<i32, _>(0usize), Some(7));
    assert_eq!(row.get::<i32, _>("duplicate"), Some(7));
    assert_eq!(row.get::<i32, _>("unique_name"), Some(9));
    assert_eq!(row.get::<i32, _>("nullable"), None);
    assert_eq!(row.get::<Option<i32>, _>("nullable"), Some(None));
    assert_eq!(row.get::<&str, _>("duplicate"), None);
    assert_eq!(
        row.try_get::<&str, _>("duplicate")
            .expect("native mismatch remains None"),
        None
    );
    assert!(matches!(
        row.try_get_compat::<&str, _>("duplicate"),
        Err(Error::Conversion(_))
    ));
    assert!(matches!(
        row.try_get::<i32, _>("missing"),
        Err(Error::ColumnNotFound(name)) if name == "missing"
    ));
    assert!(matches!(
        row.try_get::<i32, _>(99usize),
        Err(Error::ColumnIndexOutOfBounds {
            index: 99,
            count: 4
        })
    ));
}

#[tokio::test]
async fn row_iteration_preserves_column_order_and_nulls() {
    let Some(mut client) = connect().await else {
        return;
    };
    let rows = client
        .simple_query(
            "SELECT CAST(7 AS int) AS first_value, CAST(NULL AS int) AS nullable, \
             CAST('last' AS nvarchar(12)) AS final_value",
        )
        .await
        .expect("query row iteration contract")
        .into_first_result();
    let row = rows.first().expect("one result row");

    let cells = row
        .cells()
        .map(|(column, value)| (column.name().to_string(), value))
        .collect::<Vec<_>>();
    assert_eq!(
        cells
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["first_value", "nullable", "final_value"]
    );
    let mut cells = cells.into_iter();
    assert!(matches!(
        cells.next().map(|(_, value)| value),
        Some(ColumnData::I32(Some(7)))
    ));
    assert!(matches!(
        cells.next().map(|(_, value)| value),
        Some(ColumnData::I32(None))
    ));
    assert!(matches!(
        cells.next().map(|(_, value)| value),
        Some(ColumnData::String(Some(value))) if value == "last"
    ));
    assert!(cells.next().is_none());

    let values = row.clone().into_iter().collect::<Vec<_>>();
    assert_eq!(values.len(), row.columns().len());
    let mut values = values.into_iter();
    assert!(matches!(values.next(), Some(ColumnData::I32(Some(7)))));
    assert!(matches!(values.next(), Some(ColumnData::I32(None))));
    assert!(matches!(
        values.next(),
        Some(ColumnData::String(Some(value))) if value == "last"
    ));
    assert!(values.next().is_none());

    assert_eq!(row.get::<i32, _>(0usize), Some(7));
    assert_eq!(row.get::<i32, _>("first_value"), Some(7));
    assert_eq!(row.get::<Option<i32>, _>("nullable"), Some(None));
    assert!(matches!(
        row.try_get::<i32, _>(3usize),
        Err(Error::ColumnIndexOutOfBounds { index: 3, count: 3 })
    ));
}

#[tokio::test]
async fn collected_results_preserve_empty_middle_set() {
    let Some(mut client) = connect().await else {
        return;
    };
    let result = client
        .simple_query(
            "SELECT CAST(7 AS int) AS first_value; \
             SELECT CAST(1 AS int) AS empty_value WHERE 1 = 0; \
             SELECT CAST(9 AS int) AS final_value",
        )
        .await
        .expect("query three result sets");
    assert_eq!(result.result_set_count(), 3);
    let results = result.into_results();
    assert_eq!(
        results.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![1, 0, 1]
    );
    let first = results
        .first()
        .and_then(|rows| rows.first())
        .expect("first result row");
    let final_row = results
        .get(2)
        .and_then(|rows| rows.first())
        .expect("final result row");
    assert_eq!(first.get::<i32, _>("first_value"), Some(7));
    assert_eq!(final_row.get::<i32, _>("final_value"), Some(9));
}

#[tokio::test]
async fn row_stream_collects_all_sets() {
    let Some(mut client) = connect().await else {
        return;
    };
    let rows: Vec<Row> = client
        .simple_query("SELECT 1 AS value; SELECT 2 AS value")
        .await
        .expect("query stream source")
        .into_row_stream()
        .try_collect()
        .await
        .expect("collect row stream");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.first()
            .expect("first streamed row")
            .get::<i32, _>("value"),
        Some(1)
    );
    assert_eq!(
        rows.get(1)
            .expect("second streamed row")
            .get::<i32, _>("value"),
        Some(2)
    );
}

#[tokio::test]
async fn query_stream_metadata_order_and_result_indexes() {
    let Some(mut client) = connect().await else {
        return;
    };
    let mut stream = client.simple_query_compat(
        "SELECT CAST(7 AS int) AS first_value; \
         SELECT CAST('unused' AS nvarchar(12)) AS empty_value WHERE 1 = 0; \
         SELECT CAST(9 AS bigint) AS final_value",
    );

    let first_columns = stream
        .columns()
        .await
        .expect("read first result metadata")
        .expect("first result metadata exists");
    let first_column = first_columns.first().expect("first result column exists");
    assert_eq!(first_column.name(), "first_value");
    assert_eq!(first_column.column_type(), ColumnType::Int4);

    let mut observed = Vec::new();
    while let Some(item) = stream.next().await {
        match item.expect("read compatibility stream item") {
            QueryItem::Metadata(metadata) => {
                observed.push(("metadata", metadata.result_index()));
                let expected = match metadata.result_index() {
                    0 => ("first_value", ColumnType::Int4),
                    1 => ("empty_value", ColumnType::NVarchar),
                    2 => ("final_value", ColumnType::Int8),
                    _ => ("unexpected", ColumnType::Null),
                };
                let column = metadata
                    .columns()
                    .first()
                    .expect("result metadata column exists");
                assert_eq!(column.name(), expected.0);
                assert_eq!(column.column_type(), expected.1);
            }
            QueryItem::Row(row) => {
                observed.push(("row", row.result_index()));
                if row.result_index() == 0 {
                    assert_eq!(row.get::<&str, _>("first_value"), None);
                    assert_eq!(
                        row.try_get::<&str, _>("first_value")
                            .expect("native mismatch remains None"),
                        None
                    );
                    assert!(matches!(
                        row.try_get_compat::<&str, _>("first_value"),
                        Err(Error::Conversion(_))
                    ));
                    assert!(matches!(
                        row.try_get_compat::<i32, _>("first_value"),
                        Ok(Some(7))
                    ));
                }
            }
        }
    }
    assert_eq!(
        observed,
        [
            ("metadata", 0),
            ("row", 0),
            ("metadata", 1),
            ("metadata", 2),
            ("row", 2),
        ]
    );
}

#[tokio::test]
async fn query_stream_collectors_match_tiberius_shape() {
    let Some(mut client) = connect().await else {
        return;
    };
    let sql = "SELECT CAST(7 AS int) AS value; \
               SELECT CAST(0 AS int) AS value WHERE 1 = 0; \
               SELECT CAST(9 AS int) AS value";

    let results = client
        .simple_query_compat(sql)
        .into_results()
        .await
        .expect("collect all compatibility results");
    assert_eq!(results.iter().map(Vec::len).collect::<Vec<_>>(), [1, 0, 1]);
    assert_eq!(
        results
            .first()
            .and_then(|rows| rows.first())
            .expect("first result row exists")
            .result_index(),
        0
    );
    assert_eq!(
        results
            .get(2)
            .and_then(|rows| rows.first())
            .expect("third result row exists")
            .result_index(),
        2
    );

    let empty_results = client
        .simple_query_compat(
            "SELECT CAST(0 AS int) AS first_empty WHERE 1 = 0; \
             SELECT CAST(8 AS int) AS value; \
             SELECT CAST(0 AS int) AS trailing_empty WHERE 1 = 0",
        )
        .into_results()
        .await
        .expect("collect empty compatibility results");
    assert_eq!(
        empty_results.iter().map(Vec::len).collect::<Vec<_>>(),
        [0, 1, 0]
    );

    let mut empty_stream = client.simple_query_compat(
        "SELECT CAST(0 AS int) AS first_empty WHERE 1 = 0; \
         SELECT CAST('' AS nvarchar(8)) AS second_empty WHERE 1 = 0",
    );
    let first_empty = empty_stream
        .columns()
        .await
        .expect("read first empty metadata")
        .and_then(|columns| columns.first())
        .expect("first empty column");
    assert_eq!(first_empty.name(), "first_empty");
    assert_eq!(first_empty.column_type(), ColumnType::Int4);
    assert!(matches!(
        empty_stream.next().await,
        Some(Ok(QueryItem::Metadata(metadata)))
            if metadata.result_index() == 0
                && metadata.columns().first().is_some_and(|column| {
                    column.name() == "first_empty" && column.column_type() == ColumnType::Int4
                })
    ));
    let second_empty = empty_stream
        .columns()
        .await
        .expect("read second empty metadata")
        .and_then(|columns| columns.first())
        .expect("second empty column");
    assert_eq!(second_empty.name(), "second_empty");
    assert_eq!(second_empty.column_type(), ColumnType::NVarchar);
    assert!(matches!(
        empty_stream.next().await,
        Some(Ok(QueryItem::Metadata(metadata)))
            if metadata.result_index() == 1
                && metadata.columns().first().is_some_and(|column| {
                    column.name() == "second_empty"
                        && column.column_type() == ColumnType::NVarchar
                })
    ));
    drop(empty_stream);

    let first = client
        .simple_query_compat(sql)
        .into_first_result()
        .await
        .expect("collect first compatibility result");
    assert_eq!(first.len(), 1);
    assert_eq!(
        first
            .first()
            .expect("first compatibility row exists")
            .get::<i32, _>("value"),
        Some(7)
    );

    let row = client
        .simple_query_compat(sql)
        .into_row()
        .await
        .expect("collect one compatibility row")
        .expect("first row exists");
    assert_eq!(row.get::<i32, _>("value"), Some(7));

    let rows: Vec<Row> = client
        .simple_query_compat(sql)
        .into_row_stream()
        .try_collect()
        .await
        .expect("flatten compatibility rows");
    assert_eq!(
        rows.iter()
            .map(|row| (row.result_index(), row.get::<i32, _>("value")))
            .collect::<Vec<_>>(),
        [(0, Some(7)), (2, Some(9))]
    );
}

#[tokio::test]
async fn execute_counts_iterate_and_total() {
    let Some(mut client) = connect().await else {
        return;
    };
    client
        .simple_query("CREATE TABLE #compat_counts (id int)")
        .await
        .expect("create count table");
    let result = client
        .execute(
            "INSERT INTO #compat_counts VALUES (1), (2); \
             UPDATE #compat_counts SET id = id + 1; \
             DELETE FROM #compat_counts WHERE id = 99",
            &[],
        )
        .await
        .expect("execute count batch");
    assert_eq!(result.total(), 4);
    assert_eq!(result.into_iter().collect::<Vec<_>>(), vec![2, 2, 0]);
}

#[tokio::test]
async fn typed_null_parameter_preserves_column_type() {
    let Some(mut client) = connect().await else {
        return;
    };
    let value = None::<i32>;
    let rows = client
        .query("SELECT @P1 AS nullable", &[&value])
        .await
        .expect("query typed NULL")
        .into_first_result();
    let row = rows.first().expect("typed NULL row");
    assert_eq!(
        row.columns()
            .first()
            .expect("typed NULL column metadata")
            .column_type(),
        ColumnType::Int4
    );
    assert_eq!(row.get::<Option<i32>, _>("nullable"), Some(None));
}

#[tokio::test]
async fn dynamic_query_builder_queries_in_binding_order() {
    let Some(mut client) = connect().await else {
        return;
    };
    let borrowed = String::from("borrowed");
    let mut query = Query::new("SELECT @P1 AS first_value, @P2 AS second_value");
    query.bind(borrowed.as_str());
    query.bind(String::from("owned"));

    let row = query
        .query(&mut client)
        .await
        .expect("start dynamic query")
        .into_row()
        .await
        .expect("read dynamic query")
        .expect("dynamic query row");
    assert_eq!(row.get::<&str, _>("first_value"), Some("borrowed"));
    assert_eq!(row.get::<&str, _>("second_value"), Some("owned"));
}

#[tokio::test]
async fn dynamic_query_builder_executes_and_is_recreated_for_repeated_use() {
    let Some(mut client) = connect().await else {
        return;
    };
    client
        .simple_query("CREATE TABLE #compat_query_builder (id int NOT NULL)")
        .await
        .expect("create dynamic query table");

    let mut first = Query::new("INSERT INTO #compat_query_builder VALUES (@P1), (@P2)");
    first.bind(1i32);
    first.bind(2i32);
    assert_eq!(
        first
            .execute(&mut client)
            .await
            .expect("execute first dynamic query")
            .total(),
        2
    );

    let mut second = Query::new(String::from(
        "UPDATE #compat_query_builder SET id = id + @P1",
    ));
    second.bind(10i32);
    assert_eq!(
        second
            .execute(&mut client)
            .await
            .expect("execute second dynamic query")
            .total(),
        2
    );
}

#[tokio::test]
async fn execute_result_preserves_order_zero_counts_and_iteration() {
    let Some(mut client) = connect().await else {
        return;
    };
    client
        .simple_query("CREATE TABLE #compat_execute_result (id int NOT NULL)")
        .await
        .expect("create execute-result table");

    let first = client
        .execute(
            "INSERT INTO #compat_execute_result VALUES (1), (2); \
             UPDATE #compat_execute_result SET id = id WHERE id < 0; \
             INSERT INTO #compat_execute_result VALUES (3)",
            &[],
        )
        .await
        .expect("execute first batch");
    assert_eq!(first.rows_affected(), &[2, 0, 1]);
    assert_eq!(first.total(), 3);
    assert_eq!(
        ExecuteResult::into_iter(first).collect::<Vec<_>>(),
        vec![2, 0, 1]
    );

    let second = Query::new(
        "UPDATE #compat_execute_result SET id = id WHERE id < 0; \
         DELETE FROM #compat_execute_result WHERE id = 3",
    )
    .execute(&mut client)
    .await
    .expect("execute second batch");
    assert_eq!(second.rows_affected(), &[0, 1]);
    assert_eq!(second.total(), 1);
    let mut iterated = Vec::new();
    for count in second {
        iterated.push(count);
    }
    assert_eq!(iterated, vec![0, 1]);
}

#[tokio::test]
async fn dynamic_query_builder_preserves_typed_null() {
    let Some(mut client) = connect().await else {
        return;
    };
    let mut query = Query::new("SELECT @P1 AS nullable");
    query.bind(Option::<i32>::None);

    let row = query
        .query(&mut client)
        .await
        .expect("start typed NULL query")
        .into_row()
        .await
        .expect("read typed NULL query")
        .expect("typed NULL row");
    assert_eq!(
        row.columns()
            .first()
            .expect("typed NULL column metadata")
            .column_type(),
        ColumnType::Int4
    );
    assert_eq!(row.get::<Option<i32>, _>("nullable"), Some(None));
}

#[tokio::test]
async fn dynamic_query_builder_propagates_server_parameter_errors() {
    let Some(mut client) = connect().await else {
        return;
    };
    let missing = Query::new("SELECT @P1");
    assert!(missing.query(&mut client).await.err().is_some());

    let mut wrong_type = Query::new("SELECT CAST(@P1 AS int)");
    wrong_type.bind("not an integer");
    let stream = wrong_type
        .query(&mut client)
        .await
        .expect("server emits metadata before evaluating the cast");
    assert!(stream.into_results().await.err().is_some());
}

#[derive(Clone)]
struct NullableBulkRow {
    id: i32,
    value: Option<String>,
}

#[async_trait]
impl BulkLoadRow for NullableBulkRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer
            .write_column_value(*column_index, &ColumnValues::Int(self.id))
            .await?;
        *column_index += 1;
        let value = self.value.as_ref().map_or(ColumnValues::Null, |value| {
            ColumnValues::String(SqlString::from_utf8_string(value.clone()))
        });
        writer.write_column_value(*column_index, &value).await?;
        *column_index += 1;
        Ok(())
    }
}

#[tokio::test]
async fn bulk_nullable_rows_succeed() {
    let Some(mut client) = connect().await else {
        return;
    };
    client
        .simple_query("CREATE TABLE #compat_bulk (id int NOT NULL, value nvarchar(8) NULL)")
        .await
        .expect("create bulk table");
    let result = client
        .bulk_insert("#compat_bulk")
        .send([
            NullableBulkRow {
                id: 1,
                value: Some("one".to_string()),
            },
            NullableBulkRow { id: 2, value: None },
        ])
        .await
        .expect("bulk insert nullable rows");
    assert_eq!(result.rows_affected, 2);

    let rows = client
        .simple_query("SELECT id, value FROM #compat_bulk ORDER BY id")
        .await
        .expect("read bulk rows")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("first bulk row")
            .get::<&str, _>("value"),
        Some("one")
    );
    assert_eq!(
        rows.get(1)
            .expect("second bulk row")
            .get::<Option<&str>, _>("value"),
        Some(None)
    );
}

#[tokio::test]
async fn compat_bulk_tuple_arities_finalize_and_reset() {
    let Some(mut client) = connect().await else {
        return;
    };

    macro_rules! check_arity {
        ($table:literal, $columns:literal, $row:expr) => {{
            client
                .simple_query(concat!("CREATE TABLE ", $table, " (", $columns, ")"))
                .await
                .expect("create tuple-arity table");
            let mut bulk = client
                .bulk_insert($table)
                .await
                .expect("start compatibility bulk");
            bulk.send($row.into_row())
                .await
                .expect("send tuple-arity row");
            assert_eq!(
                bulk.finalize()
                    .await
                    .expect("finalize tuple-arity bulk")
                    .rows_affected(),
                &[1]
            );
        }};
    }

    check_arity!("#compat_bulk_1", "c1 int", 1i32);
    check_arity!("#compat_bulk_2", "c1 int, c2 int", (1i32, 2i32));
    check_arity!(
        "#compat_bulk_3",
        "c1 int, c2 int, c3 int",
        (1i32, 2i32, 3i32)
    );
    check_arity!(
        "#compat_bulk_4",
        "c1 int, c2 int, c3 int, c4 int",
        (1i32, 2i32, 3i32, 4i32)
    );
    check_arity!(
        "#compat_bulk_5",
        "c1 int, c2 int, c3 int, c4 int, c5 int",
        (1i32, 2i32, 3i32, 4i32, 5i32)
    );
    check_arity!(
        "#compat_bulk_6",
        "c1 int, c2 int, c3 int, c4 int, c5 int, c6 int",
        (1i32, 2i32, 3i32, 4i32, 5i32, 6i32)
    );
    check_arity!(
        "#compat_bulk_7",
        "c1 int, c2 int, c3 int, c4 int, c5 int, c6 int, c7 int",
        (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32)
    );
    check_arity!(
        "#compat_bulk_8",
        "c1 int, c2 int, c3 int, c4 int, c5 int, c6 int, c7 int, c8 int",
        (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32)
    );
    check_arity!(
        "#compat_bulk_9",
        "c1 int, c2 int, c3 int, c4 int, c5 int, c6 int, c7 int, c8 int, c9 int",
        (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32, 9i32)
    );
    check_arity!(
        "#compat_bulk_10",
        "c1 int, c2 int, c3 int, c4 int, c5 int, c6 int, c7 int, c8 int, c9 int, c10 int",
        (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32, 9i32, 10i32)
    );
}

#[tokio::test]
async fn compat_bulk_incremental_lifecycle_validates_and_runs_twice() {
    let Some(mut client) = connect().await else {
        return;
    };
    client
        .simple_query(
            "CREATE TABLE #compat_bulk_lifecycle (id int NOT NULL, value nvarchar(8) NULL)",
        )
        .await
        .expect("create lifecycle table");

    let mut first = client
        .bulk_insert("#compat_bulk_lifecycle")
        .await
        .expect("start first compatibility bulk");
    first
        .send((1i32, Some("one")).into_row())
        .await
        .expect("send healthy row");
    let error = first
        .send(2i32.into_row())
        .await
        .expect_err("wrong-width row must fail");
    assert!(matches!(error, Error::BulkInput(_)));
    first
        .send((2i32, Option::<&str>::None).into_row())
        .await
        .expect("width failure must not poison request");
    assert_eq!(
        first
            .finalize()
            .await
            .expect("finalize first compatibility bulk")
            .rows_affected(),
        &[2]
    );

    let mut second = client
        .bulk_insert("#compat_bulk_lifecycle")
        .await
        .expect("start second compatibility bulk");
    second
        .send((3i32, Some("three")).into_row())
        .await
        .expect("send second-operation row");
    assert_eq!(
        second
            .finalize()
            .await
            .expect("finalize second compatibility bulk")
            .rows_affected(),
        &[1]
    );

    let empty = client
        .bulk_insert("#compat_bulk_lifecycle")
        .await
        .expect("start empty compatibility bulk")
        .finalize()
        .await
        .expect("finalize empty compatibility bulk");
    assert_eq!(empty.rows_affected(), &[0]);

    let abandoned = client
        .bulk_insert("#compat_bulk_lifecycle")
        .await
        .expect("start abandoned compatibility bulk");
    drop(abandoned);
    assert_eq!(
        client
            .simple_query("SELECT COUNT(*) AS n FROM #compat_bulk_lifecycle")
            .await
            .expect("client remains usable after unsent request drop")
            .into_first_result()
            .first()
            .and_then(|row| row.get::<i32, _>("n")),
        Some(3)
    );
}

#[tokio::test]
async fn compat_bulk_oversized_value_is_explicit_input_error() {
    let Some(mut client) = connect().await else {
        return;
    };
    client
        .simple_query("CREATE TABLE #compat_bulk_limit (value nvarchar(8) NOT NULL)")
        .await
        .expect("create limited-width table");
    let mut bulk = client
        .bulk_insert("#compat_bulk_limit")
        .await
        .expect("start limited-width bulk");
    bulk.send("too long for nvarchar(8)".into_row())
        .await
        .expect("compatibility adapter retains rows until finalize");
    assert!(matches!(bulk.finalize().await, Err(Error::BulkInput(_))));

    let mut too_wide = client
        .bulk_insert("#compat_bulk_limit")
        .await
        .expect("start wrong-width bulk");
    too_wide
        .send(("one", "two").into_row())
        .await
        .expect("row width is checked against metadata during finalize");
    assert!(matches!(
        too_wide.finalize().await,
        Err(Error::BulkInput(message)) if message.starts_with("Column index ")
    ));

    let mut native = client
        .bulk_insert("#compat_bulk_limit")
        .await
        .expect("start native-only value bulk");
    let mut row = mssql_rs::TokenRow::new();
    row.push(ColumnData::Native(
        mssql_tds::datatypes::sqltypes::SqlType::Variant(Box::new(
            mssql_tds::datatypes::sqltypes::SqlType::Int(Some(1)),
        )),
    ));
    native
        .send(row)
        .await
        .expect("adapter retains native-only row until finalize");
    assert!(matches!(
        native.finalize().await,
        Err(Error::BulkInput(message)) if message.contains("bridge-native parameter")
    ));

    let mut healthy = client
        .bulk_insert("#compat_bulk_limit")
        .await
        .expect("start healthy bulk after completed input error");
    healthy
        .send("fits".into_row())
        .await
        .expect("healthy input stays quiet");
    assert_eq!(
        healthy
            .finalize()
            .await
            .expect("healthy bulk succeeds")
            .rows_affected(),
        &[1]
    );
}

#[test]
fn local_conversion_contract_uses_bridge_value_types() {
    assert!(matches!(
        None::<i32>.to_sql(),
        mssql_tds::datatypes::sqltypes::SqlType::Int(None)
    ));
    assert_eq!(<i32 as FromSql>::from_sql(&ColumnValues::Int(7)), Some(7));
    assert_eq!(
        <&[u8] as FromSql>::from_sql(&ColumnValues::Bytes(vec![1, 2, 3])),
        Some(&[1, 2, 3][..])
    );
}

#[test]
fn compat_conversion_traits_cover_values_nulls_and_errors() {
    macro_rules! check_value {
        ($value:expr, $variant:path, $ty:ty) => {{
            let value: $ty = $value;
            let data = value.into_sql();
            assert!(matches!(data, $variant(Some(_))));
            assert_eq!(
                <$ty>::from_sql_owned(data).expect("decode owned value"),
                Some(value)
            );
        }};
    }

    check_value!(true, ColumnData::Bit, bool);
    check_value!(u8::MAX, ColumnData::U8, u8);
    check_value!(-12, ColumnData::I16, i16);
    check_value!(-34, ColumnData::I32, i32);
    check_value!(-56, ColumnData::I64, i64);
    check_value!(1.25, ColumnData::F32, f32);
    check_value!(2.5, ColumnData::F64, f64);

    let text = String::from("owned");
    assert_eq!(
        String::from_sql_owned(text.clone().into_sql()).expect("decode owned string"),
        Some(text)
    );
    let bytes = vec![1, 2, 3];
    assert_eq!(
        Vec::<u8>::from_sql_owned(bytes.clone().into_sql()).expect("decode owned bytes"),
        Some(bytes)
    );
    let decimal = rust_decimal::Decimal::new(-12345, 2);
    assert_eq!(
        rust_decimal::Decimal::from_sql_owned(decimal.into_sql()).expect("decode decimal"),
        Some(decimal)
    );
    let date = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).expect("valid date");
    assert_eq!(
        chrono::NaiveDate::from_sql_owned(date.into_sql()).expect("decode date"),
        Some(date)
    );

    assert_eq!(
        <i32 as CompatFromSql>::from_sql(&ColumnData::I32(None)).expect("decode NULL"),
        None
    );
    assert!(matches!(
        <i32 as CompatFromSql>::from_sql(&ColumnData::String(Some("wrong".into()))),
        Err(Error::Conversion(_))
    ));
    assert!(matches!(
        String::from_sql_owned(ColumnData::I32(Some(1))),
        Err(Error::Conversion(_))
    ));
}
