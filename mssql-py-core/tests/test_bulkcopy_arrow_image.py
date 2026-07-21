# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for IMAGE data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_image.py. Arrow ``binary`` maps to SQL ``IMAGE`` (a
deprecated large-object binary type).
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_image_basic(client_context):
    """Arrow binary bulkcopy into an IMAGE column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableImage"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, blob IMAGE)")

    small = bytes([0xDE, 0xAD, 0xBE, 0xEF])
    large = bytes([0x5A]) * 5000
    source = pa.table(
        {
            "id": pa.array([1, 2], type=pa.int32()),
            "blob": pa.array([small, large], type=pa.binary()),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "blob")],
    )

    assert result["rows_copied"] == 2
    assert result["batch_count"] == 1

    cursor.execute(
        f"SELECT id, CAST(blob AS VARBINARY(MAX)), DATALENGTH(blob) FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()
    assert rows[0][1] == small and rows[0][2] == 4
    assert rows[1][1] == large and rows[1][2] == 5000

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_image_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableImage"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, blob IMAGE)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "blob": pa.array([b"\x01\x02", None, b"\x0a\x0b\x0c"], type=pa.binary()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(
        f"SELECT id, CAST(blob AS VARBINARY(MAX)) FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()
    assert rows[0][1] == b"\x01\x02"
    assert rows[1][1] is None
    assert rows[2][1] == b"\x0a\x0b\x0c"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_image_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable IMAGE column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableImage"
    cursor.execute(f"CREATE TABLE {table_name} (blob IMAGE NOT NULL)")

    source = pa.table({"blob": pa.array([b"\x01", None, b"\x03"], type=pa.binary())})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
