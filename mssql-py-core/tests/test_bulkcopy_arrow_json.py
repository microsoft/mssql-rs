# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for the native JSON data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_json.py. Arrow ``utf8``/``large_utf8`` maps to the native
SQL ``JSON`` type (SQL Server 2025+). Tests skip automatically if the server
does not support the native JSON type.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


def _create_json_table(cursor, table_name, columns):
    """Create a table with a native JSON column, skipping if unsupported."""
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    try:
        cursor.execute(f"CREATE TABLE {table_name} ({columns})")
    except Exception as exc:  # pragma: no cover - server-version dependent
        pytest.skip(f"native JSON type not supported by this server: {exc}")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_json_basic(client_context):
    """Arrow utf8 bulkcopy into a native JSON column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableJson"
    _create_json_table(cursor, table_name, "id INT, doc JSON")

    docs = [
        '{"id": 1, "name": "Alice"}',
        '{"id": 2, "values": [1, 2, 3]}',
        '{"nested": {"a": true, "b": null}}',
    ]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "doc": pa.array(docs),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "doc")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(
        f"SELECT id, JSON_VALUE(doc, '$.id') FROM {table_name} WHERE id = 1"
    )
    row = cursor.fetchone()
    assert row[0] == 1 and str(row[1]) == "1"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_json_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableJson"
    _create_json_table(cursor, table_name, "id INT, doc JSON")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "doc": pa.array(['{"a": 1}', None, '{"c": 3}']),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, JSON_VALUE(doc, '$.a') FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert str(rows[0][1]) == "1"
    assert rows[1][1] is None

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_json_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable JSON column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowNonNullableJson"
    _create_json_table(cursor, table_name, "doc JSON NOT NULL")

    source = pa.table({"doc": pa.array(['{"a": 1}', None])})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
    conn.close()
