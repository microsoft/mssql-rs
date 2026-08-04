# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for MONEY / SMALLMONEY data types (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_money.py. Arrow ``decimal128(p, s)`` maps to SQL ``MONEY``
and ``SMALLMONEY`` (both scaled fixed-point, 4 decimal places).
"""
from decimal import Decimal

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_money_basic(client_context):
    """Arrow decimal128 bulkcopy into MONEY and SMALLMONEY columns."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableMoney"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (amt MONEY, small_amt SMALLMONEY)")

    source = pa.table(
        {
            "amt": pa.array(
                [Decimal("123.45"), Decimal("-9999.99"), Decimal("0.00")],
                type=pa.decimal128(18, 4),
            ),
            "small_amt": pa.array(
                [Decimal("12.34"), Decimal("-100.50"), Decimal("0.00")],
                type=pa.decimal128(10, 4),
            ),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "amt"), (1, "small_amt")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT amt, small_amt FROM {table_name} ORDER BY amt")
    rows = cursor.fetchall()
    assert rows[0][0] == Decimal("-9999.99") and rows[0][1] == Decimal("-100.50")
    assert rows[2][0] == Decimal("123.45") and rows[2][1] == Decimal("12.34")

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_money_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableMoney"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, amt MONEY)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "amt": pa.array(
                [Decimal("100.00"), None, Decimal("300.75")], type=pa.decimal128(18, 4)
            ),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, amt FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == Decimal("100.00")
    assert rows[1][1] is None
    assert rows[2][1] == Decimal("300.75")

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smallmoney_overflow_raises(client_context):
    """A decimal beyond SMALLMONEY range must be rejected, not wrapped."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSmallMoneyOverflow"
    cursor.execute(f"CREATE TABLE {table_name} (amt SMALLMONEY NOT NULL)")

    # SMALLMONEY max is 214748.3647.
    source = pa.table(
        {"amt": pa.array([Decimal("300000.0000")], type=pa.decimal128(18, 4))}
    )

    with pytest.raises(ValueError, match="(?i)smallmoney"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_money_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable MONEY column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowNonNullableMoney"
    cursor.execute(f"CREATE TABLE {table_name} (amt MONEY NOT NULL)")

    source = pa.table(
        {"amt": pa.array([Decimal("1.00"), None], type=pa.decimal128(18, 4))}
    )

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
