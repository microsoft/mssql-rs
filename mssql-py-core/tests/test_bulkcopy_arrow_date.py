# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for DATE data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_date.py. Arrow ``date32`` (and ``date64``) map to SQL
``DATE``.
"""
import datetime

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_date_basic(client_context):
    """Arrow date32 bulkcopy with two date columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableDate"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (event_date DATE, birth_date DATE)")

    source = pa.table(
        {
            "event_date": pa.array(
                [datetime.date(2020, 1, 15), datetime.date(2021, 6, 10), datetime.date(2022, 12, 25)],
                type=pa.date32(),
            ),
            "birth_date": pa.array(
                [datetime.date(1990, 5, 20), datetime.date(1985, 3, 25), datetime.date(2000, 7, 4)],
                type=pa.date32(),
            ),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "event_date"), (1, "birth_date")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT event_date, birth_date FROM {table_name} ORDER BY event_date")
    rows = cursor.fetchall()
    assert rows[0][0] == datetime.date(2020, 1, 15)
    assert rows[0][1] == datetime.date(1990, 5, 20)
    assert rows[2][0] == datetime.date(2022, 12, 25)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_date_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableDate"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, event_date DATE)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "event_date": pa.array(
                [datetime.date(2020, 1, 15), None, datetime.date(2022, 12, 25)],
                type=pa.date32(),
            ),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, event_date FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == datetime.date(2020, 1, 15)
    assert rows[1][1] is None
    assert rows[2][1] == datetime.date(2022, 12, 25)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_date_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable DATE column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableDate"
    cursor.execute(f"CREATE TABLE {table_name} (event_date DATE NOT NULL)")

    source = pa.table(
        {"event_date": pa.array([datetime.date(2020, 1, 1), None], type=pa.date32())}
    )

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
