# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Portable ExecuteMany parity coverage from the synchronous mssql-python suite."""

import asyncio
import datetime
import uuid
import warnings
from decimal import Decimal

import mssql_py_core
import pytest


async def connect(client_context, *, autocommit=True):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await mssql_py_core.PyAsyncConnection.connect(
            client_context, autocommit=autocommit
        )


async def fetchall(cursor):
    rows = []
    while (row := await cursor.fetchone()) is not None:
        rows.append(row)
    return rows


@pytest.mark.integration
def test_executemany_matches_repeated_execute_and_rowcount(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_compare (source varchar(10), value int)",
                use_prepare=False,
            )
            rows = [(1,), (2,), (3,)]
            await cursor.executemany(
                "INSERT INTO #many_compare VALUES ('many', ?)", rows
            )
            assert cursor.rowcount == len(rows)
            for row in rows:
                await cursor.execute(
                    "INSERT INTO #many_compare VALUES ('one', ?)",
                    row,
                    use_prepare=False,
                )
            await cursor.execute(
                "SELECT source, value FROM #many_compare ORDER BY value, source",
                use_prepare=False,
            )
            assert await fetchall(cursor) == [
                ("many", 1),
                ("one", 1),
                ("many", 2),
                ("one", 2),
                ("many", 3),
                ("one", 3),
            ]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_strings_empty_unicode_null_and_lengths(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_strings (id int, value nvarchar(max) NULL)",
                use_prepare=False,
            )
            values = [
                "",
                " ",
                "\t\r\n",
                "ascii",
                "東京",
                "😀",
                "a" * 8_001,
                "界" * 4_001,
                None,
            ]
            cursor.setinputsizes([(4, 0, 0), (-9, 4_001, 0)])
            await cursor.executemany(
                "INSERT INTO #many_strings VALUES (?, ?)",
                list(enumerate(values)),
            )
            assert cursor.rowcount == len(values)
            await cursor.execute(
                "SELECT value FROM #many_strings ORDER BY id", use_prepare=False
            )
            assert [row[0] for row in await fetchall(cursor)] == values
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_binary_empty_null_bytearray_and_max(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_binary (id int, value varbinary(max) NULL)",
                use_prepare=False,
            )
            values = [b"", b"\x00\x01\xff", bytearray(b"mutable"), b"x" * 8_001, None]
            cursor.setinputsizes([(4, 0, 0), (-4, 0, 0)])
            await cursor.executemany(
                "INSERT INTO #many_binary VALUES (?, ?)", list(enumerate(values))
            )
            await cursor.execute(
                "SELECT value FROM #many_binary ORDER BY id", use_prepare=False
            )
            assert [row[0] for row in await fetchall(cursor)] == [
                b"",
                b"\x00\x01\xff",
                b"mutable",
                b"x" * 8_001,
                None,
            ]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_integer_boolean_float_and_null_matrix(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_numbers (id int, integer_value bigint NULL, bit_value bit NULL, float_value float NULL)",
                use_prepare=False,
            )
            rows = [
                (1, -2_147_483_649, False, -1.5),
                (2, -32_769, True, 0.0),
                (3, -1, False, 1.25),
                (4, 0, True, 1.0e100),
                (5, 255, None, None),
                (6, 32_768, True, 3.141592653589793),
                (7, 2_147_483_648, False, -1.0e100),
                (8, None, None, None),
            ]
            cursor.setinputsizes([(4, 0, 0), (-5, 0, 0), (-7, 0, 0), (8, 0, 0)])
            await cursor.executemany(
                "INSERT INTO #many_numbers VALUES (?, ?, ?, ?)", rows
            )
            await cursor.execute(
                "SELECT id, integer_value, bit_value, float_value FROM #many_numbers ORDER BY id",
                use_prepare=False,
            )
            actual = await fetchall(cursor)
            assert [row[:3] for row in actual] == [row[:3] for row in rows]
            assert [row[3] for row in actual] == pytest.approx(
                [row[3] for row in rows], nan_ok=True
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_decimal_precision_scale_sign_and_null(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_decimal (id int, value decimal(38, 12) NULL)",
                use_prepare=False,
            )
            values = [
                Decimal("0"),
                Decimal("1.230000000000"),
                Decimal("-99999999999999999999999999.999999999999"),
                Decimal("0.000000000001"),
                None,
            ]
            cursor.setinputsizes([(4, 0, 0), (3, 38, 12)])
            await cursor.executemany(
                "INSERT INTO #many_decimal VALUES (?, ?)", list(enumerate(values))
            )
            await cursor.execute(
                "SELECT value FROM #many_decimal ORDER BY id", use_prepare=False
            )
            assert [row[0] for row in await fetchall(cursor)] == values
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("sql_type", [2, 3])
def test_executemany_setinputsizes_numeric_coercion(client_context, sql_type):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_numeric_hint (id int, value decimal(18, 4) NULL)",
                use_prepare=False,
            )
            cursor.setinputsizes([(4, 0, 0), (sql_type, 18, 4)])
            await cursor.executemany(
                "INSERT INTO #many_numeric_hint VALUES (?, ?)",
                [(1, Decimal("1.2500")), (2, 2), (3, 3.5), (4, None)],
            )
            await cursor.execute(
                "SELECT value FROM #many_numeric_hint ORDER BY id", use_prepare=False
            )
            assert [row[0] for row in await fetchall(cursor)] == [
                Decimal("1.2500"),
                Decimal("2.0000"),
                Decimal("3.5000"),
                None,
            ]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_temporal_values_and_extreme_offsets(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_temporal (id int, d date, t time(6), dt datetime2(6), dto datetimeoffset(6))",
                use_prepare=False,
            )
            rows = [
                (
                    1,
                    datetime.date(2020, 2, 29),
                    datetime.time(0, 0, 0),
                    datetime.datetime(2020, 2, 29, 1, 2, 3, 456789),
                    datetime.datetime(
                        2020,
                        2,
                        29,
                        1,
                        2,
                        3,
                        456789,
                        tzinfo=datetime.timezone(datetime.timedelta(hours=14)),
                    ),
                ),
                (
                    2,
                    datetime.date(9999, 12, 31),
                    datetime.time(23, 59, 59, 999999),
                    datetime.datetime(2000, 1, 1),
                    datetime.datetime(
                        2000,
                        1,
                        1,
                        tzinfo=datetime.timezone(datetime.timedelta(hours=-12)),
                    ),
                ),
            ]
            cursor.setinputsizes(
                [(4, 0, 0), (91, 0, 0), (92, 0, 6), (93, 0, 6), (-155, 0, 6)]
            )
            await cursor.executemany(
                "INSERT INTO #many_temporal VALUES (?, ?, ?, ?, ?)", rows
            )
            await cursor.execute(
                "SELECT id, d, t, dt, dto FROM #many_temporal ORDER BY id",
                use_prepare=False,
            )
            assert await fetchall(cursor) == rows
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_uuid_xml_money_and_geography_expression(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_special (id int, identifier uniqueidentifier NULL, payload xml NULL, amount money NULL, location geography NULL)",
                use_prepare=False,
            )
            first = uuid.UUID("12345678-1234-5678-1234-567812345678")
            second = uuid.UUID("87654321-4321-8765-4321-876543218765")
            rows = [
                (1, first, "<root><value>one</value></root>", Decimal("922337203685477.5807"), "POINT(-122.3 47.6)"),
                (2, second, "<root />", Decimal("-1.2500"), "POINT(0 0)"),
            ]
            cursor.setinputsizes(
                [(4, 0, 0), (-11, 0, 0), (-152, 0, 0), (60, 0, 0), (-9, 100, 0)]
            )
            await cursor.executemany(
                "INSERT INTO #many_special VALUES (?, ?, ?, ?, geography::STGeomFromText(?, 4326))",
                rows,
            )
            await cursor.execute(
                "SELECT id, identifier, CONVERT(nvarchar(max), payload), amount, location.STAsText() FROM #many_special ORDER BY id",
                use_prepare=False,
            )
            assert await fetchall(cursor) == [
                (1, first, "<root><value>one</value></root>", Decimal("922337203685477.5807"), "POINT (-122.3 47.6)"),
                (2, second, "<root/>", Decimal("-1.2500"), "POINT (0 0)"),
            ]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_all_null_columns_with_explicit_types(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_nulls (id int, binary_value varbinary(20), decimal_value decimal(18,4), guid_value uniqueidentifier)",
                use_prepare=False,
            )
            cursor.setinputsizes(
                [(4, 0, 0), (-3, 20, 0), (3, 18, 4), (-11, 0, 0)]
            )
            await cursor.executemany(
                "INSERT INTO #many_nulls VALUES (?, ?, ?, ?)",
                [(1, None, None, None), (2, None, None, None)],
            )
            await cursor.execute(
                "SELECT id, binary_value, decimal_value, guid_value FROM #many_nulls ORDER BY id",
                use_prepare=False,
            )
            assert await fetchall(cursor) == [
                (1, None, None, None),
                (2, None, None, None),
            ]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_pyformat_empty_name_and_extra_keys(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_pyformat (value int)", use_prepare=False
            )
            await cursor.executemany(
                "INSERT INTO #many_pyformat VALUES (%()s)",
                [{"": 1, "ignored": "x"}, {"": 2, "ignored": "y"}],
            )
            await cursor.execute(
                "SELECT value FROM #many_pyformat ORDER BY value", use_prepare=False
            )
            assert await fetchall(cursor) == [(1,), (2,)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_output_buffers_all_rows_in_order(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_output (id int, identifier uniqueidentifier)",
                use_prepare=False,
            )
            rows = [(1, uuid.uuid4()), (2, uuid.uuid4()), (3, uuid.uuid4())]
            await cursor.executemany(
                "INSERT INTO #many_output OUTPUT inserted.id, inserted.identifier VALUES (?, ?)",
                rows,
            )
            assert cursor.rowcount == -1
            assert await fetchall(cursor) == rows
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_accepts_generators_and_list_rows(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_iterable (id int, value varchar(20))",
                use_prepare=False,
            )

            def rows():
                for value in range(5):
                    yield [value, f"value-{value}"]

            await cursor.executemany(
                "INSERT INTO #many_iterable VALUES (?, ?)", rows()
            )
            await cursor.execute(
                "SELECT id, value FROM #many_iterable ORDER BY id", use_prepare=False
            )
            assert await fetchall(cursor) == [
                (value, f"value-{value}") for value in range(5)
            ]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_large_mixed_batch(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #many_stress (id int, number bigint, value nvarchar(40), payload varbinary(20), amount decimal(18,4) NULL)",
                use_prepare=False,
            )
            rows = [
                (
                    index,
                    index * 100_000,
                    f"row-{index}-東京",
                    index.to_bytes(4, "little"),
                    None if index % 7 == 0 else Decimal(index) / Decimal("10"),
                )
                for index in range(1_000)
            ]
            cursor.setinputsizes(
                [(4, 0, 0), (-5, 0, 0), (-9, 40, 0), (-3, 20, 0), (3, 18, 4)]
            )
            await cursor.executemany(
                "INSERT INTO #many_stress VALUES (?, ?, ?, ?, ?)", rows
            )
            assert cursor.rowcount == len(rows)
            await cursor.execute(
                "SELECT COUNT_BIG(*), SUM(number) FROM #many_stress",
                use_prepare=False,
            )
            assert await cursor.fetchone() == (
                len(rows),
                sum(row[1] for row in rows),
            )
        finally:
            await conn.close()

    asyncio.run(run())
