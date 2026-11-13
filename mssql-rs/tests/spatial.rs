// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Spatial regressions for issue #69. Uses the shared live-test environment.

pub mod common;

use futures_util::TryStreamExt;
use mssql_rs::{Client, ColumnType, ColumnValues, Row};

async fn connect() -> Option<Client> {
    let config = common::live_config()?;
    Some(
        Client::connect(&config)
            .await
            .expect("connect to SQL Server"),
    )
}

fn assert_spatial_bytes(row: &Row, name: &str, column_type: ColumnType, srid: u32) {
    let index = row.column_index(name).expect("expected named column");
    let column = row
        .columns()
        .get(index)
        .expect("expected spatial column metadata");
    assert_eq!(column.column_type(), column_type);
    assert!(column.is_plp());
    assert_eq!(column.char_length(), None);
    let expected_name = format!("{name}_bytes");
    let expected = row.get::<&[u8], _>(expected_name.as_str());
    assert_eq!(
        row.try_get::<&[u8], _>(index)
            .expect("expected non-NULL column index"),
        expected
    );
    assert_eq!(row.get::<Vec<u8>, _>(name), expected.map(<[u8]>::to_vec));
    assert_eq!(row.get::<Option<&[u8]>, _>(name), Some(expected));
    let value = (row.raw_value(index), expected);
    assert!(
        matches!(
            value,
            (Some(ColumnValues::Bytes(_)), Some(_)) | (Some(ColumnValues::Null), None)
        ),
        "unexpected spatial value: {value:?}"
    );
    if let (Some(ColumnValues::Bytes(bytes)), Some(expected)) = value {
        assert_eq!(bytes, expected);
        assert_eq!(
            bytes.get(..4).expect("spatial payload contains SRID"),
            &srid.to_le_bytes()
        );
        assert_eq!(
            row.get::<&[u8], _>(name)
                .expect("expected non-NULL column name")
                .as_ptr(),
            bytes.as_ptr()
        );
    }
    assert_eq!(row.get::<&str, _>(name), None);
}

const SHAPES: &str = "
    DECLARE @shapes TABLE (id int, g geography, m geometry);
    INSERT INTO @shapes VALUES
        (1, geography::Point(1, 2, 4326), geometry::Point(2, 1, 0)),
        (2, geography::STGeomFromText('LINESTRING(0 0, 1 1)', 4326),
            geometry::STGeomFromText('LINESTRING(0 0, 1 1)', 0)),
        (3, geography::STGeomFromText('POLYGON((0 0, 1 0, 1 1, 0 0))', 4326),
            geometry::STGeomFromText('POLYGON((0 0, 1 0, 1 1, 0 0))', 0)),
        (4, geography::STGeomFromText('GEOMETRYCOLLECTION EMPTY', 4326),
            geometry::STGeomFromText('GEOMETRYCOLLECTION EMPTY', 0)),
        (5, NULL, NULL),
        (6, geography::STGeomFromText('POINT(2 1 3 4)', 4326),
            geometry::STGeomFromText('CIRCULARSTRING(0 0, 1 1, 2 0)', 0));
    SELECT id, g, m, g.Serialize() AS g_bytes, m.Serialize() AS m_bytes,
           CAST(0x010203 AS varbinary(3)) AS ordinary_bytes, N'after' AS tail
    FROM @shapes ORDER BY id;
";

