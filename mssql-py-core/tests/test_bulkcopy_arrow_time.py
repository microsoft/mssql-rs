# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for TIME data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_time.py. Arrow ``time64(us|ns)`` and ``time32(s|ms)``
map to SQL ``TIME``.
"""
import datetime

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_time_basic(client_context):
    """Arrow time64 bulkcopy with two time columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (start_time TIME(6), end_time TIME(6))")

    source = pa.table(
        {
            "start_time": pa.array(
                [datetime.time(9, 30, 0), datetime.time(8, 15, 45), datetime.time(10, 0, 0)],
                type=pa.time64("us"),
            ),
            "end_time": pa.array(
                [datetime.time(17, 45, 30), datetime.time(16, 30, 15), datetime.time(18, 0, 0)],
                type=pa.time64("us"),
            ),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "start_time"), (1, "end_time")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT start_time, end_time FROM {table_name} ORDER BY start_time")
    rows = cursor.fetchall()
    assert rows[0][0] == datetime.time(8, 15, 45)
    assert rows[2][0] == datetime.time(10, 0, 0)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_time_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, t TIME(6))")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "t": pa.array(
                [datetime.time(1, 2, 3), None, datetime.time(23, 59, 59)],
                type=pa.time64("us"),
            ),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, t FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == datetime.time(1, 2, 3)
    assert rows[1][1] is None
    assert rows[2][1] == datetime.time(23, 59, 59)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_time_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable TIME column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableTime"
    cursor.execute(f"CREATE TABLE {table_name} (t TIME NOT NULL)")

    source = pa.table(
        {"t": pa.array([datetime.time(1, 2, 3), None], type=pa.time64("us"))}
    )

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
