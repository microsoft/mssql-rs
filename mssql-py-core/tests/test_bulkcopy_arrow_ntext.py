# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for NTEXT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_ntext.py. Arrow ``utf8`` maps to SQL ``NTEXT`` (a
deprecated large-object Unicode type).
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_ntext_basic(client_context):
    """Arrow bulkcopy with an ntext column and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableNText"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, body NTEXT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "body": pa.array(["first note", "second wörld", "third 🚀"]),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "body")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, CAST(body AS NVARCHAR(MAX)) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == "first note"
    assert rows[1][1] == "second wörld"
    assert rows[2][1] == "third 🚀"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_ntext_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableNText"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, body NTEXT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "body": pa.array(["alpha", None, "gamma"]),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, CAST(body AS NVARCHAR(MAX)) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == "alpha"
    assert rows[1][1] is None
    assert rows[2][1] == "gamma"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_ntext_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable NTEXT column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableNText"
    cursor.execute(f"CREATE TABLE {table_name} (body NTEXT NOT NULL)")

    source = pa.table({"body": pa.array(["a", None, "c"])})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