#[tokio::test]
async fn spatial_buffered_and_streamed_reads_preserve_native_serialization() {
    let Some(mut client) = connect().await else {
        return;
    };
    let buffered = client
        .simple_query(SHAPES)
        .await
        .expect("simple query succeeds for spatial buffered and streamed reads preserve native serialization")
        .into_first_result();
    let streamed: Vec<Row> = client
        .simple_query_streamed(SHAPES)
        .try_collect()
        .await
        .expect("collect query rows");
    assert_eq!(buffered, streamed);
    for rows in [&buffered, &streamed] {
        assert_eq!(rows.len(), 6);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(
                row.get::<i32, _>("id"),
                Some(i32::try_from(index).expect("row index fits i32") + 1)
            );
            assert_spatial_bytes(row, "g", ColumnType::Geography, 4326);
            assert_spatial_bytes(row, "m", ColumnType::Geometry, 0);
            assert_eq!(
                row.columns()
                    .get(5)
                    .expect("expected ordinary binary column metadata")
                    .column_type(),
                ColumnType::BigVarBin
            );
            assert_eq!(row.get::<&[u8], _>("ordinary_bytes"), Some(&[1, 2, 3][..]));
            assert_eq!(row.get::<&str, _>("tail"), Some("after"));
        }
        assert!(
            rows.get(3)
                .expect("expected row at index 3")
                .get::<Vec<u8>, _>("g")
                .is_some(),
            "EMPTY is not NULL"
        );
        assert!(
            rows.get(3)
                .expect("expected row at index 3")
                .get::<Vec<u8>, _>("m")
                .is_some(),
            "EMPTY is not NULL"
        );
        assert_eq!(
            rows.get(4)
                .expect("expected row at index 4")
                .get::<Vec<u8>, _>("g"),
            None
        );
        assert_eq!(
            rows.get(4)
                .expect("expected row at index 4")
                .get::<Vec<u8>, _>("m"),
            None
        );
        // A single point: native SRID + version/flags + two coordinates, not WKB.
        assert_eq!(
            rows.first()
                .expect("expected row at index 0")
                .get::<&[u8], _>("g")
                .expect("expected non-NULL column g")
                .len(),
            22
        );
        assert_eq!(
            rows.first()
                .expect("expected row at index 0")
                .get::<&[u8], _>("m")
                .expect("expected non-NULL column m")
                .len(),
            22
        );
    }
    let next = client
        .simple_query("SELECT 42 AS n")
        .await
        .expect("query succeeds: SELECT 42 AS n")
        .into_first_result();
    assert_eq!(
        next.first()
            .expect("expected row at index 0")
            .get::<i32, _>("n"),
        Some(42)
    );
}

#[tokio::test]
async fn spatial_parameterized_reads_span_multiple_packets() {
    let Some(mut client) = connect().await else {
        return;
    };
    let sql = "
        DECLARE @wkt nvarchar(max) = N'LINESTRING(0 1', @i int = 1;
        WHILE @i < @P1
        BEGIN
            SET @wkt += N', ' + CONVERT(nvarchar(30), CAST(@i AS decimal(10,2)) / 100) + N' 1';
            SET @i += 1;
        END;
        SET @wkt += N')';
        DECLARE @g geography = geography::STGeomFromText(@wkt, 4326);
        DECLARE @m geometry = geometry::STGeomFromText(@wkt, 1234);
        SELECT @g AS g, @m AS m, @g.Serialize() AS g_bytes,
               @m.Serialize() AS m_bytes, 42 AS tail;
    ";
    let buffered = client
        .query(sql, &[&2000i32])
        .await
        .expect("query succeeds for spatial parameterized reads span multiple packets")
        .into_first_result();
    let streamed: Vec<Row> = client
        .query_streamed(sql, &[&2000i32])
        .try_collect()
        .await
        .expect("collect query rows");
    assert_eq!(buffered, streamed);
    for rows in [&buffered, &streamed] {
        assert_eq!(rows.len(), 1);
        let row = rows.first().expect("expected row at index 0");
        assert_spatial_bytes(row, "g", ColumnType::Geography, 4326);
        assert_spatial_bytes(row, "m", ColumnType::Geometry, 1234);
        for name in ["g", "m"] {
            assert!(
                row.get::<&[u8], _>(name)
                    .expect("expected non-NULL column name")
                    .len()
                    > 32_000
            );
        }
        assert_eq!(row.get::<i32, _>("tail"), Some(42));
    }
}

#[tokio::test]
async fn non_spatial_udt_remains_opaque_bytes() {
    let Some(mut client) = connect().await else {
        return;
    };
    let rows = client
        .simple_query(
            "DECLARE @h hierarchyid = hierarchyid::Parse('/1/2/');
             SELECT @h AS h, CAST(@h AS varbinary(max)) AS h_bytes,
                    CAST(NULL AS hierarchyid) AS missing;",
        )
        .await
        .expect("query succeeds: DECLARE @h hierarchyid = hierarchyid::Parse('/1/2/'); SELECT @h AS h, CAST(@h AS varbinary(max)) ...")
        .into_first_result();
    assert_eq!(rows.len(), 1);
    let row = rows.first().expect("expected row at index 0");
    assert_eq!(
        row.columns()
            .first()
            .expect("expected hierarchyid column metadata")
            .column_type(),
        ColumnType::Udt
    );
    assert_eq!(
        row.columns()
            .get(2)
            .expect("expected nullable hierarchyid column metadata")
            .column_type(),
        ColumnType::Udt
    );
    assert!(matches!(row.raw_value(0), Some(ColumnValues::Bytes(_))));
    assert_eq!(row.get::<&[u8], _>("h"), row.get::<&[u8], _>("h_bytes"));
    assert_eq!(row.get::<Vec<u8>, _>("missing"), None);
}
