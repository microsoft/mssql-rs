# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for VARCHAR data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_varchar_collations.py. Arrow ``utf8`` maps to SQL
``VARCHAR`` (single-byte / collation-encoded character data).
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_varchar_basic(client_context):
    """Arrow bulkcopy with varchar columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableVarchar"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, name VARCHAR(50))")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "name": pa.array(["alpha", "beta", "gamma"]),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "name")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, name FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == "alpha"
    assert rows[2][1] == "gamma"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_varchar_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableVarchar"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, name VARCHAR(100))")

    source = pa.table(
        {
            "id": pa.array([1, 2, 4], type=pa.int32()),
            "name": pa.array(["Alice", None, "Charlie"]),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, name FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == "Alice"
    assert rows[1][1] is None
    assert rows[2][1] == "Charlie"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_varchar_max_large(client_context):
    """Arrow utf8 with a large string round-trips through VARCHAR(MAX)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowVarcharMax"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, content VARCHAR(MAX))")

    large_text = "x" * 20000
    source = pa.table(
        {
            "id": pa.array([1, 2], type=pa.int32()),
            "content": pa.array(["small", large_text]),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT id, content FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[1][1] == large_text

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_varchar_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable VARCHAR column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableVarchar"
    cursor.execute(f"CREATE TABLE {table_name} (name VARCHAR(50) NOT NULL)")

    source = pa.table({"name": pa.array(["a", None, "c"])})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
