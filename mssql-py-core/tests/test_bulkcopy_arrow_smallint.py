# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for SMALLINT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_smallint.py. Arrow ``int16`` maps naturally to SQL
``SMALLINT`` (-32768..32767); wider Arrow integer types are range-checked.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smallint_basic(client_context):
    """Arrow bulkcopy with two smallint columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableSmallInt"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id SMALLINT, value SMALLINT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int16()),
            "value": pa.array([-32768, 0, 32767], type=pa.int16()),
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
    assert rows[0][1] == -32768
    assert rows[2][1] == 32767

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smallint_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableSmallInt"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id SMALLINT, value SMALLINT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 4], type=pa.int16()),
            "value": pa.array([100, None, 300], type=pa.int16()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == 100
    assert rows[1][1] is None
    assert rows[2][1] == 300

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smallint_out_of_range_raises(client_context):
    """A wider Arrow int whose value exceeds SMALLINT range must raise."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowSmallIntRange"
    cursor.execute(f"CREATE TABLE {table_name} (value SMALLINT NOT NULL)")

    source = pa.table({"value": pa.array([1, 40000, 3], type=pa.int32())})

    with pytest.raises(ValueError, match="(?i)out of range"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smallint_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable SMALLINT column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableSmallInt"
    cursor.execute(f"CREATE TABLE {table_name} (id SMALLINT NOT NULL)")

    source = pa.table({"id": pa.array([1, None, 3], type=pa.int16())})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
