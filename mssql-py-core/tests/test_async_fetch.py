# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for asynchronous cursor row fetching."""

import asyncio
import datetime
import importlib.abc
import importlib.machinery
import sys
import threading
import uuid
import warnings
from decimal import Decimal

import mssql_py_core
import pytest

# Pytest invokes the outer synchronous test functions. Each nested `run`
# coroutine contains the asynchronous scenario, and `asyncio.run(run())`
# creates an event loop, drives that coroutine to completion, and closes the
# loop before the test returns.


class RecordingLogger:
    def __init__(self):
        self.events = []

    def py_core_log(self, level, message, module_name, _line):
        self.events.append((level, message, module_name))


class BlockingDecimalFinder(importlib.abc.MetaPathFinder):
    """Pause decimal imports in a worker thread at a deterministic test point.

    Row and description materialization import ``decimal`` outside the asyncio
    event-loop thread. The two threading events let a test wait until that
    import starts and release it after cancelling the Python-facing future.
    """

    def __init__(self, entered, release):
        self.entered = entered
        self.release = release

    def find_spec(self, fullname, path, target=None):
        if fullname != "decimal":
            return None
        # Tell the event-loop thread that materialization reached the intended
        # cancellation point, then block only this worker thread.
        self.entered.set()
        self.release.wait()
        return importlib.machinery.PathFinder.find_spec(fullname, path, target)


async def connect(client_context, python_logger=None):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await mssql_py_core.PyAsyncConnection.connect(
            client_context, python_logger, autocommit=True
        )


async def execute_after_cancellation_settles(cursor, operation):
    """Run a probe after the detached ATTENTION drain releases the session.

    Cancelling the Python future ends the caller's wait immediately, but the
    Rust task keeps ownership while it sends ATTENTION and drains to DONE_ATTN.
    During that short interval another execute reports ``busy``. Sleeping
    yields control to the event loop so the detached task can make progress;
    the deadline turns a lost ownership release into a clear test failure.
    """
    deadline = asyncio.get_running_loop().time() + 6
    while True:
        try:
            await cursor.execute(operation, use_prepare=False)
            return
        except RuntimeError as error:
            if "busy" not in str(error).lower():
                raise
            if asyncio.get_running_loop().time() >= deadline:
                pytest.fail("Cancelled fetch left the connection permanently busy")
            await asyncio.sleep(0.01)


def test_module_exposes_fetchone():
    assert hasattr(mssql_py_core.PyAsyncCursor, "fetchone")
    assert hasattr(mssql_py_core.PyAsyncCursor, "fetchmany")
    assert hasattr(mssql_py_core.PyAsyncCursor, "fetchall")
    assert hasattr(mssql_py_core.PyAsyncCursor, "nextset")


def test_module_exposes_dbapi_exception_hierarchy():
    assert issubclass(mssql_py_core.Warning, Exception)
    assert issubclass(mssql_py_core.InterfaceError, mssql_py_core.Error)
    assert issubclass(mssql_py_core.DatabaseError, mssql_py_core.Error)
    assert issubclass(mssql_py_core.DataError, mssql_py_core.DatabaseError)
    assert issubclass(mssql_py_core.OperationalError, mssql_py_core.DatabaseError)
    assert issubclass(mssql_py_core.IntegrityError, mssql_py_core.DatabaseError)
    assert issubclass(mssql_py_core.InternalError, mssql_py_core.DatabaseError)
    assert issubclass(mssql_py_core.ProgrammingError, mssql_py_core.DatabaseError)
    assert issubclass(mssql_py_core.NotSupportedError, mssql_py_core.DatabaseError)


