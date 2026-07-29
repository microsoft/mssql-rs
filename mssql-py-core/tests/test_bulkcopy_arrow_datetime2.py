# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for DATETIME2 data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_datetime2.py. Arrow ``timestamp(unit)`` *without* a
timezone maps to SQL ``DATETIME2``. A timezone-aware Arrow timestamp is
rejected for a DATETIME2 destination (use DATETIMEOFFSET instead) so the
timezone is never silently dropped (decision C1).
"""
import datetime

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetime2_basic(client_context):
    """Arrow timestamp (no tz) bulkcopy into DATETIME2 columns."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestDateTime2"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, event_datetime DATETIME2(6))")

    values = [
        datetime.datetime(2024, 1, 15, 9, 30, 15, 123456),
        datetime.datetime(2024, 2, 20, 14, 45, 30, 999999),
        datetime.datetime(2024, 3, 10, 0, 0, 0, 0),
    ]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "event_datetime": pa.array(values, type=pa.timestamp("us")),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "event_datetime")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, event_datetime FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == datetime.datetime(2024, 1, 15, 9, 30, 15, 123456)
    assert rows[1][1] == datetime.datetime(2024, 2, 20, 14, 45, 30, 999999)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetime2_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableDateTime2"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, ts DATETIME2(6))")

    values = [datetime.datetime(2024, 1, 1, 12, 0, 0), None, datetime.datetime(2024, 3, 3, 3, 3, 3)]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "ts": pa.array(values, type=pa.timestamp("us")),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, ts FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == datetime.datetime(2024, 1, 1, 12, 0, 0)
    assert rows[1][1] is None
    assert rows[2][1] == datetime.datetime(2024, 3, 3, 3, 3, 3)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_tz_aware_timestamp_to_datetime2_raises(client_context):
    """C1: a tz-aware Arrow timestamp -> DATETIME2 must raise, pointing at datetimeoffset."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowTzToDateTime2"
    cursor.execute(f"CREATE TABLE {table_name} (ts DATETIME2)")

    ts = pa.array(
        [datetime.datetime(2020, 1, 1, 12, tzinfo=datetime.timezone.utc)],
        type=pa.timestamp("us", tz="UTC"),
    )
    source = pa.table({"ts": ts})

    with pytest.raises(ValueError, match="(?i)datetimeoffset"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetime2_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable DATETIME2 column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableDateTime2"
    cursor.execute(f"CREATE TABLE {table_name} (ts DATETIME2 NOT NULL)")

    source = pa.table(
        {"ts": pa.array([datetime.datetime(2020, 1, 1), None], type=pa.timestamp("us"))}
    )

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetime2_scale_zero(client_context):
    """DATETIME2(0) preserves the explicit scale 0 (sub-second precision dropped).

    Regression test: the destination scale (0) must be honored, not overridden
    by the Arrow microsecond unit.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowDateTime2ScaleZero"
    cursor.execute(f"CREATE TABLE {table_name} (ts DATETIME2(0) NOT NULL)")

    source = pa.table(
        {
            "ts": pa.array(
                [datetime.datetime(2024, 1, 15, 9, 30, 15)], type=pa.timestamp("us")
            )
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT ts FROM {table_name}")
    assert cursor.fetchone()[0] == datetime.datetime(2024, 1, 15, 9, 30, 15)

    conn.close()
