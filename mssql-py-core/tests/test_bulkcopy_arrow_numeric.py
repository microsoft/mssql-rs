# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for NUMERIC data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_numeric.py. SQL ``NUMERIC(p, s)`` is a synonym of
``DECIMAL(p, s)``; both accept Arrow ``decimal128(p, s)``.
"""
import pytest
import mssql_py_core
from decimal import Decimal

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_numeric_basic(client_context):
    """Arrow bulkcopy with two numeric columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableNumeric"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id NUMERIC(10, 2), value NUMERIC(18, 4))")

    source = pa.table(
        {
            "id": pa.array(
                [Decimal("1.00"), Decimal("2.50"), Decimal("3.99")],
                type=pa.decimal128(10, 2),
            ),
            "value": pa.array(
                [Decimal("100.5000"), Decimal("200.7500"), Decimal("300.9999")],
                type=pa.decimal128(18, 4),
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
    assert rows[0][0] == Decimal("1.00") and rows[0][1] == Decimal("100.5000")
    assert rows[2][0] == Decimal("3.99") and rows[2][1] == Decimal("300.9999")

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_numeric_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableNumeric"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id NUMERIC(10, 2), value NUMERIC(18, 4))")

    source = pa.table(
        {
            "id": pa.array(
                [Decimal("1.00"), Decimal("2.50"), Decimal("4.75")],
                type=pa.decimal128(10, 2),
            ),
            "value": pa.array(
                [Decimal("100.5000"), None, Decimal("400.1234")],
                type=pa.decimal128(18, 4),
            ),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == Decimal("100.5000")
    assert rows[1][1] is None
    assert rows[2][1] == Decimal("400.1234")

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_numeric_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable NUMERIC column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableNumeric"
    cursor.execute(f"CREATE TABLE {table_name} (id NUMERIC(10, 2) NOT NULL)")

    source = pa.table(
        {"id": pa.array([Decimal("1.00"), None, Decimal("3.00")], type=pa.decimal128(10, 2))}
    )

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
