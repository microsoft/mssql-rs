# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for BIGINT data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_bigint.py. Arrow ``int64`` maps directly to SQL
``BIGINT``. Arrow ``uint64`` also targets ``BIGINT`` but is overflow-checked:
values greater than ``i64::MAX`` are rejected rather than wrapped negative (A2).
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_bigint_basic(client_context):
    """Arrow bulkcopy with two bigint columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableBigInt"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id BIGINT, value BIGINT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int64()),
            "value": pa.array(
                [-9_000_000_000_000_000_000, 0, 9_000_000_000_000_000_000],
                type=pa.int64(),
            ),
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
    assert rows[0][1] == -9_000_000_000_000_000_000
    assert rows[2][1] == 9_000_000_000_000_000_000

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_bigint_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableBigInt"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id BIGINT, value BIGINT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 4], type=pa.int64()),
            "value": pa.array([100, None, 400], type=pa.int64()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == 100
    assert rows[1][1] is None
    assert rows[2][1] == 400

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_uint64_to_bigint(client_context):
    """A2: Arrow uint64 loads into BIGINT for values within i64 range."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowUInt64BigInt"
    cursor.execute(f"CREATE TABLE {table_name} (n BIGINT NOT NULL)")

    source = pa.table(
        {"n": pa.array([1, 42, 9_000_000_000_000_000_000], type=pa.uint64())}
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT n FROM {table_name} ORDER BY n")
    assert [r[0] for r in cursor.fetchall()] == [1, 42, 9_000_000_000_000_000_000]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_uint64_overflow_raises(client_context):
    """A2: uint64 values above i64::MAX must be rejected, not wrapped negative."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowUInt64Overflow"
    cursor.execute(f"CREATE TABLE {table_name} (n BIGINT NOT NULL)")

    # 2**64 - 1 exceeds i64::MAX.
    source = pa.table({"n": pa.array([1, 18_446_744_073_709_551_615], type=pa.uint64())})

    with pytest.raises(ValueError, match="(?i)bigint"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_bigint_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable BIGINT column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableBigInt"
    cursor.execute(f"CREATE TABLE {table_name} (id BIGINT NOT NULL)")

    source = pa.table({"id": pa.array([1, None, 3], type=pa.int64())})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
