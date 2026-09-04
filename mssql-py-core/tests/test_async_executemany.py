# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for asynchronous cursor ExecuteMany execution and parameter binding."""

import asyncio
import itertools
import sys
import threading
from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal
import uuid
import warnings

import pytest

import mssql_py_core


class RecordingLogger:
    def __init__(self):
        self.events = []

    def py_core_log(self, level, message, module_name, _line):
        self.events.append((level, message, module_name))


async def connect(client_context, python_logger=None, *, autocommit=True):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await mssql_py_core.PyAsyncConnection.connect(
            client_context, python_logger, autocommit=autocommit
        )


async def execute(cursor, operation, *parameters):
    return await cursor.execute(operation, *parameters, use_prepare=False)


@pytest.mark.integration
def test_executemany_integer_boolean_null_and_rowcount_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(cursor, "CREATE TABLE #em_ints (id int, value bigint NULL)")
            rows = [
                (1, 0),
                (2, 2**31 - 1),
                (3, -(2**31)),
                (4, 2**63 - 1),
                (5, -(2**63)),
                (6, True),
                (7, False),
                (8, None),
            ]
            await cursor.executemany("INSERT INTO #em_ints VALUES (?, ?)", rows)
            assert cursor.rowcount == len(rows)
            await execute(cursor, "SELECT id, value FROM #em_ints ORDER BY id")
            assert await cursor.fetchall() == [
                (1, 0),
                (2, 2**31 - 1),
                (3, -(2**31)),
                (4, 2**63 - 1),
                (5, -(2**63)),
                (6, 1),
                (7, 0),
                (8, None),
            ]
        finally:
            await conn.close()

    asyncio.run(run())
