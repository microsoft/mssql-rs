# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for REAL / FLOAT data types (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_float.py. Arrow ``float32`` maps to SQL ``REAL`` (or
``FLOAT``) and Arrow ``float64`` maps to SQL ``FLOAT``.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_float64_basic(client_context):
    """Arrow float64 bulkcopy into a FLOAT column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowFloat64Basic"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value_col FLOAT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3, 4, 5], type=pa.int32()),
            "value_col": pa.array(
                [3.14159, 2.71828, 1.41421, -9.81, 0.0], type=pa.float64()
            ),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 5

    cursor.execute(f"SELECT id, value_col FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 5
    assert abs(rows[0][1] - 3.14159) < 1e-9
    assert rows[4][1] == 0.0

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_float32_to_real(client_context):
    """Arrow float32 bulkcopy into a REAL column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowFloat32Real"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value_col REAL)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "value_col": pa.array([1.5, 2.5, -3.25], type=pa.float32()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value_col FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    # REAL is exact for these half/quarter values.
    assert rows[0][1] == 1.5
    assert rows[2][1] == -3.25

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_float_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowFloatAutoMap"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, measurement FLOAT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "measurement": pa.array([98.6, None, 37.0], type=pa.float64()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, measurement FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert abs(rows[0][1] - 98.6) < 1e-9
    assert rows[1][1] is None
    assert rows[2][1] == 37.0

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_float_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable FLOAT column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableFloat"
    cursor.execute(f"CREATE TABLE {table_name} (value_col FLOAT NOT NULL)")

    source = pa.table({"value_col": pa.array([1.5, None, 3.5], type=pa.float64())})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
