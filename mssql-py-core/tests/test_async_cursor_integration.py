# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Integration tests for the coroutine cursor (`PyCoreAsyncCursor`).

These require a live SQL Server (`client_context` is built from `.env` /
environment by `conftest.py` and skips when credentials are absent). They are
marked `integration` and run in CI where a server is available.
"""

import asyncio
import time

import pytest

import mssql_py_core


@pytest.mark.integration
async def test_async_cursor_execute_fetch(client_context):
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cur = conn.async_cursor()
        await cur.execute("SELECT 1 AS value")
        row = await cur.fetchone()
        assert row is not None
        assert row[0] == 1
        await cur.close()
    finally:
        conn.close()


@pytest.mark.integration
async def test_async_cursor_nonblocking_during_waitfor(client_context):
    """The event loop stays free while a coroutine fetch is in flight.

    A server-side `WAITFOR DELAY` holds the fetch open for ~2s. A concurrent
    ticker increments every ~10ms. If the coroutine were `block_on`-backed it
    would pin the event-loop thread for the whole delay and the ticker would
    barely advance; because the fetch is spawned onto the connection's tokio
    runtime and merely awaited, the loop keeps running and the ticker racks up
    ticks proportional to the elapsed wall-clock time.
    """
    delay_secs = 2
    tick_interval = 0.01

    ticks = 0
    stop = False

    async def ticker():
        nonlocal ticks
        while not stop:
            ticks += 1
            await asyncio.sleep(tick_interval)

    conn = mssql_py_core.PyCoreConnection(client_context)
    ticker_task = asyncio.create_task(ticker())
    try:
        cur = conn.async_cursor()
        started = time.monotonic()
        await cur.execute(f"WAITFOR DELAY '00:00:0{delay_secs}'; SELECT 1 AS value")
        row = await cur.fetchone()
        elapsed = time.monotonic() - started
        await cur.close()
    finally:
        stop = True
        await ticker_task
        conn.close()

    assert row is not None and row[0] == 1
    # The fetch really did take about the WAITFOR duration.
    assert elapsed >= delay_secs * 0.5
    # A blocked loop would yield only a handful of ticks; a free loop yields
    # roughly elapsed / tick_interval. Use a conservative floor to stay robust
    # against scheduler jitter while still failing hard if the loop was pinned.
    assert ticks >= (delay_secs / tick_interval) * 0.25
