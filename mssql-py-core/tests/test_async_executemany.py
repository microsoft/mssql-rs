# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for asynchronous ExecuteMany command execution."""

import asyncio
import datetime
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


def test_executemany_returns_none_and_accepts_empty_input(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT 1", use_prepare=False)
            result = await cursor.executemany("SELECT ?", [])
            assert result is None
            assert cursor.rowcount == 0
            assert await cursor.execute("SET NOCOUNT ON", use_prepare=False) is cursor
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_rejects_scalar_parameter_rows(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match="parameter row 0"):
                await cursor.executemany("SELECT ?", [1])
            assert cursor.rowcount == -1
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_preflights_every_row(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match="2 parameters were supplied"):
                await cursor.executemany("SELECT ?", [(1,), (2, 3)])
            assert await cursor.execute("SET NOCOUNT ON", use_prepare=False) is cursor
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_rejects_mixed_parameter_row_styles(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match="row 1 uses a different parameter style"):
                await cursor.executemany("SELECT ?", [(1,), {"value": 2}])
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("operation", "rows"),
    [
        ("SELECT %(value)s", [{"value": 1}, (2,)]),
        ("SELECT ?", [[1], {"value": 2}]),
    ],
)
def test_executemany_rejects_mixed_parameter_row_styles_in_either_order(
    mock_client_context, operation, rows
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match="row 1 uses a different parameter style"):
                await cursor.executemany(operation, rows)
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_reports_missing_mapping_key_and_restores_state(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(KeyError, match="value"):
                await cursor.executemany(
                    "SELECT %(value)s", [{"value": 1}, {"other": 2}]
                )
            assert cursor.rowcount == -1
            assert await cursor.execute("SET NOCOUNT ON", use_prepare=False) is cursor
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_propagates_generator_failure_and_restores_state(
    mock_client_context,
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()

            def rows():
                yield (1,)
                raise ValueError("row source failed")

            with pytest.raises(ValueError, match="row source failed"):
                await cursor.executemany("SELECT ?", rows())
            assert cursor.rowcount == -1
            assert await cursor.execute("SET NOCOUNT ON", use_prepare=False) is cursor
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_defers_parameter_iteration_until_await(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            iterated = False

            def rows():
                nonlocal iterated
                iterated = True
                yield 1

            awaitable = cursor.executemany("SELECT ?", rows())
            assert not iterated
            with pytest.raises(TypeError, match="parameter row 0"):
                await awaitable
            assert iterated
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_inserts_rows_and_aggregates_rowcount(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #executemany_values (id int NOT NULL, value nvarchar(40) NOT NULL)",
                use_prepare=False,
            )
            result = await cursor.executemany(
                "INSERT INTO #executemany_values (id, value) VALUES (?, ?)",
                [(1, "one"), (2, "東京"), (300, "three")],
            )
            assert result is None
            assert cursor.rowcount == 3
            await cursor.execute(
                "IF (SELECT COUNT(*) FROM #executemany_values) <> 3 "
                "THROW 50000, 'Unexpected ExecuteMany row count', 1",
                use_prepare=False,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_binds_named_rows_and_ignores_extra_keys(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #executemany_named (id int NOT NULL, value int NOT NULL)",
                use_prepare=False,
            )
            await cursor.executemany(
                "INSERT INTO #executemany_named (id, value) "
                "VALUES (%(id)s, %(id)s)",
                [
                    {"id": 1, "ignored": "a"},
                    {"id": 2, "ignored": "b"},
                ],
            )
            assert cursor.rowcount == 2
            await cursor.execute(
                "IF EXISTS (SELECT 1 FROM #executemany_named WHERE id <> value) "
                "THROW 50000, 'Unexpected named values', 1",
                use_prepare=False,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_preserves_typed_null_metadata(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #executemany_typed_nulls (amount decimal(12,3) NULL, observed_at datetimeoffset(4) NULL)",
                use_prepare=False,
            )
            cursor.setinputsizes([(3, 12, 3), (-155, 0, 4)])
            await cursor.executemany(
                "INSERT INTO #executemany_typed_nulls (amount, observed_at) VALUES (?, ?)",
                [
                    (None, None),
                    (
                        Decimal("123.456"),
                        datetime.datetime(
                            2026,
                            8,
                            31,
                            12,
                            30,
                            45,
                            123400,
                            tzinfo=datetime.timezone(datetime.timedelta(hours=5, minutes=30)),
                        ),
                    ),
                ],
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_buffers_rows_from_result_producing_operations(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #executemany_output (id int NOT NULL)",
                use_prepare=False,
            )
            await cursor.executemany(
                "INSERT INTO #executemany_output OUTPUT inserted.id VALUES (?)",
                [(1,), (2,)],
            )
            assert cursor.rowcount == -1
            assert await cursor.fetchone() == (1,)
            assert await cursor.fetchone() == (2,)
            assert await cursor.fetchone() is None
            assert await cursor.execute("SET NOCOUNT ON", use_prepare=False) is cursor
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_unread_buffered_rows_can_be_replaced_or_closed(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #executemany_unread_output (id int NOT NULL)",
                use_prepare=False,
            )
            await cursor.executemany(
                "INSERT INTO #executemany_unread_output OUTPUT inserted.id VALUES (?)",
                [(1,), (2,)],
            )
            assert await cursor.fetchone() == (1,)
            assert await cursor.execute("SELECT 3", use_prepare=False) is cursor
            assert await cursor.fetchone() == (3,)
            assert await cursor.fetchone() is None

            await cursor.executemany(
                "INSERT INTO #executemany_unread_output OUTPUT inserted.id VALUES (?)",
                [(4,), (5,)],
            )
            await cursor.close()
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_failure_stops_later_rows_without_automatic_rollback(client_context):
    async def run():
        conn = await connect(client_context, autocommit=False)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #executemany_failure (id int PRIMARY KEY)",
                use_prepare=False,
            )
            with pytest.raises(RuntimeError, match="Query execution failed"):
                await cursor.executemany(
                    "INSERT INTO #executemany_failure (id) VALUES (?)",
                    [(1,), (1,), (2,)],
                )
            assert cursor.rowcount == -1
            await conn.rollback()
        finally:
            await conn.close()

    asyncio.run(run())
