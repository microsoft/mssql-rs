# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for DATETIMEOFFSET data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_datetimeoffset.py. Arrow ``timestamp(unit, tz=Some)``
maps to SQL ``DATETIMEOFFSET``.

Note on offsets: Arrow stores timestamp values as a UTC instant plus a single
timezone string (it has no per-value offset). The Rust writer therefore
preserves the exact UTC instant and stores the offset as ``+00:00``. Named,
non-UTC source timezones are normalized to their UTC instant on write — the
displayed offset will be ``+00:00``, not the original zone's offset.
"""
import datetime

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetimeoffset_basic(client_context):
    """Arrow tz-aware timestamp bulkcopy into a DATETIMEOFFSET column (UTC instant)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestDateTimeOffset"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, event_time DATETIMEOFFSET(6))")

    utc = datetime.timezone.utc
    values = [
        datetime.datetime(2024, 1, 15, 9, 30, 45, 123456, tzinfo=utc),
        datetime.datetime(2024, 2, 20, 14, 45, 30, 500000, tzinfo=utc),
        datetime.datetime(2024, 3, 10, 18, 15, 0, 0, tzinfo=utc),
    ]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "event_time": pa.array(values, type=pa.timestamp("us", tz="UTC")),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "event_time")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, event_time FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    # UTC instant preserved; the stored offset is +00:00.
    assert rows[0][1].utcoffset() == datetime.timedelta(0)
    assert rows[0][1].astimezone(utc) == values[0]
    assert rows[2][1].astimezone(utc) == values[2]

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetimeoffset_non_utc_instant_preserved(client_context):
    """A non-UTC source zone keeps its UTC instant; the stored offset is +00:00."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowDToNonUtc"
    cursor.execute(f"CREATE TABLE {table_name} (event_time DATETIMEOFFSET(6) NOT NULL)")

    est = datetime.timezone(datetime.timedelta(hours=-5))
    src_value = datetime.datetime(2024, 2, 20, 14, 45, 30, 500000, tzinfo=est)
    source = pa.table(
        {"event_time": pa.array([src_value], type=pa.timestamp("us", tz="-05:00"))}
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT event_time FROM {table_name}")
    stored = cursor.fetchone()[0]
    # Offset normalized to +00:00, but the absolute instant is unchanged.
    assert stored.utcoffset() == datetime.timedelta(0)
    assert stored.astimezone(datetime.timezone.utc) == src_value.astimezone(
        datetime.timezone.utc
    )

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetimeoffset_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableDToffset"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, event_time DATETIMEOFFSET(6))")

    utc = datetime.timezone.utc
    values = [
        datetime.datetime(2024, 1, 1, 0, 0, 0, tzinfo=utc),
        None,
        datetime.datetime(2024, 3, 3, 3, 3, 3, tzinfo=utc),
    ]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "event_time": pa.array(values, type=pa.timestamp("us", tz="UTC")),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, event_time FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1].astimezone(utc) == values[0]
    assert rows[1][1] is None
    assert rows[2][1].astimezone(utc) == values[2]

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_datetimeoffset_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable DATETIMEOFFSET column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableDToffset"
    cursor.execute(f"CREATE TABLE {table_name} (event_time DATETIMEOFFSET NOT NULL)")

    utc = datetime.timezone.utc
    source = pa.table(
        {
            "event_time": pa.array(
                [datetime.datetime(2024, 1, 1, tzinfo=utc), None],
                type=pa.timestamp("us", tz="UTC"),
            )
        }
    )

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
