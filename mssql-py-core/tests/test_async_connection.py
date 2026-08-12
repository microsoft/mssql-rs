# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for PyAsyncConnection: connect, close, commit, rollback, cursor.

NOTE: PyAsyncConnection is a preview API. `PREVIEW_WARNED` is a process-wide
AtomicBool that latches to True after the first successful FutureWarning
emission — see `test_future_warning_propagates_when_promoted_to_error` for the
ordering constraint that follows.
"""

import asyncio
import warnings

import pytest
import mssql_py_core


# ---------------------------------------------------------------------------
# Preview warning
# ---------------------------------------------------------------------------

# This test must run before any other test in this session that awaits
# PyAsyncConnection.connect(). Once the warning fires successfully,
# PREVIEW_WARNED (a process-wide AtomicBool in Rust) is set to True and the
# warning cannot be re-observed in this process. If invocation order places
# another connect-issuing test before this one, this test self-skips.
def test_future_warning_propagates_when_promoted_to_error():
    """warnings.filterwarnings('error', FutureWarning) makes connect() raise it."""
    async def try_connect():
        try:
            await mssql_py_core.PyAsyncConnection.connect({})
        except FutureWarning:
            return "warning_raised"
        except Exception as exc:
            return f"other:{type(exc).__name__}"
        return "no_error"

    with warnings.catch_warnings():
        warnings.simplefilter("error", FutureWarning)
        outcome = asyncio.run(try_connect())

    if outcome == "warning_raised":
        return
    if outcome.startswith("other:"):
        pytest.skip(
            f"PREVIEW_WARNED already latched by an earlier test; connect proceeded and hit {outcome[6:]}"
        )
    pytest.fail(f"unexpected outcome: {outcome!r}")


# ---------------------------------------------------------------------------
# Connect
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_connect_returns_pyasyncconnection(client_context):
    """Awaiting connect() yields a PyAsyncConnection instance."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                assert isinstance(conn, mssql_py_core.PyAsyncConnection)
            finally:
                await conn.close()

    asyncio.run(run())


# ---------------------------------------------------------------------------
# Close
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_close_is_awaitable(client_context):
    """close() returns an awaitable that resolves cleanly."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            # pyo3-async-runtimes converts Rust `()` to a Python empty tuple.
            result = await conn.close()
            assert result is None or result == ()

    asyncio.run(run())


@pytest.mark.integration
def test_close_is_idempotent(client_context):
    """Awaiting close() twice does not raise."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            await conn.close()
            await conn.close()  # no-op path (tds_client is None)

    asyncio.run(run())


# ---------------------------------------------------------------------------
# Commit / Rollback
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_commit_returns_awaitable_that_resolves(client_context):
    """commit() returns an awaitable; resolves cleanly or raises RuntimeError."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                # With no active transaction, SQL Server returns error 3902
                # which the driver surfaces as RuntimeError. Either the server
                # accepts the commit (auto-commit mode) or raises — both are
                # acceptable outcomes for this plumbing test.
                try:
                    result = await conn.commit()
                    assert result is None or result == ()
                except RuntimeError as e:
                    assert "Commit failed" in str(e)
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_rollback_returns_awaitable_that_resolves(client_context):
    """rollback() returns an awaitable; resolves cleanly or raises RuntimeError."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                try:
                    result = await conn.rollback()
                    assert result is None or result == ()
                except RuntimeError as e:
                    assert "Rollback failed" in str(e)
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_commit_after_close_raises_connection_closed(client_context):
    """commit() on a closed connection raises RuntimeError synchronously."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            await conn.close()
            with pytest.raises(RuntimeError, match="Connection is closed"):
                await conn.commit()

    asyncio.run(run())


@pytest.mark.integration
def test_rollback_after_close_raises_connection_closed(client_context):
    """rollback() on a closed connection raises RuntimeError synchronously."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            await conn.close()
            with pytest.raises(RuntimeError, match="Connection is closed"):
                await conn.rollback()

    asyncio.run(run())
