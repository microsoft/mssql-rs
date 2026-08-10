# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Integration tests for the sync + async cursors against a live server.

These require a reachable SQL Server (via the standard ``client_context``
fixture / ``.env``) and are marked ``integration``, so they skip when no server
is configured. The mock-server-backed behavioral tests for the sync/async
cursors live in ``tests/rs-only-tests/test_sync_async_cursor_mock.py``.

``conn.cursor()`` is the synchronous cursor whose row-pull hot loop runs on the
reactor-free sync edge; ``conn.async_cursor()`` is the genuine ``asyncio``
coroutine cursor over the async edge.
"""

import asyncio

import pytest

import mssql_py_core


@pytest.mark.integration
def test_sync_cursor_execute(client_context):
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS value")
        result = cursor.fetchone()
        assert result is not None
        assert result[0] == 1
        cursor.close()
    finally:
        conn.close()


@pytest.mark.integration
def test_sync_cursor_fetchall(client_context):
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cursor = conn.cursor()
        cursor.execute("SELECT 1 AS value UNION ALL SELECT 2 UNION ALL SELECT 3")
        results = cursor.fetchall()
        assert [row[0] for row in results] == [1, 2, 3]
        cursor.close()
    finally:
        conn.close()


@pytest.mark.integration
async def test_sync_cursor_matches_async(client_context):
    """The sync fetch path returns byte-identical rows to the coroutine path."""
    query = "SELECT 1 AS a, CAST('x' AS NVARCHAR(10)) AS b UNION ALL SELECT 2, 'y'"

    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        ac = conn.async_cursor()
        await ac.execute(query)
        async_rows = await ac.fetchall()
        await ac.close()

        sc = conn.cursor()
        sc.execute(query)
        sync_rows = sc.fetchall()
        sc.close()

        assert sync_rows == async_rows
    finally:
        conn.close()


@pytest.mark.integration
async def test_async_cursor_fetch(client_context):
    """The coroutine cursor awaits execute/fetch and returns correct rows."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cur = conn.async_cursor()
        await cur.execute("SELECT 1 AS value UNION ALL SELECT 2 UNION ALL SELECT 3")
        assert await cur.fetchone() == (1,)
        assert await cur.fetchall() == [(2,), (3,)]
        await cur.close()
    finally:
        conn.close()


@pytest.mark.integration
async def test_async_cursor_does_not_block_loop(client_context):
    """A genuine non-blocking proof: a ~1s server-side ``WAITFOR DELAY`` runs
    inside the coroutine's ``execute`` await. If the fetch blocked the event
    loop (as a ``block_on`` path would), a concurrent 10 ms ticker could not
    advance during that second. With the coroutine cursor the loop stays free,
    so the ticker racks up many ticks."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cur = conn.async_cursor()

        ticks = 0
        stop = False

        async def ticker():
            nonlocal ticks
            while not stop:
                await asyncio.sleep(0.01)
                ticks += 1

        ticker_task = asyncio.ensure_future(ticker())
        await cur.execute("WAITFOR DELAY '00:00:01'; SELECT 1 AS value")
        rows = await cur.fetchall()
        stop = True
        await ticker_task

        assert rows == [(1,)]
        # ~1s / 10ms ≈ 100 ticks when non-blocking; a blocked loop yields ~0.
        assert ticks >= 10
        await cur.close()
    finally:
        conn.close()


@pytest.mark.integration
def test_dml_rowcount_async(client_context):
    """A count-bearing DML statement captures rowcount on the async edge before
    any flip (the sync cursor never flips for a rowless DML result)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE #l6_rowcount (id INT)")
        cursor.execute("INSERT INTO #l6_rowcount (id) VALUES (1), (2), (3)")
        assert cursor.rowcount == 3
        cursor.close()
    finally:
        conn.close()


@pytest.mark.integration
async def test_rowcount_sync_equals_async_select(client_context):
    query = "SELECT 1 AS value UNION ALL SELECT 2 UNION ALL SELECT 3"

    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        ac = conn.async_cursor()
        await ac.execute(query)
        async_rowcount = ac.rowcount
        await ac.fetchall()
        await ac.close()

        sc = conn.cursor()
        sc.execute(query)
        sync_rowcount = sc.rowcount
        sc.fetchall()
        sc.close()

        assert sync_rowcount == async_rowcount
    finally:
        conn.close()


@pytest.mark.integration
def test_sync_cursor_error_mid_fetch_recovers(client_context):
    """A fetch error on the sync edge surfaces, then the cell reverts to async
    so the connection stays usable (ruling 4: recover via into_async drain)."""
    # The conversion trips on the third row, after rows have started streaming.
    bad_query = (
        "SELECT CAST(value AS INT) AS n "
        "FROM (VALUES ('1'), ('2'), ('notanumber')) AS t(value)"
    )

    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        sc = conn.cursor()
        sc.execute(bad_query)
        with pytest.raises(RuntimeError):
            sc.fetchall()
        sc.close()

        # The connection reverted to the async edge and remains usable.
        ac = conn.cursor()
        ac.execute("SELECT 1")
        assert ac.fetchone() == (1,)
        ac.close()
    finally:
        conn.close()


@pytest.mark.integration
async def test_dml_rowcount_sync_equals_async(client_context):
    """DML never flips to the sync edge, so both cursors capture the same count
    sourced from the one async oracle."""
    dml = "INSERT INTO #l6_rowcount_parity (id) VALUES (1), (2)"

    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        sc = conn.cursor()
        sc.execute("CREATE TABLE #l6_rowcount_parity (id INT)")
        sc.execute(dml)
        sync_rowcount = sc.rowcount
        sc.close()

        ac = conn.async_cursor()
        await ac.execute(dml)
        async_rowcount = ac.rowcount
        await ac.close()

        assert sync_rowcount == async_rowcount
    finally:
        conn.close()
