# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy edge-case type coverage (cursor.bulkcopy_arrow).

Exercises Arrow source types and conversion paths that the per-type test files
do not already cover: the narrow integer widths (``int8``/``uint8``/``uint16``/
``uint32``), the null-typed column, ``float32`` targeting ``FLOAT``, the
``large_utf8``/``large_binary`` variants, ``date64``, the second/millisecond
``time32`` and nanosecond ``time64`` units, second/millisecond ``timestamp``
units, the money rescale-up/rescale-down branches plus money overflow, a
scale-0 decimal, and the legacy ``smalldatetime`` range/rounding edges.
"""
import datetime
from decimal import Decimal

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_narrow_integer_widths(client_context):
    """int8 / uint8 / uint16 / uint32 sources all load into integer columns."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowNarrowInts"
    cursor.execute(
        f"CREATE TABLE {table_name} "
        f"(c8 SMALLINT, cu8 SMALLINT, cu16 INT, cu32 BIGINT)"
    )

    source = pa.table(
        {
            "c8": pa.array([-5, 100], type=pa.int8()),
            "cu8": pa.array([200, 0], type=pa.uint8()),
            "cu16": pa.array([60000, 1], type=pa.uint16()),
            "cu32": pa.array([4_000_000_000, 2], type=pa.uint32()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT c8, cu8, cu16, cu32 FROM {table_name} ORDER BY c8")
    rows = cursor.fetchall()
    assert rows[0] == (-5, 200, 60000, 4_000_000_000)
    assert rows[1] == (100, 0, 1, 2)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_null_type_column(client_context):
    """An all-null Arrow column (type=null) loads NULLs into a nullable column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowNullType"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, v INT NULL)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "v": pa.nulls(3),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, v FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert [r[1] for r in rows] == [None, None, None]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_float32_to_float(client_context):
    """Arrow float32 widens into a SQL FLOAT (f64) column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowFloat32"
    cursor.execute(f"CREATE TABLE {table_name} (v FLOAT NOT NULL)")

    # 1.5 and 2.25 are exactly representable in both f32 and f64.
    source = pa.table({"v": pa.array([1.5, 2.25], type=pa.float32())})

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT v FROM {table_name} ORDER BY v")
    assert [r[0] for r in cursor.fetchall()] == [1.5, 2.25]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_large_utf8(client_context):
    """large_string loads into both NVARCHAR and VARCHAR columns."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowLargeUtf8"
    cursor.execute(f"CREATE TABLE {table_name} (n NVARCHAR(50), v VARCHAR(50))")

    source = pa.table(
        {
            "n": pa.array(["café", "naïve"], type=pa.large_string()),
            "v": pa.array(["hello", "world"], type=pa.large_string()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT n, v FROM {table_name} ORDER BY v")
    rows = cursor.fetchall()
    assert rows[0] == ("café", "hello")
    assert rows[1] == ("naïve", "world")

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_large_binary(client_context):
    """large_binary loads into a VARBINARY column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowLargeBinary"
    cursor.execute(f"CREATE TABLE {table_name} (b VARBINARY(50) NOT NULL)")

    source = pa.table(
        {"b": pa.array([b"\xde\xad", b"\xbe\xef"], type=pa.large_binary())}
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT b FROM {table_name} ORDER BY b")
    vals = [bytes(r[0]) for r in cursor.fetchall()]
    assert vals == [b"\xbe\xef", b"\xde\xad"]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_date64(client_context):
    """Arrow date64 (ms) loads into a SQL DATE column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowDate64"
    cursor.execute(f"CREATE TABLE {table_name} (d DATE NOT NULL)")

    source = pa.table(
        {
            "d": pa.array(
                [datetime.date(2020, 1, 2), datetime.date(1999, 12, 31)],
                type=pa.date64(),
            )
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT d FROM {table_name} ORDER BY d")
    assert [r[0] for r in cursor.fetchall()] == [
        datetime.date(1999, 12, 31),
        datetime.date(2020, 1, 2),
    ]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_time32_and_time64_ns(client_context):
    """time32(s) / time32(ms) / time64(ns) all load into TIME columns."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowTimeUnits"
    cursor.execute(
        f"CREATE TABLE {table_name} (ts TIME(0), tms TIME(3), tns TIME(7))"
    )

    source = pa.table(
        {
            "ts": pa.array([datetime.time(1, 2, 3)], type=pa.time32("s")),
            "tms": pa.array([datetime.time(1, 2, 3, 123000)], type=pa.time32("ms")),
            "tns": pa.array([datetime.time(1, 2, 3, 123456)], type=pa.time64("ns")),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT ts, tms, tns FROM {table_name}")
    row = cursor.fetchall()[0]
    assert row[0] == datetime.time(1, 2, 3)
    assert row[1] == datetime.time(1, 2, 3, 123000)
    assert row[2] == datetime.time(1, 2, 3, 123456)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_timestamp_second_and_millisecond(client_context):
    """timestamp(s) / timestamp(ms) load into DATETIME2 columns."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowTimestampUnits"
    cursor.execute(f"CREATE TABLE {table_name} (ds DATETIME2(0), dms DATETIME2(3))")

    source = pa.table(
        {
            "ds": pa.array(
                [datetime.datetime(2020, 1, 2, 3, 4, 5)], type=pa.timestamp("s")
            ),
            "dms": pa.array(
                [datetime.datetime(2020, 1, 2, 3, 4, 5, 123000)],
                type=pa.timestamp("ms"),
            ),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT ds, dms FROM {table_name}")
    row = cursor.fetchall()[0]
    assert row[0] == datetime.datetime(2020, 1, 2, 3, 4, 5)
    assert row[1] == datetime.datetime(2020, 1, 2, 3, 4, 5, 123000)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_money_rescale_up_and_down(client_context):
    """decimal128 at scale 2 (rescale up) and scale 5 (rescale down) into MONEY."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowMoneyRescale"
    cursor.execute(f"CREATE TABLE {table_name} (up MONEY, down MONEY)")

    source = pa.table(
        {
            # scale 2 -> money scale 4: multiply by 100 (rescale up).
            "up": pa.array([Decimal("123.45")], type=pa.decimal128(18, 2)),
            # scale 5 -> money scale 4: 1.23456 rounds half-away-from-zero to 1.2346.
            "down": pa.array([Decimal("1.23456")], type=pa.decimal128(18, 5)),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT up, down FROM {table_name}")
    row = cursor.fetchall()[0]
    assert row[0] == Decimal("123.45")
    assert row[1] == Decimal("1.2346")

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_money_overflow_raises(client_context):
    """A decimal whose ×10⁴ scaling exceeds i64 must be rejected for MONEY."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowMoneyOverflow"
    cursor.execute(f"CREATE TABLE {table_name} (amt MONEY NOT NULL)")

    # 1e15 scaled by 10^4 = 1e19 > i64::MAX.
    source = pa.table(
        {"amt": pa.array([Decimal("1000000000000000")], type=pa.decimal128(38, 0))}
    )

    with pytest.raises(ValueError, match="(?i)money"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_decimal_scale_zero(client_context):
    """A scale-0 decimal128 loads into a DECIMAL(p,0) column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowDecimalScale0"
    cursor.execute(f"CREATE TABLE {table_name} (v DECIMAL(18,0) NOT NULL)")

    source = pa.table(
        {"v": pa.array([Decimal("42"), Decimal("-7")], type=pa.decimal128(18, 0))}
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT v FROM {table_name} ORDER BY v")
    assert [r[0] for r in cursor.fetchall()] == [Decimal("-7"), Decimal("42")]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smalldatetime_out_of_range_raises(client_context):
    """A timestamp before 1900 must be rejected for SMALLDATETIME."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSmallDateTimeRange"
    cursor.execute(f"CREATE TABLE {table_name} (ts SMALLDATETIME NOT NULL)")

    source = pa.table(
        {"ts": pa.array([datetime.datetime(1899, 1, 1)], type=pa.timestamp("us"))}
    )

    with pytest.raises(ValueError, match="(?i)smalldatetime"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_smalldatetime_rounds_across_day(client_context):
    """23:59:45 rounds up a full minute, carrying hour and day forward."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSmallDateTimeCarry"
    cursor.execute(f"CREATE TABLE {table_name} (ts SMALLDATETIME NOT NULL)")

    source = pa.table(
        {
            "ts": pa.array(
                [datetime.datetime(1970, 1, 1, 23, 59, 45)], type=pa.timestamp("us")
            )
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT ts FROM {table_name}")
    assert cursor.fetchall()[0][0] == datetime.datetime(1970, 1, 2, 0, 0)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_xml_from_large_string(client_context):
    """XML destination reads from a large_string source (read_utf8 large path)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowXmlLargeString"
    cursor.execute(f"CREATE TABLE {table_name} (doc XML NOT NULL)")

    source = pa.table(
        {"doc": pa.array(["<r><a>1</a></r>"], type=pa.large_string())}
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT CAST(doc AS NVARCHAR(MAX)) FROM {table_name}")
    assert cursor.fetchall()[0][0] == "<r><a>1</a></r>"

    conn.close()
