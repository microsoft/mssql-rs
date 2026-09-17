# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for INT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_int.py. Arrow ``int32`` maps naturally to SQL ``INT``.
Arrow ``int64`` (the pandas/pyarrow default integer) is range-checked when it
targets ``INT``: in-range values load, out-of-range values raise (A1).

Basic, auto-mapping, and non-nullable cases are in test_bulkcopy_arrow_integers.py.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


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