def test_fetchone_without_result_set_raises(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(mssql_py_core.Error) as exc_info:
                await cursor.fetchone()
            assert isinstance(exc_info.value, mssql_py_core.ProgrammingError)
            assert "No active result set" in str(exc_info.value)
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchmany(1)
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchall()
            with pytest.raises(TypeError):
                cursor.fetchall(1)
            with pytest.raises(TypeError):
                cursor.fetchall(size=1)
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.nextset()
            with pytest.raises(TypeError):
                cursor.nextset(1)
        finally:
            await conn.close()

    asyncio.run(run())


def test_fetchone_after_cursor_close_raises(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            await cursor.close()
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await cursor.fetchone()
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await cursor.fetchmany(0)
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await cursor.fetchall()
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await cursor.nextset()
        finally:
            await conn.close()

    asyncio.run(run())


def test_fetchmany_argument_and_arraysize_contract(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            assert cursor.arraysize == 1
            cursor.arraysize = 3
            assert cursor.arraysize == 3

            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchmany(0)
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchmany(-1)
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchmany(False)
            cursor.arraysize = -2
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchmany()

            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchmany(True)
            with pytest.raises(TypeError):
                cursor.fetchmany(1.5)
            with pytest.raises(TypeError):
                cursor.fetchmany(1, 2)
            with pytest.raises(TypeError):
                cursor.fetchmany(unknown=1)
            with pytest.raises(TypeError):
                cursor.arraysize = 1.5
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_takes_no_arguments_and_distinguishes_null_from_exhaustion(
    client_context,
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError):
                cursor.fetchone(None)
            with pytest.raises(TypeError):
                cursor.fetchone(size=1)

            await cursor.execute("SELECT CAST(NULL AS int)", use_prepare=False)
            assert await cursor.fetchone() == (None,)
            assert await cursor.fetchone() is None
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_returns_rows_and_releases_final_exhaustion(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute(
                "SELECT value FROM (VALUES (1), (2)) AS rows(value) ORDER BY value",
                use_prepare=False,
            )

            assert await owner.fetchone() == (1,)
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await other.execute("SELECT 3", use_prepare=False)
            assert await owner.fetchone() == (2,)
            assert await owner.fetchone() is None
            assert await owner.fetchone() is None

            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_logs_exhaustion(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT 1", use_prepare=False)
            logger.events.clear()

            assert await cursor.fetchone() == (1,)
            assert await cursor.fetchone() is None

            assert (
                20,
                "PyAsyncCursor::fetchone: result set exhausted",
                "async_fetch.rs",
            ) in logger.events

            await cursor.execute("SELECT 1", use_prepare=False)
            logger.events.clear()
            assert await cursor.fetchmany(2) == [(1,)]
            assert (
                10,
                "PyAsyncCursor::fetchmany: started; requested=2",
                "async_fetch.rs",
            ) in logger.events
            assert (
                20,
                "PyAsyncCursor::fetchmany: result set exhausted",
                "async_fetch.rs",
            ) in logger.events
            assert any(
                level == 10
                and "PyAsyncCursor::fetchmany: completed; requested=2; returned=1; exhausted=true; elapsed_ms="
                in message
                for level, message, _module in logger.events
            )

            await cursor.execute("SELECT 1 UNION ALL SELECT 2", use_prepare=False)
            logger.events.clear()
            assert await cursor.fetchall() == [(1,), (2,)]
            assert (
                10,
                "PyAsyncCursor::fetchall: started",
                "async_fetch.rs",
            ) in logger.events
            assert (
                20,
                "PyAsyncCursor::fetchall: result set exhausted",
                "async_fetch.rs",
            ) in logger.events
            assert any(
                level == 10
                and "PyAsyncCursor::fetchall: completed; returned=2; exhausted=true; elapsed_ms="
                in message
                for level, message, _module in logger.events
            )

        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchmany_uses_arraysize_and_interleaves_without_skipping(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.arraysize = 2
            await cursor.execute(
                "SELECT value FROM (VALUES (1), (2), (3), (4), (5)) rows(value) ORDER BY value",
                use_prepare=False,
            )
            description = cursor.description

            assert await cursor.fetchone() == (1,)
            assert await cursor.fetchmany() == [(2,), (3,)]
            assert await cursor.fetchmany(None) == [(4,), (5,)]
            assert cursor.description == description
            assert await cursor.fetchmany(size=2) == []
            assert await cursor.fetchmany() == []
            assert await cursor.fetchone() is None
            assert cursor.description == description
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_large_fetchmany_keeps_event_loop_responsive(client_context):
    async def run():
        conn = await connect(client_context)
        heartbeat_ticks = 0
        stop_heartbeat = False

        async def heartbeat():
            nonlocal heartbeat_ticks
            while not stop_heartbeat:
                heartbeat_ticks += 1
                await asyncio.sleep(0)

        try:
            cursor = conn.cursor()
            cursor.arraysize = 4096
            await cursor.execute(
                """
                WITH numbers AS (
                    SELECT 0 AS value
                    UNION ALL
                    SELECT value + 1 FROM numbers WHERE value < 4095
                )
                SELECT value FROM numbers ORDER BY value
                OPTION (MAXRECURSION 4095)
                """,
                use_prepare=False,
            )
            heartbeat_task = asyncio.create_task(heartbeat())
            await asyncio.sleep(0)
            ticks_before_fetch = heartbeat_ticks

            rows = await cursor.fetchmany()
            ticks_during_fetch = heartbeat_ticks - ticks_before_fetch

            assert rows == [(value,) for value in range(4096)]
            assert ticks_during_fetch > 0
        finally:
            stop_heartbeat = True
            if "heartbeat_task" in locals():
                await heartbeat_task
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchmany_nonpositive_size_does_not_advance_and_partial_batch_releases_session(
    client_context,
):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute(
                "SELECT value FROM (VALUES (1), (2), (3)) rows(value) ORDER BY value",
                use_prepare=False,
            )

            assert await owner.fetchmany(0) == []
            assert await owner.fetchmany(-10) == []
            assert await owner.fetchmany(2) == [(1,), (2,)]
            assert await owner.fetchmany(2) == [(3,)]
            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchmany_stops_at_current_result_set(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute("SELECT 1; SELECT 2", use_prepare=False)

            assert await owner.fetchmany(10) == [(1,)]
            assert await owner.fetchmany(10) == []
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await other.execute("SELECT 3", use_prepare=False)

            await owner.close()
            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchall_returns_remaining_rows_and_releases_finished_batch(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute(
                "SELECT value FROM (VALUES (1), (2), (3), (4)) rows(value) ORDER BY value",
                use_prepare=False,
            )
            description = owner.description

            assert await owner.fetchone() == (1,)
            assert await owner.fetchmany(1) == [(2,)]
            assert await owner.fetchall() == [(3,), (4,)]
            assert await owner.fetchall() == []
            assert owner.description == description

            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchall_empty_result_and_current_result_set_boundary(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()

            await owner.execute("SELECT 1 WHERE 1 = 0", use_prepare=False)
            assert await owner.fetchall() == []
            await other.execute("SET NOCOUNT ON", use_prepare=False)

            await owner.execute("SELECT 1; SELECT 2", use_prepare=False)
            assert await owner.fetchall() == [(1,)]
            assert await owner.fetchall() == []
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await other.execute("SELECT 3", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchall_keeps_event_loop_responsive(client_context):
    async def run():
        conn = await connect(client_context)
        heartbeat_ticks = 0
        stop_heartbeat = False

        async def heartbeat():
            nonlocal heartbeat_ticks
            while not stop_heartbeat:
                heartbeat_ticks += 1
                await asyncio.sleep(0)

        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                WITH numbers AS (
                    SELECT 0 AS value
                    UNION ALL
                    SELECT value + 1 FROM numbers WHERE value < 4095
                )
                SELECT value FROM numbers ORDER BY value
                OPTION (MAXRECURSION 4095)
                """,
                use_prepare=False,
            )
            heartbeat_task = asyncio.create_task(heartbeat())
            await asyncio.sleep(0)
            ticks_before_fetch = heartbeat_ticks

            rows = await cursor.fetchall()
            ticks_during_fetch = heartbeat_ticks - ticks_before_fetch

            assert rows == [(value,) for value in range(4096)]
            assert ticks_during_fetch > 0
        finally:
            stop_heartbeat = True
            if "heartbeat_task" in locals():
                await heartbeat_task
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_nextset_drains_rows_and_updates_description(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT 1 AS first_value UNION ALL SELECT 2; "
                "SELECT N'x' AS second_value; SELECT 3 AS third_value",
                use_prepare=False,
            )
            assert cursor.description[0][:2] == ("first_value", int)
            assert await cursor.fetchone() == (1,)

            assert await cursor.nextset() is True
            assert cursor.description[0][:2] == ("second_value", str)
            assert await cursor.fetchall() == [("x",)]

            assert await cursor.nextset() is True
            assert cursor.description[0][:2] == ("third_value", int)
            assert await cursor.fetchone() == (3,)

            assert await cursor.nextset() is False
            assert cursor.description is None
            assert await cursor.nextset() is False
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetch_interleaving_across_result_transitions_and_ownership(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute(
                """
                DECLARE @sink TABLE (value int);
                SELECT value FROM (VALUES (1), (2), (3), (4), (5), (6)) rows(value)
                    ORDER BY value;
                INSERT INTO @sink VALUES (1);
                SELECT value FROM (VALUES (10), (11), (12), (13)) rows(value)
                    ORDER BY value;
                SELECT CAST(1 AS int) AS empty_value WHERE 1 = 0;
                SELECT value FROM (VALUES (20), (21)) rows(value) ORDER BY value;
                """,
                use_prepare=False,
            )

            assert await owner.fetchone() == (1,)
            assert await owner.fetchmany(2) == [(2,), (3,)]
            assert await owner.fetchall() == [(4,), (5,), (6,)]
            assert await owner.fetchall() == []
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await other.execute("SELECT 99", use_prepare=False)

            assert await owner.nextset() is True
            assert owner.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await owner.fetchall()

            assert await owner.nextset() is True
            assert await owner.fetchone() == (10,)
            assert await owner.fetchmany(2) == [(11,), (12,)]
            assert await owner.fetchall() == [(13,)]

            assert await owner.nextset() is True
            assert owner.description[0][0] == "empty_value"
            assert await owner.fetchone() is None
            assert await owner.fetchmany(2) == []
            assert await owner.fetchall() == []

            assert await owner.nextset() is True
            assert await owner.fetchmany(1) == [(20,)]
            assert await owner.fetchall() == [(21,)]
            assert await owner.nextset() is False
            assert await owner.nextset() is False
            await other.execute("SET NOCOUNT ON", use_prepare=False)

            await owner.execute("SELECT 1; SELECT 2", use_prepare=False)
            await owner.close()
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await owner.nextset()
            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_execute_preserves_leading_statement_without_rows(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute(
                "DECLARE @rows TABLE (value int); "
                "INSERT INTO @rows VALUES (1); "
                "SELECT value AS selected_value FROM @rows",
                use_prepare=use_prepare,
            )

            assert owner.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await owner.fetchone()
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await other.execute("SELECT 2", use_prepare=False)

            assert await owner.nextset() is True
            assert owner.description[0][:2] == ("selected_value", int)
            assert await owner.fetchall() == [(1,)]
            assert await owner.nextset() is False
            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_nextset_reports_batch_end_after_terminal_no_rows(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            other = conn.cursor()

            await cursor.execute(
                "CREATE TABLE #async_terminal_no_rows (value int)",
                use_prepare=False,
            )
            assert cursor.description is None
            await other.execute("SELECT 0", use_prepare=False)
            assert await other.fetchall() == [(0,)]
            assert await cursor.nextset() is False
            assert await cursor.nextset() is False

            await cursor.execute(
                "INSERT INTO #async_terminal_no_rows VALUES (?)",
                1,
                use_prepare=use_prepare,
            )
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

            await cursor.execute(
                "SELECT 2 AS selected_value; "
                "INSERT INTO #async_terminal_no_rows VALUES (2)",
                use_prepare=False,
            )
            assert await cursor.fetchall() == [(2,)]
            assert await cursor.nextset() is True
            assert cursor.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchone()
            assert await cursor.nextset() is False
            assert await cursor.nextset() is False

            await cursor.execute("PRINT 'terminal information'", use_prepare=False)
            assert cursor.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchone()
            assert await cursor.nextset() is False

            await other.execute(
                "SELECT value FROM #async_terminal_no_rows ORDER BY value",
                use_prepare=False,
            )
            assert await other.fetchall() == [(1,), (2,)]

            await cursor.execute("SET NOCOUNT ON", use_prepare=False)
            assert cursor.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchone()
            await other.execute("SELECT 3", use_prepare=False)
            assert await other.fetchall() == [(3,)]
            assert await cursor.nextset() is False
            assert await cursor.nextset() is False
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_nextset_asyncio_timeout_resynchronizes_connection(client_context):
    """A timed-out nextset drains through DONE_ATTN and leaves a reusable session.

    1. Execute a batch whose first result is deliberately expensive to drain.
    2. Give ``nextset`` a 10 ms asyncio deadline; ``wait_for`` cancels its
       awaitable when that deadline expires.
    3. Let the detached Rust task finish ATTENTION settlement.
    4. Execute and fetch a new query to prove the same session is synchronized.
    """

    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT TOP (10000000) REPLICATE(CAST('x' AS varchar(100)), 100) "
                "FROM sys.all_objects AS first_rows "
                "CROSS JOIN sys.all_objects AS second_rows; SELECT 2",
                use_prepare=False,
            )

            with pytest.raises(asyncio.TimeoutError):
                # wait_for requests cancellation of nextset when 10 ms elapse.
                await asyncio.wait_for(cursor.nextset(), timeout=0.01)

            probe = conn.cursor()
            await execute_after_cancellation_settles(probe, "SELECT 3")
            assert await probe.fetchone() == (3,)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_cancelled_fetchone_resynchronizes_connection(client_context):
    """Explicit cancellation does not abandon an in-flight TDS row parser.

    1. Start fetching a large value so the row parser remains in progress.
    2. Schedule the PyO3 Future and yield to let its read begin.
    3. Cancel and await it so Python observes ``CancelledError``.
    4. Wait for background ATTENTION settlement, then reuse the connection.
    """

    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT REPLICATE(CAST('x' AS varchar(max)), 32 * 1024 * 1024)",
                use_prepare=False,
            )

            # PyO3 returns an asyncio Future rather than a coroutine.
            # ensure_future accepts either and gives the test a cancellable handle.
            fetch = asyncio.ensure_future(cursor.fetchone())
            # Suspend this coroutine so the event loop can start the fetch.
            await asyncio.sleep(0.01)
            # cancel() requests cancellation; awaiting delivers CancelledError.
            fetch.cancel()
            with pytest.raises(asyncio.CancelledError):
                await fetch

            probe = conn.cursor()
            await execute_after_cancellation_settles(probe, "SELECT 1")
            assert await probe.fetchone() == (1,)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_nextset_surfaces_statement_without_rows(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "DECLARE @rows TABLE (value int); "
                "SELECT 1 AS first_value; "
                "INSERT INTO @rows VALUES (1); "
                "SELECT 2 AS second_value",
                use_prepare=False,
            )

            assert await cursor.nextset() is True
            assert cursor.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchone()

            assert await cursor.nextset() is True
            assert cursor.description[0][:2] == ("second_value", int)
            assert await cursor.fetchall() == [(2,)]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetch_preserves_sql_server_error_type_and_diagnostics(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT 1 / divisor AS value "
                "FROM (VALUES (1), (0)) AS source(divisor) "
                "ORDER BY divisor DESC",
                use_prepare=False,
            )

            with pytest.raises(mssql_py_core.DatabaseError) as exc_info:
                await cursor.fetchall()

            assert "PyAsyncCursor.fetchall failed while reading rows" in str(
                exc_info.value
            )
            assert exc_info.value.sql_errors
            assert exc_info.value.sql_errors[0]["number"] == 8134
            assert "divide by zero" in exc_info.value.sql_errors[0]["message"].lower()

            await conn.cursor().execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_nextset_preserves_sql_server_error_type_and_diagnostics(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT 1; "
                "RAISERROR('nextset information', 10, 2); "
                "RAISERROR('nextset failure', 16, 1)",
                use_prepare=False,
            )

            assert await cursor.nextset() is True
            assert cursor.description is None

            with pytest.raises(mssql_py_core.DatabaseError) as exc_info:
                await cursor.nextset()

            assert "PyAsyncCursor.nextset failed while advancing results" in str(
                exc_info.value
            )
            assert exc_info.value.sql_errors
            assert exc_info.value.sql_errors[0]["number"] == 50000
            assert exc_info.value.sql_errors[0]["class"] == 16
            assert exc_info.value.sql_errors[0]["state"] == 1
            assert len(exc_info.value.info_messages) == 1
            assert exc_info.value.info_messages[0]["number"] == 50000
            assert exc_info.value.info_messages[0]["class"] == 0
            assert exc_info.value.info_messages[0]["state"] == 2
            assert (
                exc_info.value.info_messages[0]["message"]
                == "nextset information"
            )

            await conn.cursor().execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_nextset_logs_result_transitions(client_context):
    async def run():
        logger = RecordingLogger()
        conn = await connect(client_context, logger)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT 1; SELECT N'x' AS value", use_prepare=False)
            logger.events.clear()

            assert await cursor.nextset() is True
            assert (
                10,
                "PyAsyncCursor::nextset: started",
                "async_fetch.rs",
            ) in logger.events
            assert any(
                level == 10
                and "PyAsyncCursor::nextset: completed; has_result=true; has_rows=true; column_count=1; elapsed_ms="
                in message
                for level, message, _module in logger.events
            )

            assert await cursor.nextset() is False
            assert (
                20,
                "PyAsyncCursor::nextset: batch exhausted",
                "async_fetch.rs",
            ) in logger.events

            await cursor.execute(
                "SELECT 1; SELECT CAST(1 AS decimal(10, 2)) AS value; "
                "SELECT 3 AS recovered_value",
                use_prepare=False,
            )
            decimal_module = sys.modules["decimal"]
            sys.modules["decimal"] = None
            logger.events.clear()
            try:
                with pytest.raises(
                    mssql_py_core.InternalError,
                    match="Advanced result set but cursor description materialization failed",
                ):
                    await cursor.nextset()
            finally:
                sys.modules["decimal"] = decimal_module
            assert cursor.description is None
            with pytest.raises(
                mssql_py_core.ProgrammingError, match="No active result set"
            ):
                await cursor.fetchone()
            # The failed conversion leaves this row set owned but unread;
            # nextset drains it before exposing the following result.
            assert await cursor.nextset() is True
            assert cursor.description[0][:2] == ("recovered_value", int)
            assert await cursor.fetchone() == (3,)
            assert await cursor.nextset() is False
            assert any(
                level == 40
                and "PyAsyncCursor::nextset: description materialization failed; "
                "column_count=1; elapsed_ms=" in message
                and "; read_ms=" in message
                and "; materialization_ms=" in message
                for level, message, _module in logger.events
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_nextset_cancellation_during_description_materialization_does_not_publish_rows(
    client_context,
):
    """Cancellation during description conversion cannot publish stale metadata.

    1. Force decimal metadata conversion through a deliberately blocked worker.
    2. Schedule ``nextset`` and yield until that worker reaches the block.
    3. Cancel the Python Future, then release the worker.
    4. Verify cancellation wins the publication race: description stays empty.
    5. Wait for ATTENTION settlement and prove the connection remains reusable.
    """

    async def run():
        conn = await connect(client_context)
        entered = threading.Event()
        release = threading.Event()
        finder = BlockingDecimalFinder(entered, release)
        # Removing the cached module guarantees materialization invokes finder.
        decimal_module = sys.modules.pop("decimal")
        sys.meta_path.insert(0, finder)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT 1; SELECT CAST(2 AS decimal(10, 2)) AS value",
                use_prepare=False,
            )
            # ensure_future schedules the PyO3 Future on the current event loop.
            task = asyncio.ensure_future(cursor.nextset())
            for _ in range(200):
                if entered.is_set():
                    break
                # Poll the cross-thread signal without blocking the event loop.
                await asyncio.sleep(0.01)
            else:
                pytest.fail("Description materialization did not start")

            # Cancellation is requested while the detached task still owns the
            # session. Releasing the worker lets it observe that request and
            # settle ATTENTION without publishing its completed description.
            task.cancel()
            release.set()
            with pytest.raises(asyncio.CancelledError):
                await task

            assert cursor.description is None
            probe = conn.cursor()
            await execute_after_cancellation_settles(probe, "SELECT 3")
            assert await probe.fetchone() == (3,)
            assert cursor.description is None
        finally:
            release.set()
            if finder in sys.meta_path:
                sys.meta_path.remove(finder)
            await asyncio.to_thread(__import__, "decimal")
            sys.modules["decimal"] = decimal_module
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchall_cancellation_during_row_materialization_keeps_connection_reusable(
    client_context,
):
    """Row conversion cancellation does not reclaim already-released TDS ownership.

    1. Fetch the complete SQL row, then block its Python decimal conversion.
    2. Cancel the Python-facing Future while conversion remains blocked.
    3. Run another query before releasing the worker to prove protocol ownership
       was released as soon as the row read completed.
    4. Release the worker and wait for its import to finish during cleanup.
    """

    async def run():
        conn = await connect(client_context)
        entered = threading.Event()
        release = threading.Event()
        finder = BlockingDecimalFinder(entered, release)
        decimal_module = None
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT CAST(1 AS decimal(10, 2)) AS value", use_prepare=False
            )
            # Force row conversion through the blocking import hook.
            decimal_module = sys.modules.pop("decimal")
            sys.meta_path.insert(0, finder)

            task = asyncio.ensure_future(cursor.fetchall())
            for _ in range(200):
                if entered.is_set():
                    break
                # Keep the event loop responsive while the worker is blocked.
                await asyncio.sleep(0.01)
            else:
                pytest.fail("Row materialization did not start")

            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task

            # Do not release materialization yet: successful reuse here proves
            # the completed TDS read no longer owns the session.
            probe = conn.cursor()
            await probe.execute("SELECT 2", use_prepare=False)
            assert await probe.fetchone() == (2,)
            release.set()
            # Wait outside the event-loop thread for the import lock to clear.
            await asyncio.to_thread(__import__, "decimal")
        finally:
            release.set()
            if finder in sys.meta_path:
                sys.meta_path.remove(finder)
            await asyncio.to_thread(__import__, "decimal")
            if decimal_module is not None:
                sys.modules["decimal"] = decimal_module
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_nextset_keeps_event_loop_responsive_while_draining(client_context):
    async def run():
        conn = await connect(client_context)
        heartbeat_ticks = 0
        stop_heartbeat = False

        async def heartbeat():
            nonlocal heartbeat_ticks
            while not stop_heartbeat:
                heartbeat_ticks += 1
                await asyncio.sleep(0)

        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                WITH numbers AS (
                    SELECT 0 AS value
                    UNION ALL
                    SELECT value + 1 FROM numbers WHERE value < 32767
                )
                SELECT value FROM numbers OPTION (MAXRECURSION 32767);
                SELECT 42 AS final_value
                """,
                use_prepare=False,
            )
            heartbeat_task = asyncio.create_task(heartbeat())
            await asyncio.sleep(0)
            ticks_before_nextset = heartbeat_ticks

            assert await cursor.nextset() is True
            ticks_during_nextset = heartbeat_ticks - ticks_before_nextset

            assert ticks_during_nextset > 0
            assert await cursor.fetchone() == (42,)
        finally:
            stop_heartbeat = True
            if "heartbeat_task" in locals():
                await heartbeat_task
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_nextset_rejects_concurrent_operation_and_cancellation_resynchronizes_session(
    client_context,
):
    """A nextset stays exclusive until cancellation has resynchronized the session.

    1. Create the first ``nextset`` awaitable, which claims cursor ownership.
    2. Confirm a second call is rejected instead of reading concurrently.
    3. Yield once so the scheduled operation enters protocol work, then cancel it.
    4. Wait for ATTENTION settlement and prove a new cursor can reuse the session.
    """

    async def run():
        conn = await connect(client_context)
        cursor = conn.cursor()
        try:
            await cursor.execute(
                "SELECT REPLICATE(CAST('x' AS varchar(max)), 32000000); SELECT 2",
                use_prepare=False,
            )
            task = asyncio.ensure_future(cursor.nextset())
            with pytest.raises(RuntimeError, match="busy with another cursor operation"):
                cursor.nextset()
            # sleep(0) yields one event-loop turn without adding a real delay.
            await asyncio.sleep(0)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task

            probe = conn.cursor()
            await execute_after_cancellation_settles(probe, "SELECT 1")
            assert await probe.fetchone() == (1,)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_empty_result_releases_finished_batch(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute("SELECT 1 WHERE 1 = 0", use_prepare=False)

            assert await owner.fetchone() is None
            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_preserves_later_result_sets(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute("SELECT 1; SELECT 2", use_prepare=False)

            assert await owner.fetchone() == (1,)
            assert await owner.fetchone() is None
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await other.execute("SELECT 3", use_prepare=False)

            await owner.close()
            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_decodes_wide_sql_type_matrix(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST(1 AS bit),
                    CAST(255 AS tinyint),
                    CAST(-32768 AS smallint),
                    CAST(-2147483648 AS int),
                    CAST(9223372036854775807 AS bigint),
                    CAST(12.5 AS real),
                    CAST(-123.25 AS float),
                    CAST('ascii' AS varchar(10)),
                    CAST(N'東京' AS nvarchar(10)),
                    CAST(0x010203 AS varbinary(3)),
                    CAST(123.4500 AS decimal(10, 4)),
                    CAST(-987.65 AS numeric(10, 2)),
                    CAST('2026-08-20' AS date),
                    CAST('12:34:56.123456' AS time(6)),
                    CAST('2026-08-20T12:34:56' AS datetime),
                    CAST('2026-08-20T12:34:00' AS smalldatetime),
                    CAST('2026-08-20T12:34:56.123456' AS datetime2(6)),
                    CAST('2026-08-20T12:34:56.123456+05:30' AS datetimeoffset(6)),
                    CAST(12345.6789 AS money),
                    CAST(-1234.5678 AS smallmoney),
                    CAST('12345678-1234-5678-9abc-123456789abc' AS uniqueidentifier),
                    CAST('<root answer="42" />' AS xml),
                    CAST(N'{"answer":42}' AS json),
                    CAST('[1,2,3]' AS vector(3))
                """,
                use_prepare=False,
            )

            row = await cursor.fetchone()
            assert row is not None
            assert row[:23] == (
                True,
                255,
                -32768,
                -2147483648,
                9223372036854775807,
                12.5,
                -123.25,
                "ascii",
                "東京",
                b"\x01\x02\x03",
                Decimal("123.4500"),
                Decimal("-987.65"),
                datetime.date(2026, 8, 20),
                datetime.time(12, 34, 56, 123456),
                datetime.datetime(2026, 8, 20, 12, 34, 56),
                datetime.datetime(2026, 8, 20, 12, 34),
                datetime.datetime(2026, 8, 20, 12, 34, 56, 123456),
                datetime.datetime(
                    2026,
                    8,
                    20,
                    12,
                    34,
                    56,
                    123456,
                    tzinfo=datetime.timezone(datetime.timedelta(hours=5, minutes=30)),
                ),
                Decimal("12345.6789"),
                Decimal("-1234.5678"),
                uuid.UUID("12345678-1234-5678-9abc-123456789abc"),
                '<root answer="42"/>',
                '{"answer":42}',
            )
            assert row[23] == pytest.approx([1.0, 2.0, 3.0])
            assert await cursor.fetchone() is None
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_decodes_null_for_every_supported_type(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST(NULL AS bit), CAST(NULL AS tinyint),
                    CAST(NULL AS smallint), CAST(NULL AS int),
                    CAST(NULL AS bigint), CAST(NULL AS real),
                    CAST(NULL AS float), CAST(NULL AS varchar(10)),
                    CAST(NULL AS nvarchar(10)), CAST(NULL AS varbinary(10)),
                    CAST(NULL AS decimal(10, 4)), CAST(NULL AS numeric(10, 2)),
                    CAST(NULL AS date), CAST(NULL AS time(7)),
                    CAST(NULL AS datetime), CAST(NULL AS smalldatetime),
                    CAST(NULL AS datetime2(7)), CAST(NULL AS datetimeoffset(7)),
                    CAST(NULL AS money), CAST(NULL AS smallmoney),
                    CAST(NULL AS uniqueidentifier), CAST(NULL AS xml),
                    CAST(NULL AS json), CAST(NULL AS vector(3)),
                    CAST(NULL AS sql_variant), CAST(NULL AS geography),
                    CAST(NULL AS geometry), CAST(NULL AS hierarchyid)
                """,
                use_prepare=False,
            )

            row = await cursor.fetchone()
            assert row == (None,) * 28
            assert await cursor.fetchone() is None
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_preserves_temporal_boundaries(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST('0001-01-01' AS date),
                    CAST('9999-12-31' AS date),
                    CAST('23:59:59' AS time(0)),
                    CAST('12:34:56.123' AS time(3)),
                    CAST('12:34:56.1234567' AS time(7)),
                    CAST('0001-01-01T00:00:00' AS datetime2(7)),
                    CAST('9999-12-31T23:59:59.9999999' AS datetime2(7)),
                    CAST('2026-08-20T00:15:00+14:00' AS datetimeoffset(7)),
                    CAST('2026-08-20T23:45:00-14:00' AS datetimeoffset(7))
                """,
                use_prepare=False,
            )

            assert await cursor.fetchone() == (
                datetime.date(1, 1, 1),
                datetime.date(9999, 12, 31),
                datetime.time(23, 59, 59),
                datetime.time(12, 34, 56, 123000),
                datetime.time(12, 34, 56, 123456),
                datetime.datetime(1, 1, 1),
                datetime.datetime(9999, 12, 31, 23, 59, 59, 999999),
                datetime.datetime(
                    2026,
                    8,
                    20,
                    0,
                    15,
                    tzinfo=datetime.timezone(datetime.timedelta(hours=14)),
                ),
                datetime.datetime(
                    2026,
                    8,
                    20,
                    23,
                    45,
                    tzinfo=datetime.timezone(datetime.timedelta(hours=-14)),
                ),
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_preserves_all_temporal_scales_and_legacy_rounding(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST('12:34:56.1234567' AS time(0)),
                    CAST('12:34:56.1234567' AS time(1)),
                    CAST('12:34:56.1234567' AS time(2)),
                    CAST('12:34:56.1234567' AS time(3)),
                    CAST('12:34:56.1234567' AS time(4)),
                    CAST('12:34:56.1234567' AS time(5)),
                    CAST('12:34:56.1234567' AS time(6)),
                    CAST('12:34:56.1234567' AS time(7)),
                    CAST('2026-08-20T12:34:56.1234567' AS datetime2(0)),
                    CAST('2026-08-20T12:34:56.1234567' AS datetime2(1)),
                    CAST('2026-08-20T12:34:56.1234567' AS datetime2(2)),
                    CAST('2026-08-20T12:34:56.1234567' AS datetime2(3)),
                    CAST('2026-08-20T12:34:56.1234567' AS datetime2(4)),
                    CAST('2026-08-20T12:34:56.1234567' AS datetime2(5)),
                    CAST('2026-08-20T12:34:56.1234567' AS datetime2(6)),
                    CAST('2026-08-20T12:34:56.1234567' AS datetime2(7)),
                    CAST('2026-08-20T12:34:56.1234567+05:30' AS datetimeoffset(0)),
                    CAST('2026-08-20T12:34:56.1234567+05:30' AS datetimeoffset(1)),
                    CAST('2026-08-20T12:34:56.1234567+05:30' AS datetimeoffset(2)),
                    CAST('2026-08-20T12:34:56.1234567+05:30' AS datetimeoffset(3)),
                    CAST('2026-08-20T12:34:56.1234567+05:30' AS datetimeoffset(4)),
                    CAST('2026-08-20T12:34:56.1234567+05:30' AS datetimeoffset(5)),
                    CAST('2026-08-20T12:34:56.1234567+05:30' AS datetimeoffset(6)),
                    CAST('2026-08-20T12:34:56.1234567+05:30' AS datetimeoffset(7)),
                    CAST('2026-08-20T12:34:56.002' AS datetime),
                    CAST('2026-08-20T12:34:30' AS smalldatetime)
                """,
                use_prepare=False,
            )

            row = await cursor.fetchone()
            microseconds = (0, 100000, 120000, 123000, 123500, 123460, 123457, 123456)
            assert row[:8] == tuple(
                datetime.time(12, 34, 56, value) for value in microseconds
            )
            assert row[8:16] == tuple(
                datetime.datetime(2026, 8, 20, 12, 34, 56, value)
                for value in microseconds
            )
            offset = datetime.timezone(datetime.timedelta(hours=5, minutes=30))
            assert row[16:24] == tuple(
                datetime.datetime(2026, 8, 20, 12, 34, 56, value, tzinfo=offset)
                for value in microseconds
            )
            assert row[24:] == (
                datetime.datetime(2026, 8, 20, 12, 34, 56, 3333),
                datetime.datetime(2026, 8, 20, 12, 35),
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_preserves_decimal_and_money_boundaries(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST(99999999999999999999999999999999999999 AS decimal(38, 0)),
                    CAST(-0.0000001 AS numeric(38, 7)),
                    CAST(922337203685477.5807 AS money),
                    CAST(-922337203685477.5808 AS money),
                    CAST(-0.0001 AS money),
                    CAST(214748.3647 AS smallmoney),
                    CAST(-214748.3648 AS smallmoney),
                    CAST(-0.0001 AS smallmoney)
                """,
                use_prepare=False,
            )

            assert await cursor.fetchone() == (
                Decimal("99999999999999999999999999999999999999"),
                Decimal("-0.0000001"),
                Decimal("922337203685477.5807"),
                Decimal("-922337203685477.5808"),
                Decimal("-0.0001"),
                Decimal("214748.3647"),
                Decimal("-214748.3648"),
                Decimal("-0.0001"),
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_preserves_decimal_precision_scale_and_server_rounding(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST(9 AS decimal(1, 0)),
                    CAST(-9 AS numeric(1, 0)),
                    CAST(0.99999999999999999999999999999999999999 AS decimal(38, 38)),
                    CAST(9999999999999999999.9999999999999999999 AS decimal(38, 19)),
                    CAST(-0.0000000001 AS numeric(38, 10)),
                    CAST(1.23456789 AS decimal(10, 6)),
                    CAST(-1.23456789 AS numeric(10, 6)),
                    CAST(1.23456789 AS real),
                    CAST(1.2345678901234567 AS float(53))
                """,
                use_prepare=False,
            )

            row = await cursor.fetchone()
            assert row[:7] == (
                Decimal("9"),
                Decimal("-9"),
                Decimal("0.99999999999999999999999999999999999999"),
                Decimal("9999999999999999999.9999999999999999999"),
                Decimal("-0.0000000001"),
                Decimal("1.234568"),
                Decimal("-1.234568"),
            )
            assert row[7] == pytest.approx(1.23456789, rel=1e-6)
            assert row[8] == pytest.approx(1.2345678901234567, rel=1e-15)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_decodes_empty_fixed_and_large_lob_values(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST('' AS varchar(max)),
                    CAST(N'' AS nvarchar(max)),
                    CAST(0x AS varbinary(max)),
                    CAST('x' AS char(4)),
                    CAST(N'東' AS nchar(3)),
                    CAST(0x0102 AS binary(4)),
                    REPLICATE(CAST('ab' AS varchar(max)), 50000),
                    REPLICATE(CAST(N'東京' AS nvarchar(max)), 25000),
                    CONVERT(varbinary(max), REPLICATE(CAST('ab' AS varchar(max)), 50000)),
                    CAST('<root>' + REPLICATE(CAST('x' AS varchar(max)), 100000)
                        + '</root>' AS xml)
                """,
                use_prepare=False,
            )

            row = await cursor.fetchone()
            assert row is not None
            assert row[:6] == ("", "", b"", "x   ", "東  ", b"\x01\x02\x00\x00")
            assert row[6] == "ab" * 50000
            assert row[7] == "東京" * 25000
            assert row[8] == b"ab" * 50000
            assert row[9] == f"<root>{'x' * 100000}</root>"
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_decodes_sql_variant_base_type_matrix(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST(CAST(123 AS int) AS sql_variant),
                    CAST(CAST(255 AS tinyint) AS sql_variant),
                    CAST(CAST(-32000 AS smallint) AS sql_variant),
                    CAST(CAST(9223372036854775807 AS bigint) AS sql_variant),
                    CAST(CAST(12.5 AS real) AS sql_variant),
                    CAST(CAST(-123.25 AS float) AS sql_variant),
                    CAST(CAST(999.99 AS decimal(10, 2)) AS sql_variant),
                    CAST(CAST(1 AS bit) AS sql_variant),
                    CAST(CAST('Hello' AS varchar(10)) AS sql_variant),
                    CAST(CAST(N'東京' AS nvarchar(10)) AS sql_variant),
                    CAST(CAST('2026-08-20' AS date) AS sql_variant),
                    CAST(CAST('12:34:56.1234567' AS time(7)) AS sql_variant),
                    CAST(CAST('2026-08-20T12:34:56' AS datetime2) AS sql_variant),
                    CAST(CAST(0x48656C6C6F AS varbinary(5)) AS sql_variant),
                    CAST(CAST('12345678-1234-5678-9abc-123456789abc'
                        AS uniqueidentifier) AS sql_variant),
                    CAST(NULL AS sql_variant)
                """,
                use_prepare=False,
            )

            assert await cursor.fetchone() == (
                123,
                255,
                -32000,
                9223372036854775807,
                12.5,
                -123.25,
                Decimal("999.99"),
                True,
                "Hello",
                "東京",
                datetime.date(2026, 8, 20),
                datetime.time(12, 34, 56, 123456),
                datetime.datetime(2026, 8, 20, 12, 34, 56),
                b"Hello",
                uuid.UUID("12345678-1234-5678-9abc-123456789abc"),
                None,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_decodes_spatial_and_hierarchyid_as_bytes(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    geography::Point(47.651, -122.349, 4326),
                    geometry::STGeomFromText('POINT (10 20)', 0),
                    hierarchyid::Parse('/1/2/'),
                    CAST(NULL AS geography),
                    CAST(NULL AS geometry),
                    CAST(NULL AS hierarchyid)
                """,
                use_prepare=False,
            )

            row = await cursor.fetchone()
            assert row is not None
            assert all(isinstance(value, bytes) and value for value in row[:3])
            assert row[3:] == (None, None, None)
        finally:
            await conn.close()

    asyncio.run(run())


def test_fetchone_after_connection_close_raises(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        cursor = conn.cursor()
        await conn.close()

        with pytest.raises(RuntimeError, match="Connection is closed"):
            await cursor.fetchone()

    asyncio.run(run())


@pytest.mark.integration
def test_exhausted_fetchone_after_connection_close_raises(client_context):
    async def run():
        conn = await connect(client_context)
        cursor = conn.cursor()
        await cursor.execute("SELECT 1", use_prepare=False)
        assert await cursor.fetchone() == (1,)
        assert await cursor.fetchone() is None

        await conn.close()
        with pytest.raises(RuntimeError, match="Connection is closed"):
            await cursor.fetchone()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("operation", ["fetchone", "fetchmany", "fetchall"])
def test_fetch_rejects_concurrent_read_on_same_cursor(client_context, operation):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT REPLICATE(CAST('x' AS varchar(max)), 8000000)",
                use_prepare=False,
            )

            operations = {
                "fetchone": lambda: cursor.fetchone(),
                "fetchmany": lambda: cursor.fetchmany(1),
                "fetchall": lambda: cursor.fetchall(),
            }
            first = operations[operation]()
            with pytest.raises(RuntimeError, match="busy with another cursor operation"):
                operations[operation]()
            result = await first
            value = result[0] if operation == "fetchone" else result[0][0]
            assert len(value) == 8000000
            assert await cursor.fetchone() is None
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("operation", ["fetchone", "fetchmany", "fetchall"])
def test_cancelling_blocked_fetch_resynchronizes_session(client_context, operation):
    """Every fetch API resynchronizes when cancelled during a blocked row read.

    1. Start a large result and choose one of the three fetch APIs.
    2. Schedule its awaitable and yield one event-loop turn so reading begins.
    3. Cancel it and verify Python receives ``CancelledError``.
    4. Wait for the detached Rust task to finish ATTENTION settlement.
    5. Execute another query to prove the shared session can be reused.
    """

    async def run():
        conn = await connect(client_context)
        cursor = conn.cursor()
        try:
            await cursor.execute(
                "SELECT REPLICATE(CAST('x' AS varchar(max)), 32000000)",
                use_prepare=False,
            )
            operations = {
                "fetchone": lambda: cursor.fetchone(),
                "fetchmany": lambda: cursor.fetchmany(1),
                "fetchall": lambda: cursor.fetchall(),
            }
            awaitable = operations[operation]()
            # ensure_future accepts the Future returned by the PyO3 binding.
            task = asyncio.ensure_future(awaitable)
            # Give the scheduled fetch one turn to enter the large row read.
            await asyncio.sleep(0)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task

            probe = conn.cursor()
            await execute_after_cancellation_settles(probe, "SELECT 1")
            assert await probe.fetchone() == (1,)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_bounded_loop_preserves_order_values_and_reuse(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                WITH numbers AS (
                    SELECT 0 AS value
                    UNION ALL
                    SELECT value + 1 FROM numbers WHERE value < 255
                )
                SELECT value, CONCAT('V_', value), value * 3
                FROM numbers ORDER BY value
                OPTION (MAXRECURSION 256)
                """,
                use_prepare=False,
            )

            for value in range(256):
                assert await cursor.fetchone() == (value, f"V_{value}", value * 3)
            assert await cursor.fetchone() is None
            assert await cursor.fetchone() is None

            await cursor.execute("SELECT 42", use_prepare=False)
            assert await cursor.fetchone() == (42,)
            assert await cursor.fetchone() is None
        finally:
            await conn.close()

    asyncio.run(run())