@pytest.mark.integration
def test_executemany_empty_unicode_and_utf16_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_text (id int, value nvarchar(max) NULL, fixed nvarchar(5))",
            )
            rows = [
                (1, "", ""),
                (2, None, "Test"),
                (3, "Hello 😄", "😀😀"),
                (4, "中文", "A😀B"),
                (5, "Ñice tëxt", "12345"),
                (6, " " * 2, " "),
                (7, "\t\n", "Hi"),
                (8, "Ω" * 4100, "World"),
                (9, "漢" * 5000, "Valid"),
            ]
            await cursor.executemany("INSERT INTO #em_text VALUES (?, ?, ?)", rows)
            await execute(cursor, "SELECT id, value, fixed FROM #em_text ORDER BY id")
            assert await cursor.fetchall() == rows

            with pytest.raises(mssql_py_core.DatabaseError):
                await cursor.executemany(
                    "INSERT INTO #em_text VALUES (?, ?, ?)",
                    [(10, "oversized", "😀😀😀")],
                )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_binary_empty_null_bytearray_and_max_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_binary (id int, value varbinary(max) NULL)",
            )
            rows = [
                (1, b""),
                (2, b"hello"),
                (3, bytearray(b"bytearray")),
                (4, b"\x00\x01\x02"),
                (5, bytearray()),
                (6, None),
                (7, b"X" * 3500),
                (8, b"Y" * 4100),
                (9, b"Z" * 5000),
            ]
            cursor.setinputsizes([(4, 0, 0), (-4, 0, 0)])
            await cursor.executemany("INSERT INTO #em_binary VALUES (?, ?)", rows)
            await execute(cursor, "SELECT id, value FROM #em_binary ORDER BY id")
            actual = await cursor.fetchall()
            expected = [(row_id, None if value is None else bytes(value)) for row_id, value in rows]
            assert actual == expected
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_all_null_columns_and_typed_hints_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_nulls (id int, text_value varchar(50), "
                "int_value int, binary_value varbinary(32))",
            )
            cursor.setinputsizes([(4, 0, 0), (12, 50, 0), (4, 0, 0), (-3, 32, 0)])
            rows = [(1, None, None, None), (2, None, None, None)]
            await cursor.executemany("INSERT INTO #em_nulls VALUES (?, ?, ?, ?)", rows)
            await execute(cursor, "SELECT * FROM #em_nulls ORDER BY id")
            assert await cursor.fetchall() == rows
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_all_null_convertible_columns_without_hints_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_untyped_nulls (id int, text_value varchar(50), "
                "int_value int, date_value date)",
            )
            rows = [(1, None, None, None), (2, None, None, None)]
            await cursor.executemany(
                "INSERT INTO #em_untyped_nulls VALUES (?, ?, ?, ?)", rows
            )
            await execute(cursor, "SELECT * FROM #em_untyped_nulls ORDER BY id")
            assert await cursor.fetchall() == rows
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_mixed_null_and_typed_values_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_mixed_nulls (col1 int, col2 varchar(50), "
                "col3 float, col4 bit, col5 datetime2(6), col6 decimal(10, 2), "
                "col7 nvarchar(100), col8 bigint, col9 date, col10 real)",
            )
            rows = [
                (
                    index if index % 3 else None,
                    f"text_{index}" if index % 2 == 0 else None,
                    float(index * 1.5) if index % 4 else None,
                    True if index % 5 == 0 else (False if index % 5 == 1 else None),
                    datetime(2025, 1, 1, 12, 0) if index % 6 else None,
                    Decimal(f"{index}.99") if index % 3 else None,
                    f"desc_{index}" if index % 7 else None,
                    index * 100 if index % 8 else None,
                    date(2025, 1, 1) if index % 9 else None,
                    float(index / 2) if index % 10 else None,
                )
                for index in range(50)
            ]
            await cursor.executemany(
                "INSERT INTO #em_mixed_nulls VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rows,
            )
            assert cursor.rowcount == len(rows)
            await execute(
                cursor,
                "SELECT COUNT(*), COUNT(col1), COUNT(col2), COUNT(col3) "
                "FROM #em_mixed_nulls",
            )
            assert await cursor.fetchone() == (50, 33, 25, 37)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_decimal_precision_sign_null_and_money_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_decimal (id int, amount decimal(38, 10), "
                "money_value money NULL, smallmoney_value smallmoney NULL)",
            )
            rows = [
                (1, Decimal("0.1"), Decimal("12345.6789"), Decimal("987.6543")),
                (2, Decimal("-0.1"), Decimal("0.0001"), Decimal("0.0100")),
                (3, Decimal("999999999999999999.123456"), None, Decimal("42.4200")),
                (4, Decimal("-999999999999999999.654321"), Decimal("-1000.9900"), None),
                (5, None, None, None),
                (6, "35.1128407822", Decimal("1.0000"), Decimal("2.0000")),
            ]
            await cursor.executemany("INSERT INTO #em_decimal VALUES (?, ?, ?, ?)", rows)
            await execute(
                cursor,
                "SELECT id, amount, money_value, smallmoney_value FROM #em_decimal ORDER BY id",
            )
            expected = rows[:-1] + [
                (6, Decimal("35.1128407822"), Decimal("1.0000"), Decimal("2.0000"))
            ]
            assert await cursor.fetchall() == expected
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_decimal_and_float_setinputsizes_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_hints (id int, decimal_value decimal(18, 2), "
                "numeric_value numeric(10, 4), real_value real)",
            )
            cursor.setinputsizes([(4, 0, 0), (3, 18, 2), (2, 10, 4), (7, 0, 0)])
            rows = [
                (1, Decimal("19.99"), Decimal("123.4567"), 10.99),
                (2, Decimal("29.99"), Decimal("-99.0001"), 20.50),
                (3, Decimal("0.01"), Decimal("0.0000"), 30.75),
            ]
            await cursor.executemany("INSERT INTO #em_hints VALUES (?, ?, ?, ?)", rows)
            await execute(cursor, "SELECT * FROM #em_hints ORDER BY id")
            actual = await cursor.fetchall()
            assert [row[:3] for row in actual] == [row[:3] for row in rows]
            assert [row[3] for row in actual] == pytest.approx([row[3] for row in rows], abs=0.001)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_temporal_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_temporal (id int, time_value time(6), date_value date, "
                "datetime_value datetime2(6), offset_value datetimeoffset(6))",
            )
            rows = [
                (
                    1,
                    time(9, 0),
                    date(2025, 1, 1),
                    datetime(2025, 1, 1, 12, 0),
                    datetime(2023, 10, 26, 10, 30, tzinfo=timezone(timedelta(hours=5, minutes=30))),
                ),
                (
                    2,
                    time(14, 30, 15, 234567),
                    date(2025, 2, 2),
                    datetime(2025, 2, 2, 13, 1, 2, 234567),
                    datetime(2023, 10, 27, 15, 45, 10, 123456, tzinfo=timezone(timedelta(hours=-8))),
                ),
                (
                    3,
                    time(23, 59, 59, 999999),
                    date(2025, 3, 3),
                    datetime(2025, 3, 3, 23, 59, 59, 999999),
                    datetime(2023, 10, 30, tzinfo=timezone(timedelta(hours=14))),
                ),
            ]
            await cursor.executemany("INSERT INTO #em_temporal VALUES (?, ?, ?, ?, ?)", rows)
            await execute(cursor, "SELECT * FROM #em_temporal ORDER BY id")
            assert await cursor.fetchall() == rows
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_uuid_and_output_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(cursor, "CREATE TABLE #em_uuid (id int, value uniqueidentifier NULL)")
            values = [uuid.uuid4(), uuid.UUID("12345678-1234-5678-1234-567812345678"), None]
            await cursor.executemany(
                "INSERT INTO #em_uuid (id, value) OUTPUT INSERTED.value VALUES (?, ?)",
                [list(row) for row in enumerate(values, 1)],
            )
            for index, value in enumerate(values):
                assert await cursor.fetchone() == (value,)
                assert await cursor.nextset() is (index + 1 < len(values))
            await execute(cursor, "SELECT value FROM #em_uuid ORDER BY id")
            assert await cursor.fetchall() == [(value,) for value in values]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_xml_geography_and_large_text_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(cursor, "CREATE TABLE #em_xml (id int, value xml)")
            xml_rows = [(index, f"<root><item>{index}</item></root>") for index in range(5)]
            await cursor.executemany("INSERT INTO #em_xml VALUES (?, ?)", xml_rows)
            await execute(cursor, "SELECT id, CONVERT(nvarchar(max), value) FROM #em_xml ORDER BY id")
            assert await cursor.fetchall() == xml_rows

            await execute(cursor, "CREATE TABLE #em_geo (id int, value geography, name nvarchar(50))")
            geo_rows = [
                (1, "POINT(-122.34900 47.65100)", "Point"),
                (2, "LINESTRING(-122.349 47.651, -122.348 47.652)", "Line"),
            ]
            await cursor.executemany(
                "INSERT INTO #em_geo VALUES (?, geography::STGeomFromText(?, 4326), ?)",
                geo_rows,
            )
            await execute(cursor, "SELECT id, name FROM #em_geo ORDER BY id")
            assert await cursor.fetchall() == [(1, "Point"), (2, "Line")]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_named_parameter_error_and_extra_key_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.executemany(
                "SELECT %(id)s", [{"id": 1, "extra": "ignored"}, {"id": 2, "other": 3}]
            )
            assert await cursor.fetchall() == [(1,)]
            assert await cursor.nextset() is True
            assert await cursor.fetchall() == [(2,)]

            with pytest.raises(KeyError, match="name"):
                await cursor.executemany(
                    "SELECT %(id)s, %(name)s",
                    [{"id": 1, "name": "Alice"}, {"id": 2}],
                )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_row_objects_and_large_values_parity(client_context):
    class Row:
        def __init__(self, *values):
            self._values = values

        def __iter__(self):
            return iter(self._values)

    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(cursor, "CREATE TABLE #em_rows (id int, value varchar(max))")
            rows = [Row(1, "X" * 5000), Row(2, "Y" * 8001)]
            await cursor.executemany("INSERT INTO #em_rows VALUES (?, ?)", rows)
            await execute(cursor, "SELECT id, value FROM #em_rows ORDER BY id")
            assert await cursor.fetchall() == [(1, "X" * 5000), (2, "Y" * 8001)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_matches_individual_execute_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(cursor, "CREATE TABLE #em_compare (id int, value nvarchar(50))")
            rows = [(1, ""), (2, "test"), (3, None), (4, "another")]
            for row in rows:
                await execute(cursor, "INSERT INTO #em_compare VALUES (?, ?)", *row)
            await execute(cursor, "SELECT * FROM #em_compare ORDER BY id")
            individual = await cursor.fetchall()
            await execute(cursor, "TRUNCATE TABLE #em_compare")
            await cursor.executemany("INSERT INTO #em_compare VALUES (?, ?)", rows)
            await execute(cursor, "SELECT * FROM #em_compare ORDER BY id")
            assert await cursor.fetchall() == individual
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_truncation_reports_failed_row_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(cursor, "CREATE TABLE #em_truncation (id int, value nvarchar(5))")
            with pytest.raises(mssql_py_core.DatabaseError, match="parameter row 1"):
                await cursor.executemany(
                    "INSERT INTO #em_truncation VALUES (?, ?)",
                    [(1, "valid"), (2, "too long")],
                )
            await execute(cursor, "SELECT id, value FROM #em_truncation ORDER BY id")
            assert await cursor.fetchall() == [(1, "valid")]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.longhaul
def test_executemany_large_batch_mixed_types_parity(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await execute(
                cursor,
                "CREATE TABLE #em_stress (id int, int_value int, float_value float, "
                "text_value nvarchar(100), binary_value varbinary(50), "
                "decimal_value decimal(18, 6), null_value nvarchar(50))",
            )
            row_count = 10_000
            rows = [
                (
                    index,
                    index * 2,
                    float(index) * 1.5,
                    f"str_{index}",
                    bytes([index % 256]) * 10,
                    Decimal(index) / Decimal(1000),
                    None,
                )
                for index in range(row_count)
            ]
            await cursor.executemany(
                "INSERT INTO #em_stress VALUES (?, ?, ?, ?, ?, ?, ?)", rows
            )
            assert cursor.rowcount == row_count
            await execute(
                cursor,
                "SELECT id, int_value, float_value, text_value, binary_value, "
                "decimal_value, null_value FROM #em_stress "
                "WHERE id IN (0, 1, 500, 5000, 9999) ORDER BY id",
            )
            actual = await cursor.fetchall()
            expected = [rows[index] for index in (0, 1, 500, 5000, 9999)]
            assert actual == expected
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_preflight_parameter_shape_parity(mock_client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(mock_client_context, logger)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match="row 1"):
                await cursor.executemany("SELECT ?, ?", [(1, 2), (3,)])
            assert any(
                level == 40
                and "PyAsyncCursor::executemany: parameter preflight failed" in message
                and "row 1" in message
                for level, message, _module in logger.events
            )
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_rejects_non_finite_decimals_during_preflight(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            for value in (Decimal("NaN"), Decimal("Infinity"), Decimal("-Infinity")):
                with pytest.raises(TypeError):
                    await cursor.executemany("SELECT ?", [(value,)])
        finally:
            await conn.close()

    asyncio.run(run())

def test_executemany_empty_input_returns_cursor_and_sets_rowcount(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            assert await cursor.executemany("SELECT ?", []) is cursor
            assert cursor.rowcount == 0
            assert cursor.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.nextset()
        finally:
            await conn.close()

    asyncio.run(run())


def test_empty_executemany_releases_pending_results_for_another_cursor(
    mock_client_context,
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            first = conn.cursor()
            second = conn.cursor()
            await first.execute("SELECT 1", use_prepare=False)
            await first.executemany("SELECT ?", [])
            assert first.rowcount == 0
            assert await second.execute("SET NOCOUNT ON", use_prepare=False) is second
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_preflight_error_is_raised_by_awaitable(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            awaitable = cursor.executemany("SELECT ?", [(1,), 2])
            assert hasattr(awaitable, "__await__")
            with pytest.raises(TypeError, match="row 1"):
                await awaitable
            assert cursor.rowcount == -1
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_preflight_yields_to_event_loop(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        running = True
        ticks = 0

        async def ticker():
            nonlocal ticks
            while running:
                ticks += 1
                await asyncio.sleep(0)

        ticker_task = asyncio.create_task(ticker())
        try:
            await asyncio.sleep(0)
            ticks_before = ticks
            rows = [(value,) for value in range(4096)]
            rows.append(1)
            with pytest.raises(TypeError, match="row 4096"):
                await conn.cursor().executemany("SELECT ?", rows)
            assert ticks > ticks_before
        finally:
            running = False
            await ticker_task
            await conn.close()

    asyncio.run(run())


def test_executemany_preflight_cancellation_keeps_session_usable(mock_client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(mock_client_context, logger)
        try:
            cursor = conn.cursor()
            logger.events.clear()
            task = asyncio.ensure_future(
                cursor.executemany("SELECT ?", itertools.repeat((1,)))
            )
            await asyncio.sleep(0)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task

            assert await cursor.execute("SET NOCOUNT ON", use_prepare=False) is cursor
            assert any(
                level == 30
                and "PyAsyncCursor::executemany: interrupted during parameter preflight"
                in message
                and "connection remains usable" in message
                and module == "async_execute.rs"
                for level, message, module in logger.events
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_rejects_cursor_closed_during_preflight(client_context):
    class BlockingRows:
        def __init__(self):
            self.index = 0
            self.blocked = threading.Event()
            self.release = threading.Event()

        def __iter__(self):
            return self

        def __next__(self):
            if self.index == 256:
                self.blocked.set()
                if not self.release.wait(timeout=10):
                    raise RuntimeError("Timed out waiting to resume parameter preflight")
            if self.index == 512:
                raise StopIteration
            row = (self.index,)
            self.index += 1
            return row

    async def run():
        conn = await connect(client_context)
        rows = BlockingRows()
        try:
            setup = conn.cursor()
            await setup.execute(
                "CREATE TABLE #async_executemany_close_race (value int)",
                use_prepare=False,
            )

            cursor = conn.cursor()
            pending = asyncio.ensure_future(
                cursor.executemany(
                    "INSERT INTO #async_executemany_close_race VALUES (?)",
                    rows,
                    use_prepare=False,
                )
            )
            while not rows.blocked.is_set():
                await asyncio.sleep(0)

            await cursor.close()
            rows.release.set()
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await pending

            probe = conn.cursor()
            await probe.execute(
                "SELECT COUNT(*) FROM #async_executemany_close_race",
                use_prepare=False,
            )
            assert await probe.fetchone() == (0,)
        finally:
            rows.release.set()
            await conn.close()

    asyncio.run(run())


def test_executemany_rejects_mixed_mapping_and_positional_rows(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match="Mixed parameter types.*row 1"):
                await cursor.executemany(
                    "SELECT %(value)s", [{"value": 1}, (2,)]
                )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_executemany_inserts_rows_and_aggregates_rowcount(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_executemany_rowcount (id int, value nvarchar(20))",
                use_prepare=False,
            )
            assert (
                await cursor.executemany(
                    "INSERT INTO #async_executemany_rowcount VALUES (?, ?)",
                    [(1, "one"), (2, "two"), (3, "three")],
                    use_prepare=use_prepare,
                )
                is cursor
            )
            assert cursor.rowcount == 3
            assert cursor.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchone()
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchmany()
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchall()
            assert await cursor.nextset() is False
            assert await cursor.nextset() is False
            assert cursor.rowcount == 3

            await cursor.execute(
                "SELECT id, value FROM #async_executemany_rowcount ORDER BY id",
                use_prepare=False,
            )
            assert await cursor.fetchall() == [(1, "one"), (2, "two"), (3, "three")]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_reaches_execution_yield_boundary(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_executemany_yield (value int)",
                use_prepare=False,
            )
            logger.events.clear()

            await cursor.executemany(
                "INSERT INTO #async_executemany_yield VALUES (?)",
                [(value,) for value in range(256)],
                use_prepare=False,
            )

            assert any(
                level == 10
                and "yielding at execution interval; completed=256" in message
                and module == "async_execute.rs"
                for level, message, module in logger.events
            ), logger.events
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_accepts_named_rows_and_outer_generator(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_executemany_named (id int, value int)",
                use_prepare=False,
            )
            rows = ({"id": value, "value": value * 10} for value in range(1, 4))
            await cursor.executemany(
                "INSERT INTO #async_executemany_named VALUES (%(id)s, %(value)s)",
                rows,
            )
            assert cursor.rowcount == 3
            await cursor.execute(
                "SELECT id, value FROM #async_executemany_named ORDER BY id",
                use_prepare=False,
            )
            assert await cursor.fetchall() == [(1, 10), (2, 20), (3, 30)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_reports_failed_row_and_preserves_prior_autocommit_rows(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_executemany_error (id int PRIMARY KEY)",
                use_prepare=False,
            )
            with pytest.raises(mssql_py_core.DatabaseError, match="parameter row 2"):
                await cursor.executemany(
                    "INSERT INTO #async_executemany_error VALUES (?)",
                    [(1,), (2,), (1,), (3,)],
                )
            assert cursor.rowcount == -1
            await cursor.execute(
                "SELECT id FROM #async_executemany_error ORDER BY id",
                use_prepare=False,
            )
            assert await cursor.fetchall() == [(1,), (2,)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_buffers_row_results_and_preserves_boundaries(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.executemany(
                "SELECT CAST(? AS int) AS value",
                [(1,), (2,), (3,)],
                use_prepare=False,
            )
            assert cursor.rowcount == -1
            assert cursor.description[0][0] == "value"
            assert await cursor.fetchall() == [(1,)]
            assert await cursor.nextset() is True
            assert await cursor.fetchall() == [(2,)]
            assert await cursor.nextset() is True
            assert await cursor.fetchall() == [(3,)]
            assert await cursor.nextset() is False
            assert cursor.description is None
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_buffered_fetch_tracks_partial_and_exhausted_states(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            await cursor.executemany(
                "SELECT value FROM (VALUES (?), (?), (?)) rows(value) ORDER BY value",
                [(1, 2, 3)],
                use_prepare=False,
            )
            logger.events.clear()

            assert await cursor.fetchone() == (1,)
            assert await cursor.fetchall() == [(2,), (3,)]
            assert await cursor.fetchall() == []
            assert any(
                level == 10
                and "read buffered ExecuteMany rows" in message
                and module == "async_fetch.rs"
                for level, message, module in logger.events
            ), logger.events
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_exhausted_nextset_checks_connection_lifecycle(client_context):
    async def run():
        conn = await connect(client_context)
        cursor = conn.cursor()
        try:
            await cursor.executemany(
                "SELECT CAST(? AS int) AS value",
                [(1,)],
                use_prepare=False,
            )
            assert await cursor.fetchall() == [(1,)]
            description = cursor.description

            close_awaitable = conn.close()
            # Shutdown may complete before this coroutine gets its next turn.
            with pytest.raises(RuntimeError, match=r"Connection is (?:closing|closed)"):
                await cursor.nextset()
            assert cursor.description == description

            await close_awaitable
            with pytest.raises(RuntimeError, match="Connection is closed"):
                await cursor.nextset()
            assert cursor.description == description
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_close_clears_unread_buffered_result_state(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.executemany(
                "SELECT TOP (512) CAST(? AS int) AS value FROM sys.all_objects",
                [(7,)],
                use_prepare=False,
            )
            assert cursor.description is not None

            await cursor.close()
            assert cursor.description is None

            probe = conn.cursor()
            await probe.execute("SELECT 1", use_prepare=False)
            assert await probe.fetchone() == (1,)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_nextset_can_skip_unread_final_buffered_set(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            await cursor.executemany(
                "SELECT CAST(? AS int) AS value",
                [(1,)],
                use_prepare=False,
            )
            assert await cursor.nextset() is False
            assert cursor.description is None
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_buffered_nextset_description_failure_clears_results(client_context):
    async def run():
        conn = await connect(client_context)
        decimal_module = sys.modules["decimal"]
        try:
            cursor = conn.cursor()
            await cursor.executemany(
                "SELECT CAST(? AS int) AS value; "
                "SELECT CAST(? AS decimal(10, 2)) AS value",
                [(1, 2)],
                use_prepare=False,
            )
            assert await cursor.fetchall() == [(1,)]

            sys.modules["decimal"] = None
            with pytest.raises(
                mssql_py_core.InternalError,
                match="Advanced buffered result set but cursor description materialization failed",
            ):
                await cursor.nextset()
            assert cursor.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchone()
        finally:
            sys.modules["decimal"] = decimal_module
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_buffered_awaitable_creation_failure_restores_fetch_state(
    client_context,
):
    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    conn = None

    async def setup():
        connection = await connect(client_context)
        cursor = connection.cursor()
        await cursor.executemany(
            "SELECT CAST(? AS int) AS value",
            [(1,), (2,)],
            use_prepare=False,
        )
        return connection, cursor

    try:
        conn, cursor = loop.run_until_complete(setup())
        asyncio.set_event_loop(None)
        with pytest.raises(RuntimeError, match="running event loop"):
            cursor.fetchone()

        asyncio.set_event_loop(loop)

        async def fetch_first():
            return await cursor.fetchone()

        assert loop.run_until_complete(fetch_first()) == (1,)

        asyncio.set_event_loop(None)
        with pytest.raises(RuntimeError, match="running event loop"):
            cursor.nextset()

        asyncio.set_event_loop(loop)

        async def advance_and_fetch():
            assert await cursor.nextset() is True
            return await cursor.fetchone()

        assert loop.run_until_complete(advance_and_fetch()) == (2,)
    finally:
        asyncio.set_event_loop(loop)
        if conn is not None:
            async def close_connection():
                await conn.close()

            loop.run_until_complete(close_connection())
        loop.close()
        asyncio.set_event_loop(None)


@pytest.mark.integration
def test_executemany_reaches_result_buffering_yield_boundary(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            logger.events.clear()

            await cursor.executemany(
                "SELECT TOP (256) CAST(? AS int) AS value FROM sys.all_objects",
                [(7,)],
                use_prepare=False,
            )

            assert any(
                level == 10
                and "yielding at result_buffering interval; completed=256" in message
                and module == "async_execute.rs"
                for level, message, module in logger.events
            ), logger.events
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_collects_secondary_dml_rowcount(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_executemany_secondary_count (value int)",
                use_prepare=False,
            )
            await cursor.executemany(
                "SELECT CAST(? AS int) AS selected; "
                "INSERT INTO #async_executemany_secondary_count VALUES (?)",
                [(1, 10), (2, 20)],
                use_prepare=False,
            )
            assert cursor.rowcount == -1
            assert await cursor.fetchall() == [(1,)]
            assert await cursor.nextset() is True
            assert await cursor.fetchall() == [(2,)]
            assert await cursor.nextset() is False

            await cursor.execute(
                "SELECT value FROM #async_executemany_secondary_count ORDER BY value",
                use_prepare=False,
            )
            assert await cursor.fetchall() == [(10,), (20,)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_buffers_insert_output_rows(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_executemany_output (id int IDENTITY, value int)",
                use_prepare=False,
            )
            await cursor.executemany(
                "INSERT INTO #async_executemany_output (value) OUTPUT INSERTED.id, INSERTED.value VALUES (?)",
                [(10,), (20,)],
            )
            assert cursor.rowcount == -1
            assert [column[0] for column in cursor.description] == ["id", "value"]
            assert await cursor.fetchall() == [(1, 10)]
            assert await cursor.nextset() is True
            assert await cursor.fetchall() == [(2, 20)]
            assert await cursor.nextset() is False
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_preserves_multiple_result_sets_per_parameter_row(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.executemany(
                "SELECT CAST(? AS int) AS value; SELECT CAST(? AS int) + 100 AS value",
                [(1, 1), (2, 2)],
                use_prepare=False,
            )
            expected = [[(1,)], [(101,)], [(2,)], [(102,)]]
            for index, rows in enumerate(expected):
                assert await cursor.fetchall() == rows
                assert await cursor.nextset() is (index + 1 < len(expected))
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_executemany_applies_setinputsizes_and_consumes_hints(
    client_context, use_prepare
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-3, 16, 0)])  # SQL_VARBINARY
            await cursor.executemany(
                "IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> 'varbinary' "
                "THROW 50000, 'Unexpected type', 1",
                [(None,), (b"value",)],
                use_prepare=use_prepare,
            )
            with pytest.raises(TypeError, match="Unsupported Python type"):
                await cursor.executemany("SELECT ?", [({1, 2},)])
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_executemany_binds_json(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([mssql_py_core.SQL_JSON, 4])
            await cursor.executemany(
                "IF JSON_VALUE(?, '$.answer') <> ? THROW 50000, 'Invalid JSON', 1",
                [({"answer": 42}, 42), ({"answer": -7}, -7)],
                use_prepare=use_prepare,
            )
            assert cursor.rowcount == -1
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_executemany_binds_vector(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes(
                [(mssql_py_core.SQL_VECTOR, 3, 0), (mssql_py_core.SQL_VECTOR, 3, 0)]
            )
            await cursor.executemany(
                "IF VECTOR_DISTANCE('euclidean', ?, ?) <> 0 "
                "THROW 50000, 'Invalid VECTOR', 1",
                [
                    ([1.0, 2.0, 3.0], [1.0, 2.0, 3.0]),
                    ([-4.5, 0.0, 9.25], [-4.5, 0.0, 9.25]),
                ],
                use_prepare=use_prepare,
            )
            assert cursor.rowcount == -1
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_executemany_binds_table_valued_parameters(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        type_name = f"PyAsyncExecuteManyTvp_{uuid.uuid4().hex}"
        qualified_type_name = f"dbo.{type_name}"
        cursor = conn.cursor()
        try:
            await cursor.execute(
                f"CREATE TYPE dbo.[{type_name}] AS TABLE (id INT, value NVARCHAR(50))"
            )
            populated = mssql_py_core.TableValuedParameter(
                qualified_type_name,
                [(4, 0, 0), (-9, 50, 0)],
                [(1, "first"), (2, "second")],
            )
            empty = mssql_py_core.TableValuedParameter(
                qualified_type_name,
                [(4, 0, 0), (-9, 50, 0)],
                [],
            )
            null = mssql_py_core.TableValuedParameter(qualified_type_name)
            await cursor.executemany(
                "IF (SELECT COUNT(*) FROM ?) <> ? "
                "THROW 50000, 'Unexpected TVP row count', 1",
                [(populated, 2), (empty, 0), (null, 0)],
                use_prepare=use_prepare,
            )
            assert cursor.rowcount == -1
        finally:
            try:
                await cursor.execute(f"DROP TYPE IF EXISTS dbo.[{type_name}]")
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_supports_heterogeneous_inferred_signatures(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_executemany_signatures (value sql_variant)",
                use_prepare=False,
            )
            await cursor.executemany(
                "INSERT INTO #async_executemany_signatures VALUES (?)",
                [(1,), ("text",), (Decimal("1.25"),), (None,)],
            )
            assert cursor.rowcount == 4
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_error_can_be_rolled_back_in_explicit_transaction(client_context):
    async def run():
        conn = await connect(client_context, autocommit=False)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #async_executemany_transaction (id int PRIMARY KEY)",
                use_prepare=False,
            )
            with pytest.raises(mssql_py_core.DatabaseError, match="parameter row 2"):
                await cursor.executemany(
                    "INSERT INTO #async_executemany_transaction VALUES (?)",
                    [(1,), (2,), (1,)],
                )
            await conn.rollback()
            await cursor.execute(
                "SELECT OBJECT_ID('tempdb..#async_executemany_transaction')",
                use_prepare=False,
            )
            assert await cursor.fetchone() == (None,)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_logs_completion_and_execution_failure(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            await execute(cursor, "CREATE TABLE #em_trace (id int PRIMARY KEY)")
            logger.events.clear()

            await cursor.executemany("INSERT INTO #em_trace VALUES (?)", [(1,), (2,)])
            assert any(
                level == 20
                and "PyAsyncCursor::executemany: completed" in message
                and "batch_count=2" in message
                and "preflight_ms=" in message
                and "execution_ms=" in message
                and "elapsed_ms=" in message
                for level, message, _module in logger.events
            )

            logger.events.clear()
            with pytest.raises(mssql_py_core.DatabaseError):
                await cursor.executemany("INSERT INTO #em_trace VALUES (?)", [(3,), (1,)])
            assert any(
                level == 40
                and "PyAsyncCursor::executemany: failed" in message
                and "failed_row_index=1" in message
                and "error=" in message
                for level, message, _module in logger.events
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_logs_description_materialization_failure(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        decimal_module = sys.modules["decimal"]
        try:
            cursor = conn.cursor()
            logger.events.clear()
            sys.modules["decimal"] = None
            with pytest.raises(
                mssql_py_core.InternalError,
                match="cursor description materialization failed",
            ):
                await cursor.executemany(
                    "SELECT CAST(? AS decimal(10, 2)) AS value",
                    [(1,)],
                    use_prepare=False,
                )
            assert any(
                level == 40
                and "PyAsyncCursor::executemany: cursor description materialization failed"
                in message
                and "result_set_count=1" in message
                and "buffered_row_count=1" in message
                and "description_materialization_ms=" in message
                for level, message, _module in logger.events
            )
        finally:
            sys.modules["decimal"] = decimal_module
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_logs_interruption_during_execution(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            logger.events.clear()
            task = asyncio.ensure_future(
                cursor.executemany(
                    "WAITFOR DELAY '00:00:05'; SELECT ?",
                    [(1,)],
                    use_prepare=False,
                )
            )
            for _ in range(100):
                if any(
                    "PyAsyncCursor::executemany: executing parameter rows" in message
                    for _level, message, _module in logger.events
                ):
                    break
                await asyncio.sleep(0.01)
            else:
                pytest.fail("ExecuteMany did not start wire execution")

            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task

            probe = conn.cursor()
            for _ in range(100):
                try:
                    await probe.execute("SELECT 1", use_prepare=False)
                except RuntimeError as error:
                    if "busy" in str(error).lower():
                        await asyncio.sleep(0.01)
                        continue
                    assert "broken" in str(error).lower()
                    break
                else:
                    pytest.fail("Cancelled executemany left the connection reusable")
            else:
                pytest.fail("Cancelled executemany left the connection permanently busy")

            for _ in range(100):
                if any(
                    level == 30
                    and "PyAsyncCursor::executemany: interrupted" in message
                    and "phase=execution" in message
                    and "connection marked broken" in message
                    and module == "async_execute.rs"
                    for level, message, module in logger.events
                ):
                    break
                await asyncio.sleep(0.01)
            else:
                pytest.fail(f"Missing execution interruption warning: {logger.events}")
        finally:
            await conn.close()

    asyncio.run(run())
