# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Coroutine cursor (`conn.async_cursor()` -> PyCoreAsyncCursor) tests against
the mock TDS server.

These exercise the genuine `asyncio` coroutine path: `execute`/`fetchone`/
`fetchall`/`fetchmany`/`close` each return a Python awaitable built by
`pyo3_async_runtimes::tokio::future_into_py`. There is no `block_on` on this
path — the TDS I/O is spawned onto the connection's own tokio runtime and the
coroutine `.await`s the resulting join handle, so the asyncio event loop stays
free to run other tasks while a fetch is in flight.

The mock answers `SELECT 1` and `SELECT CAST(1 AS BIGINT), 2, 3`; unknown
SELECTs return an empty result set. The discriminating "loop stays free during
a long server-side WAITFOR" proof lives in the integration suite (it needs a
live server); here we prove correctness of the coroutine API plus that the
coroutine actually suspends onto the event loop rather than running inline.
"""

import asyncio

import pytest

try:
    import mssql_mock_tds

    MOCK_TDS_PY_AVAILABLE = True
except ImportError:
    MOCK_TDS_PY_AVAILABLE = False

pytestmark = pytest.mark.skipif(
    not MOCK_TDS_PY_AVAILABLE,
    reason="mssql_mock_tds not available. Build it with: cd mssql-mock-tds-py && maturin develop",
)


def _ctx(server, encryption="Optional"):
    """Build a sql_auth client context pointed at the mock server."""
    return {
        "server": server.sql_address,
        "database": "master",
        "user_name": "sa",
        "password": "unused-by-mock",
        "encryption": encryption,
        "trust_server_certificate": True,
    }


@pytest.fixture
def plaintext_server():
    """A plaintext (non-TLS) mock TDS server."""
    server = mssql_mock_tds.PyMockTdsServer(port=0, tls=False)
    with server:
        yield server


def _connect(ctx):
    import mssql_py_core

    return mssql_py_core.PyCoreConnection(ctx)


# ---------------------------------------------------------------------------
# Coroutine API — awaitable execute / fetch* / close
# ---------------------------------------------------------------------------


async def test_async_cursor_fetchone_plaintext(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        cur = conn.async_cursor()
        await cur.execute("SELECT 1")
        assert await cur.fetchone() == (1,)
        assert await cur.fetchone() is None
        await cur.close()
    finally:
        conn.close()


async def test_async_cursor_fetchall_plaintext(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        cur = conn.async_cursor()
        await cur.execute("SELECT CAST(1 AS BIGINT), 2, 3")
        assert await cur.fetchall() == [(1, 2, 3)]
        await cur.close()
    finally:
        conn.close()


async def test_async_cursor_fetchmany_plaintext(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        cur = conn.async_cursor()
        await cur.execute("SELECT 1")
        assert await cur.fetchmany(1) == [(1,)]
        assert await cur.fetchmany(1) == []
        await cur.close()
    finally:
        conn.close()


async def test_async_matches_sync_rows_plaintext(plaintext_server):
    """The coroutine cursor decodes byte-identical rows to the sync cursor for
    the same statement — both drive the one shared parse body."""
    conn = _connect(_ctx(plaintext_server))
    try:
        query = "SELECT CAST(1 AS BIGINT), 2, 3"

        ac = conn.async_cursor()
        await ac.execute(query)
        async_rows = await ac.fetchall()
        await ac.close()

        sc = conn.sync_cursor()
        sc.execute(query)
        sync_rows = sc.fetchall()
        sc.close()

        assert async_rows == sync_rows
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# The coroutine suspends onto the event loop (does not run inline)
# ---------------------------------------------------------------------------


async def test_async_cursor_yields_to_event_loop(plaintext_server):
    """A concurrent ticker advances while a fetch coroutine is awaited, proving
    the coroutine hands control back to the asyncio loop rather than running to
    completion inline on the loop thread.

    The mock answers in microseconds, so this asserts the weaker "yields at
    least once" property; the wall-clock "loop stays free during a long fetch"
    proof is the WAITFOR integration test.
    """
    ticks = 0
    stop = False

    async def ticker():
        nonlocal ticks
        while not stop:
            ticks += 1
            await asyncio.sleep(0)

    conn = _connect(_ctx(plaintext_server))
    ticker_task = asyncio.create_task(ticker())
    try:
        cur = conn.async_cursor()
        for _ in range(50):
            await cur.execute("SELECT 1")
            await cur.fetchall()
        await cur.close()
    finally:
        stop = True
        await ticker_task
        conn.close()

    assert ticks > 0
