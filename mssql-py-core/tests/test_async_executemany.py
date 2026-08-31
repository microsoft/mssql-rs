# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for asynchronous ExecuteMany command execution."""

import asyncio
import datetime
import threading
import time
import warnings
from decimal import Decimal

import mssql_py_core
import pytest


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


def test_executemany_traces_preflight_wire_and_completion(mock_client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(mock_client_context, logger)
        try:
            cursor = conn.cursor()
            logger.events.clear()
            await cursor.executemany("SELECT ?", [])

            messages = [message for _, message, _ in logger.events]
            assert any(
                message.startswith("PyAsyncCursor::executemany: preflight started")
                for message in messages
            )
            assert any(
                message.startswith("PyAsyncCursor::executemany: preflight completed")
                and "row_count=0" in message
                for message in messages
            )
            assert any(
                message.startswith("PyAsyncCursor::executemany: wire execution started")
                for message in messages
            )
            assert any(
                message.startswith("PyAsyncCursor::executemany: completed successfully")
                for message in messages
            )
            assert all(module == "async_execute.rs" for _, _, module in logger.events)
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_traces_preflight_failure_without_parameter_values(
    mock_client_context,
):
    async def run():
        logger = RecordingLogger()
        conn = await connect(mock_client_context, logger)
        secret = "value-that-must-not-be-logged"
        try:
            cursor = conn.cursor()
            logger.events.clear()
            with pytest.raises(TypeError, match="different parameter style"):
                await cursor.executemany("SELECT ?", [(secret,), {"value": secret}])

            messages = [message for _, message, _ in logger.events]
            assert any(
                message.startswith("PyAsyncCursor::executemany: preflight failed")
                for message in messages
            )
            assert all(secret not in message for message in messages)
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


def test_cancelling_executemany_during_preflight_restores_session(mock_client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(mock_client_context, logger)
        entered = threading.Event()
        release = threading.Event()
        try:
            cursor = conn.cursor()
            logger.events.clear()

            def rows():
                yield (1,)
                entered.set()
                while not release.wait(0.01):
                    pass
                yield (2,)

            task = asyncio.ensure_future(cursor.executemany("SELECT ?", rows()))
            while not entered.is_set():
                await asyncio.sleep(0)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            release.set()

            for _ in range(100):
                try:
                    await cursor.execute("SET NOCOUNT ON", use_prepare=False)
                except RuntimeError as error:
                    if "busy" in str(error).lower():
                        await asyncio.sleep(0.01)
                        continue
                    raise
                else:
                    break
            else:
                pytest.fail("Cancelled ExecuteMany preflight left the connection busy")

            assert any(
                message == "PyAsyncCursor::executemany: preflight interrupted"
                for _, message, _ in logger.events
            )
        finally:
            release.set()
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
            await cursor.execute(
                "SELECT amount, observed_at, "
                "SQL_VARIANT_PROPERTY(CAST(amount AS sql_variant), 'BaseType'), "
                "SQL_VARIANT_PROPERTY(CAST(amount AS sql_variant), 'Precision'), "
                "SQL_VARIANT_PROPERTY(CAST(amount AS sql_variant), 'Scale') "
                "FROM #executemany_typed_nulls ORDER BY amount",
                use_prepare=False,
            )
            assert await cursor.fetchone() == (None, None, None, None, None)
            assert await cursor.fetchone() == (
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
                "decimal",
                12,
                3,
            )
            assert await cursor.fetchone() is None
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
def test_executemany_buffers_multiple_output_result_sets_per_row(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.executemany(
                "DECLARE @values table (id int); "
                "INSERT INTO @values OUTPUT inserted.id SELECT ? WHERE ? = 1; "
                "INSERT INTO @values OUTPUT inserted.id VALUES (?)",
                [(1, 1, 10), (2, 0, 20)],
            )
            assert await cursor.fetchone() == (1,)
            assert await cursor.fetchone() == (10,)
            assert await cursor.fetchone() == (20,)
            assert await cursor.fetchone() is None
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
            await cursor.execute(
                "SELECT id FROM #executemany_failure ORDER BY id", use_prepare=False
            )
            assert await cursor.fetchone() == (1,)
            assert await cursor.fetchone() is None
            await conn.rollback()
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_traces_wire_failure_with_correct_operation(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            logger.events.clear()
            with pytest.raises(RuntimeError, match="expected failure"):
                await cursor.executemany(
                    "THROW 50000, 'expected failure', 1; SELECT ?", [(1,)]
                )

            messages = [message for _, message, _ in logger.events]
            assert any(
                message.startswith("PyAsyncCursor::executemany: failed")
                and "row_index=0" in message
                for message in messages
            )
            assert not any(
                message.startswith("PyAsyncCursor::execute: failed")
                for message in messages
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_autocommit_partial_failure_commits_prior_rows(client_context):
    async def run():
        conn = await connect(client_context, autocommit=True)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "CREATE TABLE #executemany_autocommit_failure (id int PRIMARY KEY)",
                use_prepare=False,
            )
            with pytest.raises(RuntimeError, match="Query execution failed"):
                await cursor.executemany(
                    "INSERT INTO #executemany_autocommit_failure VALUES (?)",
                    [(1,), (1,), (2,)],
                )
            await conn.rollback()
            await cursor.execute(
                "SELECT id FROM #executemany_autocommit_failure ORDER BY id",
                use_prepare=False,
            )
            assert await cursor.fetchone() == (1,)
            assert await cursor.fetchone() is None
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_timeout_applies_to_entire_batch(client_context):
    async def run():
        conn = await connect(client_context)
        conn.timeout = 1
        try:
            cursor = conn.cursor()
            started = time.monotonic()
            with pytest.raises(RuntimeError, match="(?i)timed out|timeout"):
                await cursor.executemany(
                    "WAITFOR DELAY '00:00:00.700'; SELECT ?", [(1,), (2,)]
                )
            assert time.monotonic() - started < 1.8
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_cancelling_executemany_during_wire_execution_breaks_session(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            logger.events.clear()
            task = asyncio.ensure_future(
                cursor.executemany(
                    "WAITFOR DELAY '00:00:05'; SELECT ?", [(1,), (2,)]
                )
            )
            await asyncio.sleep(0.1)
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
                    pytest.fail("Cancelled ExecuteMany left the connection reusable")
            else:
                pytest.fail("Cancelled ExecuteMany left the connection permanently busy")

            assert any(
                level == 30
                and message
                == "PyAsyncCursor::executemany: wire execution interrupted; connection marked broken"
                for level, message, _ in logger.events
            )
        finally:
            await conn.close()

    asyncio.run(run())


def test_executemany_empty_and_preflight_failure_preserve_input_sizes(
    mock_client_context,
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-151, 0, 0)])
            await cursor.executemany("SELECT ?", [])
            with pytest.raises(TypeError, match="require a server UDT type name"):
                await cursor.executemany("SELECT ?", [(b"serialized-udt",)])

            cursor.setinputsizes([(4, 0, 0)])
            with pytest.raises(TypeError, match="row 1 uses a different parameter style"):
                await cursor.executemany("SELECT ?", [(1,), {"value": 2}])
            with pytest.raises(TypeError, match="integer"):
                await cursor.executemany("SELECT ?", [("not-an-integer",)])
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_executemany_input_sizes_survive_wire_failure_and_clear_after_success(
    client_context,
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-9, 20, 0)])
            with pytest.raises(RuntimeError, match="expected failure"):
                await cursor.executemany(
                    "THROW 50000, 'expected failure', 1; SELECT ?", [("ascii",)]
                )
            await cursor.executemany(
                "IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> 'nvarchar' "
                "THROW 50000, 'Input size was consumed by failed ExecuteMany', 1",
                [("ascii",)],
            )

            cursor.setinputsizes([(12, 20, 0)])
            await cursor.executemany(
                "IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> 'varchar' "
                "THROW 50000, 'Input size was not applied', 1",
                [("ascii",)],
            )
            await cursor.executemany(
                "IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> 'nvarchar' "
                "THROW 50000, 'Input size survived successful ExecuteMany', 1",
                [("東京",)],
            )
        finally:
            await conn.close()

    asyncio.run(run())
