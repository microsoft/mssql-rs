# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for BIT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_bit.py, but the source is a pyarrow.Table of typed
columns instead of an iterable of Python tuples. Arrow ``bool`` maps to SQL
``BIT``.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_bit_basic(client_context):
    """Arrow bulkcopy with two bit columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableBit"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (flag1 BIT, flag2 BIT)")

    source = pa.table(
        {
            "flag1": pa.array([True, False, True], type=pa.bool_()),
            "flag2": pa.array([False, True, False], type=pa.bool_()),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "flag1"), (1, "flag2")],
    )

    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    cursor.execute(f"SELECT flag1, flag2 FROM {table_name}")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] is True and rows[0][1] is False
    assert rows[1][0] is False and rows[1][1] is True
    assert rows[2][0] is True and rows[2][1] is False

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_bit_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableBit"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, flag BIT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3, 4], type=pa.int32()),
            "flag": pa.array([True, None, False, None], type=pa.bool_()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, flag FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][1] is True
    assert rows[1][1] is None
    assert rows[2][1] is False
    assert rows[3][1] is None

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_bit_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable BIT column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableBit"
    cursor.execute(f"CREATE TABLE {table_name} (flag BIT NOT NULL)")

    source = pa.table({"flag": pa.array([True, None, False], type=pa.bool_())})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
