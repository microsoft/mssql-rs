# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Deterministic live-SQL cancellation tests for non-Linux CI platforms."""

import asyncio
import warnings

import mssql_py_core
import pytest


async def _connect(client_context):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await asyncio.wait_for(
            mssql_py_core.PyAsyncConnection.connect(client_context),
            timeout=20,
        )


async def _start_waiting_nextset(client_context):
    connection = None
    monitor = None
    operation = None
    ready = False
    try:
        connection = await _connect(client_context)
        monitor = await _connect(client_context)
        cursor = connection.cursor()
        await asyncio.wait_for(
            cursor.execute(
                "SELECT @@SPID; WAITFOR DELAY '00:01:00'; SELECT 2",
                use_prepare=False,
            ),
            timeout=15,
        )
        session_id = (
            await asyncio.wait_for(cursor.fetchone(), timeout=5)
        )[0]
        operation = asyncio.ensure_future(cursor.nextset())
        monitor_cursor = monitor.cursor()
        deadline = asyncio.get_running_loop().time() + 15
        while asyncio.get_running_loop().time() < deadline:
            await asyncio.wait_for(
                monitor_cursor.execute(
                    "SELECT wait_type FROM sys.dm_exec_requests "
                    f"WHERE session_id = {session_id}",
                    use_prepare=False,
                ),
                timeout=5,
            )
            row = await asyncio.wait_for(monitor_cursor.fetchone(), timeout=5)
            if row is not None and row[0] == "WAITFOR":
                ready = True
                return connection, monitor, cursor, operation
            await asyncio.sleep(0.05)
        pytest.fail("The SQL session never entered WAITFOR")
    finally:
        if not ready:
            if operation is not None:
                operation.cancel()
                await asyncio.wait_for(
                    asyncio.gather(operation, return_exceptions=True),
                    timeout=10,
                )
            if connection is not None:
                await asyncio.wait_for(connection.close(), timeout=10)
            if monitor is not None:
                await asyncio.wait_for(monitor.close(), timeout=10)


@pytest.mark.integration
def test_cancelled_nextset_during_server_wait_resynchronizes_connection(client_context):
    async def run():
        connection, monitor, cursor, operation = await _start_waiting_nextset(client_context)
        try:
            operation.cancel()
            with pytest.raises(asyncio.CancelledError):
                await asyncio.wait_for(operation, timeout=10)

            probe = connection.cursor()
            await asyncio.wait_for(probe.execute("SELECT 1", use_prepare=False), timeout=10)
            assert await asyncio.wait_for(probe.fetchone(), timeout=10) == (1,)
        finally:
            if not operation.done():
                operation.cancel()
                await asyncio.wait_for(
                    asyncio.gather(operation, return_exceptions=True),
                    timeout=10,
                )
            await asyncio.wait_for(connection.close(), timeout=10)
            await asyncio.wait_for(monitor.close(), timeout=10)

    asyncio.run(run())


@pytest.mark.integration
def test_cancelled_close_waiter_during_server_wait_finishes_close(client_context):
    async def run():
        connection, monitor, cursor, operation = await _start_waiting_nextset(client_context)
        close_waiter = asyncio.ensure_future(connection.close())
        try:
            assert connection.closed
            assert not close_waiter.done()
            close_waiter.cancel()
            with pytest.raises(asyncio.CancelledError):
                await asyncio.wait_for(close_waiter, timeout=10)

            await asyncio.wait_for(connection.close(), timeout=10)
            assert connection.closed
            await asyncio.wait_for(
                asyncio.gather(operation, return_exceptions=True),
                timeout=10,
            )
            with pytest.raises(mssql_py_core.InterfaceError, match="Connection is closed"):
                await cursor.fetchone()
        finally:
            if not close_waiter.done():
                close_waiter.cancel()
                await asyncio.wait_for(
                    asyncio.gather(close_waiter, return_exceptions=True),
                    timeout=10,
                )
            await asyncio.wait_for(connection.close(), timeout=10)
            await asyncio.wait_for(monitor.close(), timeout=10)

    asyncio.run(run())
