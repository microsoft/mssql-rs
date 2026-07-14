# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for TINYINT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_tinyint.py. Arrow ``uint8`` maps naturally to SQL
``TINYINT`` (0-255); wider Arrow integer types are range-checked per cell when
they target ``TINYINT``.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_tinyint_basic(client_context):
    """Arrow bulkcopy with two tinyint columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableTinyInt"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id TINYINT, value TINYINT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.uint8()),
            "value": pa.array([0, 128, 255], type=pa.uint8()),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "value")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 1 and rows[0][1] == 0
    assert rows[2][0] == 3 and rows[2][1] == 255

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_tinyint_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableTinyInt"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id TINYINT, value TINYINT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 4], type=pa.uint8()),
            "value": pa.array([100, None, 200], type=pa.uint8()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][0] == 1 and rows[0][1] == 100
    assert rows[1][0] == 2 and rows[1][1] is None
    assert rows[2][0] == 4 and rows[2][1] == 200

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_tinyint_out_of_range_raises(client_context):
    """A wider Arrow int whose value exceeds TINYINT range must raise, not wrap."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowTinyIntRange"
    cursor.execute(f"CREATE TABLE {table_name} (value TINYINT NOT NULL)")

    # 256 exceeds TINYINT (0-255); int16 is a valid source that targets TINYINT.
    source = pa.table({"value": pa.array([1, 256, 3], type=pa.int16())})

    with pytest.raises(ValueError, match="(?i)out of range"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_tinyint_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable TINYINT column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableTinyInt"
    cursor.execute(f"CREATE TABLE {table_name} (id TINYINT NOT NULL)")

    source = pa.table({"id": pa.array([1, None, 3], type=pa.uint8())})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
