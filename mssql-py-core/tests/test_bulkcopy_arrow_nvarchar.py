# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for NVARCHAR data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_nvarchar.py. Arrow ``utf8`` maps to SQL ``NVARCHAR``
(SQL Server stores it as UTF-16LE internally).
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_nvarchar_basic(client_context):
    """Arrow bulkcopy with nvarchar columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableNVarchar"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, name NVARCHAR(50), description NVARCHAR(200))"
    )

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "name": pa.array(["John Doe", "Jane Smith", "Bob Johnson"]),
            "description": pa.array(
                ["First employee", "Second employee", "Third employee"]
            ),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "name"), (2, "description")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    cursor.execute(f"SELECT id, name, description FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == "John Doe" and rows[0][2] == "First employee"
    assert rows[1][1] == "Jane Smith"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_nvarchar_unicode(client_context):
    """Arrow utf8 with non-ASCII characters round-trips through NVARCHAR."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNVarcharUnicode"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, txt NVARCHAR(100))")

    values = ["wörld", "こんにちは", "Ω≈ç√", "emoji 🚀"]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3, 4], type=pa.int32()),
            "txt": pa.array(values),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 4

    cursor.execute(f"SELECT id, txt FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert [r[1] for r in rows] == values

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_nvarchar_max_large(client_context):
    """Arrow utf8 with a large string round-trips through NVARCHAR(MAX)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNVarcharMax"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, content NVARCHAR(MAX))")

    large_text = "A" * 10000
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "content": pa.array(["Short text", large_text, "Another short text"]),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, content FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[1][1] == large_text

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_nvarchar_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableNVarchar"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, name NVARCHAR(100))")

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
def test_cursor_bulkcopy_arrow_nvarchar_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable NVARCHAR column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableNVarchar"
    cursor.execute(f"CREATE TABLE {table_name} (name NVARCHAR(50) NOT NULL)")

    source = pa.table({"name": pa.array(["a", None, "c"])})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
