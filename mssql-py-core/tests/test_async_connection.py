# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for PyAsyncConnection: connect, close, commit, rollback, cursor."""

import asyncio
import subprocess
import sys
import textwrap
import warnings

import pytest
import mssql_py_core


# ---------------------------------------------------------------------------
# Preview warning
# ---------------------------------------------------------------------------

# Isolated in a fresh interpreter so PREVIEW_WARNED (the process-wide
# AtomicBool latch in Rust) is guaranteed False; no invocation-order coupling
# and no fail-open path where a real regression would surface as a skip.
def test_future_warning_propagates_when_promoted_to_error():
    """warnings.filterwarnings('error', FutureWarning) makes connect() raise it."""
    script = textwrap.dedent(
        """
        import asyncio, sys, warnings
        import mssql_py_core

        warnings.simplefilter("error", FutureWarning)

        async def main():
            try:
                await mssql_py_core.PyAsyncConnection.connect({})
            except FutureWarning:
                sys.exit(0)
            sys.exit(1)

        asyncio.run(main())
        """
    )
    result = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        pytest.fail(
            f"expected FutureWarning to be raised in subprocess "
            f"(exit={result.returncode}, stderr={result.stderr!r})"
        )


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
    """close() returns an awaitable that resolves to None."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            result = await conn.close()
            assert result is None

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
    """commit() with no active transaction always raises SQL Server 3902."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                # PyAsyncConnection has no begin_transaction, so a fresh
                # connection has no open TDS transaction; TM_COMMIT deterministically
                # yields SQL Server 3902. Matching the server error number keeps
                # this valid after the DB-API error taxonomy lands.
                with pytest.raises(Exception, match="3902"):
                    await conn.commit()
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_rollback_returns_awaitable_that_resolves(client_context):
    """rollback() with no active transaction always raises SQL Server 3903."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                # Same rationale as commit: fresh connection has no open TDS
                # transaction; TM_ROLLBACK deterministically yields 3903.
                with pytest.raises(Exception, match="3903"):
                    await conn.rollback()
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


# ---------------------------------------------------------------------------
# timeout getter/setter (default query timeout for cursors; 0 = no timeout)
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_timeout_default_is_zero(client_context):
    """Fresh connection reports timeout=0 (pyodbc/ODBC convention: no timeout)."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                assert conn.timeout == 0
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_timeout_setter_roundtrip(client_context):
    """Setter accepts non-negative int; getter reflects last value written."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                conn.timeout = 30
                assert conn.timeout == 30
                conn.timeout = 0
                assert conn.timeout == 0
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_timeout_setter_rejects_negative(client_context):
    """Negative values overflow the u32 extractor and raise OverflowError."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                with pytest.raises(OverflowError):
                    conn.timeout = -1
                # Original value preserved.
                assert conn.timeout == 0
            finally:
                await conn.close()

    asyncio.run(run())
