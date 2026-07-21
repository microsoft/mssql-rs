# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for INT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_int.py. Arrow ``int32`` maps naturally to SQL ``INT``.
Arrow ``int64`` (the pandas/pyarrow default integer) is range-checked when it
targets ``INT``: in-range values load, out-of-range values raise (A1).
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_int_basic(client_context):
    """Arrow bulkcopy with two int columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableInt"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value INT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "value": pa.array([100, 200, 300], type=pa.int32()),
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
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 1 and rows[0][1] == 100
    assert rows[1][0] == 2 and rows[1][1] == 200

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_int_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableInt"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value INT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, None, 4], type=pa.int32()),
            "value": pa.array([100, None, 300, 400], type=pa.int32()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY COALESCE(id, 999)")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][0] == 1 and rows[0][1] == 100
    assert rows[1][0] == 2 and rows[1][1] is None
    assert rows[2][0] == 4 and rows[2][1] == 400
    assert rows[3][0] is None and rows[3][1] == 300

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_int64_narrows_to_int(client_context):
    """A1: Arrow int64 loads into a SQL INT column when values fit."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowInt64Narrow"
    cursor.execute(f"CREATE TABLE {table_name} (n INT NOT NULL)")

    source = pa.table({"n": pa.array([1, 2, 2_000_000_000], type=pa.int64())})

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT n FROM {table_name} ORDER BY n")
    assert [r[0] for r in cursor.fetchall()] == [1, 2, 2_000_000_000]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_int64_overflow_to_int_raises(client_context):
    """A1: an out-of-range int64 -> INT must raise, not silently truncate."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowInt64Overflow"
    cursor.execute(f"CREATE TABLE {table_name} (n INT NOT NULL)")

    source = pa.table({"n": pa.array([1, 5_000_000_000], type=pa.int64())})

    with pytest.raises(ValueError, match="(?i)out of range"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_int_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable INT column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableInt"
    cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL)")

    source = pa.table({"id": pa.array([1, None, 3], type=pa.int32())})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
