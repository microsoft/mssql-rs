# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Shared Arrow integer bulk copy cases; type-specific edge cases live alongside."""

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
@pytest.mark.parametrize(
    "sql_type,arrow_type,table_suffix,expected_rows",
    [
        pytest.param(
            "TINYINT", pa.uint8(), "TinyInt",
            [(1, 0), (2, 128), (3, 255)], id="tinyint",
        ),
        pytest.param(
            "SMALLINT", pa.int16(), "SmallInt",
            [(1, -32768), (2, 0), (3, 32767)], id="smallint",
        ),
        pytest.param(
            "INT", pa.int32(), "Int",
            [(1, 100), (2, 200), (3, 300)], id="int",
        ),
        pytest.param(
            "BIGINT", pa.int64(), "BigInt",
            [(1, -9_000_000_000_000_000_000), (2, 0), (3, 9_000_000_000_000_000_000)],
            id="bigint",
        ),
    ],
)
def test_cursor_bulkcopy_arrow_integer_basic(
    client_context, sql_type, arrow_type, table_suffix, expected_rows
):
    """Arrow bulkcopy with explicit mappings for each SQL integer type."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = f"BulkCopyArrowTestTable{table_suffix}"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id {sql_type}, value {sql_type})")

    source = pa.table(
        {
            "id": pa.array([row[0] for row in expected_rows], type=arrow_type),
            "value": pa.array([row[1] for row in expected_rows], type=arrow_type),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "value")],
    )

    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    assert cursor.fetchall() == expected_rows

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
@pytest.mark.parametrize(
    "sql_type,arrow_type,table_suffix,source_rows,order_by,expected_rows",
    [
        pytest.param(
            "TINYINT", pa.uint8(), "TinyInt",
            [(1, 100), (2, None), (4, 200)], "id",
            [(1, 100), (2, None), (4, 200)], id="tinyint",
        ),
        pytest.param(
            "SMALLINT", pa.int16(), "SmallInt",
            [(1, 100), (2, None), (4, 300)], "id",
            [(1, 100), (2, None), (4, 300)], id="smallint",
        ),
        pytest.param(
            "INT", pa.int32(), "Int",
            [(1, 100), (2, None), (None, 300), (4, 400)], "COALESCE(id, 999)",
            [(1, 100), (2, None), (4, 400), (None, 300)], id="int",
        ),
        pytest.param(
            "BIGINT", pa.int64(), "BigInt",
            [(1, 100), (2, None), (4, 400)], "id",
            [(1, 100), (2, None), (4, 400)], id="bigint",
        ),
    ],
)
def test_cursor_bulkcopy_arrow_integer_auto_mapping(
    client_context, sql_type, arrow_type, table_suffix, source_rows, order_by,
    expected_rows,
):
    """Arrow bulkcopy with automatic mapping, including nullable INT IDs."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = f"BulkCopyArrowAutoMapTable{table_suffix}"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id {sql_type}, value {sql_type})")

    source = pa.table(
        {
            "id": pa.array([row[0] for row in source_rows], type=arrow_type),
            "value": pa.array([row[1] for row in source_rows], type=arrow_type),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    assert result["rows_copied"] == len(source_rows)
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY {order_by}")
    assert cursor.fetchall() == expected_rows

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
@pytest.mark.parametrize(
    "sql_type,arrow_type,table_suffix",
    [
        pytest.param("TINYINT", pa.uint8(), "TinyInt", id="tinyint"),
        pytest.param("SMALLINT", pa.int16(), "SmallInt", id="smallint"),
        pytest.param("INT", pa.int32(), "Int", id="int"),
        pytest.param("BIGINT", pa.int64(), "BigInt", id="bigint"),
    ],
)
def test_cursor_bulkcopy_arrow_integer_null_to_non_nullable_column(
    client_context, sql_type, arrow_type, table_suffix
):
    """A NULL value into a non-nullable integer column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = f"#BulkCopyArrowNonNullable{table_suffix}"
    cursor.execute(f"CREATE TABLE {table_name} (id {sql_type} NOT NULL)")

    source = pa.table({"id": pa.array([1, None, 3], type=arrow_type)})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
