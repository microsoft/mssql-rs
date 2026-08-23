# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for DATETIME / SMALLDATETIME data types.

Mirrors test_bulkcopy_datetime.py / test_bulkcopy_smalldatetime.py. Arrow
tz-naive ``timestamp`` maps to the legacy SQL ``DATETIME`` (days since 1900 +
1/300s ticks) and ``SMALLDATETIME`` (minute precision).
"""
import datetime

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetime_basic(client_context):
    """Arrow tz-naive timestamp bulkcopy into a DATETIME column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableDateTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, dt DATETIME)")

    # Whole-second values round-trip exactly through DATETIME (1/300s precision).
    values = [
        datetime.datetime(2024, 1, 15, 9, 30, 15),
        datetime.datetime(1999, 12, 31, 23, 59, 59),
        datetime.datetime(2050, 6, 1, 0, 0, 0),
    ]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "dt": pa.array(values, type=pa.timestamp("us")),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "dt")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, dt FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == values[0]
    assert rows[1][1] == values[1]
    assert rows[2][1] == values[2]

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smalldatetime_basic(client_context):
    """Arrow tz-naive timestamp bulkcopy into a SMALLDATETIME column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableSmallDateTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, dt SMALLDATETIME)")

    # Zero-second values avoid the minute rounding, so they round-trip exactly.
    values = [
        datetime.datetime(2024, 1, 15, 9, 30, 0),
        datetime.datetime(2000, 1, 1, 0, 0, 0),
    ]
    source = pa.table(
        {
            "id": pa.array([1, 2], type=pa.int32()),
            "dt": pa.array(values, type=pa.timestamp("us")),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT id, dt FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == datetime.datetime(2024, 1, 15, 9, 30, 0)
    assert rows[1][1] == datetime.datetime(2000, 1, 1, 0, 0, 0)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smalldatetime_rounds_seconds(client_context):
    """SMALLDATETIME rounds seconds >= 30 up to the next minute."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSmallDateTimeRound"
    cursor.execute(f"CREATE TABLE {table_name} (dt SMALLDATETIME NOT NULL)")

    # 09:30:45 -> rounds up to 09:31:00.
    source = pa.table(
        {
            "dt": pa.array(
                [datetime.datetime(2024, 1, 15, 9, 30, 45)], type=pa.timestamp("us")
            )
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT dt FROM {table_name}")
    assert cursor.fetchone()[0] == datetime.datetime(2024, 1, 15, 9, 31, 0)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetime_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableDateTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, dt DATETIME)")

    values = [datetime.datetime(2024, 1, 1, 1, 2, 3), None, datetime.datetime(2024, 3, 3, 3, 3, 3)]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "dt": pa.array(values, type=pa.timestamp("us")),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, dt FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == values[0]
    assert rows[1][1] is None
    assert rows[2][1] == values[2]

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetime_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable DATETIME column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowNonNullableDateTime"
    cursor.execute(f"CREATE TABLE {table_name} (dt DATETIME NOT NULL)")

    source = pa.table(
        {"dt": pa.array([datetime.datetime(2024, 1, 1), None], type=pa.timestamp("us"))}
    )

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
