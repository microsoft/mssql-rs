# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for PyAsyncConnection: connect, close, commit, rollback, cursor,
timeout, lifecycle state (closed/is_connected), async context manager, repr."""

import asyncio
import subprocess
import sys
import textwrap
import warnings
from decimal import Decimal

import pytest
import mssql_py_core


class RecordingLogger:
    def __init__(self):
        self.messages = []

    def py_core_log(self, _level, message, _module_name, _line):
        self.messages.append(message)


def test_async_connection_exposes_user_facing_docstrings():
    assert "Do not cancel" in mssql_py_core.PyAsyncConnection.__doc__
    assert "autocommit" in mssql_py_core.PyAsyncConnection.connect.__doc__
    assert "Idempotent" in mssql_py_core.PyAsyncConnection.close.__doc__


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


@pytest.mark.integration
def test_successful_connect_logs_while_awaitable_is_polled(client_context):
    async def run():
        logger = RecordingLogger()
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(
                client_context, logger
            )
            try:
                assert any(
                    "PyAsyncConnection::connect: connection established" in message
                    for message in logger.messages
                )
            finally:
                await conn.close()

    asyncio.run(run())


def test_failed_connect_logs_while_awaitable_is_polled():
    async def run():
        logger = RecordingLogger()
        invalid_context = {
            "server": "127.0.0.1,1",
            "database": "master",
            "user_name": "sa",
            "password": "invalid",
            "trust_server_certificate": True,
            "encryption": "Optional",
        }
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            with pytest.raises(Exception, match="Failed to connect to SQL Server"):
                await mssql_py_core.PyAsyncConnection.connect(invalid_context, logger)
        assert any(
            "PyAsyncConnection::connect: failed" in message
            for message in logger.messages
        )

    asyncio.run(run())


@pytest.mark.integration
def test_connection_operations_reuse_connect_logger(client_context):
    async def run():
        logger = RecordingLogger()
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(
                client_context, logger
            )

        logger.messages.clear()
        assert await conn.commit() is None

        logger.messages.clear()
        cursor = conn.cursor()
        await cursor.execute("SELECT 1 AS value", use_prepare=False)
        assert any(
            "PyAsyncCursor::execute: query executed successfully; "
            "has_result_set=true; column_count=1; description_materialization_ms="
            in message
            for message in logger.messages
        )
        assert await cursor.fetchone() == (1,)

        decimal_module = sys.modules["decimal"]
        sys.modules["decimal"] = None
        logger.messages.clear()
        try:
            with pytest.raises(
                RuntimeError,
                match="Query executed but cursor description materialization failed",
            ):
                await cursor.execute(
                    "SELECT CAST(1 AS decimal(10, 2)) AS value", use_prepare=False
                )
        finally:
            sys.modules["decimal"] = decimal_module
        assert cursor.description is None
        assert any(
            "PyAsyncCursor::execute: cursor description materialization failed; "
            "column_count=1; elapsed_ms=" in message
            for message in logger.messages
        )
        assert await cursor.fetchone() == (Decimal("1.00"),)
        await cursor.close()

        logger.messages.clear()
        await conn.close()
        assert any(
            "PyAsyncConnection::close: connection closed" in message
            for message in logger.messages
        )

    asyncio.run(run())


@pytest.mark.integration
def test_autocommit_defaults_false_and_can_be_enabled(client_context):
    # TODO: When mode changes are supported, cover lazy begin, restart after
    # commit/rollback, both mode transitions, context finalization, and
    # cleanup-error precedence.
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            default_conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            autocommit_conn = await mssql_py_core.PyAsyncConnection.connect(
                client_context, autocommit=True
            )
            try:
                assert default_conn.autocommit is False
                assert autocommit_conn.autocommit is True
            finally:
                await default_conn.close()
                await autocommit_conn.close()

    asyncio.run(run())


# ---------------------------------------------------------------------------
# Close
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_close_resolves_to_none(client_context):
    """Regression guard: close() awaitable must resolve to None, not empty tuple."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            result = await conn.close()
            assert result is None

    asyncio.run(run())


# ---------------------------------------------------------------------------
# Commit / Rollback
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_commit_without_active_transaction_is_noop(client_context):
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                assert await conn.commit() is None
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_rollback_without_active_transaction_is_noop(client_context):
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                assert await conn.rollback() is None
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
def test_timeout_default_and_setter_roundtrip(client_context):
    """Default is 0 (pyodbc/ODBC convention: no timeout); setter roundtrips."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                assert conn.timeout == 0
                conn.timeout = 30
                assert conn.timeout == 30
                conn.timeout = 0
                assert conn.timeout == 0
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_timeout_setter_rejects_negative(client_context):
    """Negative values match the Python wrapper's ValueError contract."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                with pytest.raises(ValueError, match="Timeout cannot be negative"):
                    conn.timeout = -1
                assert conn.timeout == 0
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_timeout_setter_rejects_non_integer_and_overflow(client_context):
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                with pytest.raises(TypeError):
                    conn.timeout = "30"
                with pytest.raises(OverflowError):
                    conn.timeout = 2**32
                assert conn.timeout == 0
            finally:
                await conn.close()

    asyncio.run(run())


# ---------------------------------------------------------------------------
# Lifecycle state: closed (property) + is_connected() + idempotency
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_lifecycle_state_reflects_closed_and_is_connected(client_context):
    """closed and is_connected() are inverses at both live and closed states;
    close() is idempotent — a second close keeps closed=True."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            # Live state.
            assert conn.closed is False
            assert conn.is_connected() is True
            assert conn.is_connected() is (not conn.closed)
            # First close transitions to closed.
            await conn.close()
            assert conn.closed is True
            assert conn.is_connected() is False
            assert conn.is_connected() is (not conn.closed)
            # Idempotent close keeps state.
            await conn.close()
            assert conn.closed is True

    asyncio.run(run())


# ---------------------------------------------------------------------------
# __aenter__ / __aexit__ — async context manager
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_async_context_manager_closes_on_exit(client_context):
    """`async with` awaits close() on exit; conn.closed becomes True."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn_ref = None
            async with await mssql_py_core.PyAsyncConnection.connect(client_context) as conn:
                conn_ref = conn
                assert conn.closed is False
            assert conn_ref.closed is True

    asyncio.run(run())


@pytest.mark.integration
def test_async_context_manager_yields_same_object(client_context):
    """__aenter__ resolves to `self` — the same PyAsyncConnection instance."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            outer = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                async with outer as inner:
                    assert inner is outer
            finally:
                if not outer.closed:
                    await outer.close()

    asyncio.run(run())


@pytest.mark.integration
def test_async_context_manager_rejects_closed_connection(client_context):
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            await conn.close()

            entered = False
            with pytest.raises(RuntimeError, match="Connection is closed"):
                async with conn:
                    entered = True
            assert entered is False

    asyncio.run(run())


@pytest.mark.integration
def test_async_context_manager_propagates_exception_and_still_closes(client_context):
    """Exception inside the block propagates AND the connection is closed."""
    class Boom(RuntimeError):
        pass

    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn_ref = None
            with pytest.raises(Boom, match="kaboom"):
                async with await mssql_py_core.PyAsyncConnection.connect(client_context) as conn:
                    conn_ref = conn
                    raise Boom("kaboom")
            assert conn_ref is not None
            assert conn_ref.closed is True

    asyncio.run(run())


# ---------------------------------------------------------------------------
# __repr__ — introspection
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_repr_reflects_lifecycle(client_context):
    """repr flips from 'PyAsyncConnection(connected)' to '(closed)' after close."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            assert repr(conn) == "PyAsyncConnection(connected)"
            await conn.close()
            assert repr(conn) == "PyAsyncConnection(closed)"

    asyncio.run(run())


# ---------------------------------------------------------------------------
# cursor() — sync method returning PyAsyncCursor
# ---------------------------------------------------------------------------

@pytest.mark.integration
def test_cursor_returns_pyasynccursor(client_context):
    """cursor() on a live connection returns a PyAsyncCursor instance."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                cur = conn.cursor()
                assert isinstance(cur, mssql_py_core.PyAsyncCursor)
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_cursor_after_close_raises_connection_closed(client_context):
    """cursor() on a closed connection raises RuntimeError synchronously."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            await conn.close()
            with pytest.raises(RuntimeError, match="Connection is closed"):
                conn.cursor()

    asyncio.run(run())


@pytest.mark.integration
def test_cursor_can_be_created_multiple_times(client_context):
    """Per module invariant, a connection may issue multiple cursors; both
    share the same TdsClient and serialize on the same async mutex."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                cur1 = conn.cursor()
                cur2 = conn.cursor()
                assert cur1 is not cur2
                assert isinstance(cur1, mssql_py_core.PyAsyncCursor)
                assert isinstance(cur2, mssql_py_core.PyAsyncCursor)
            finally:
                await conn.close()

    asyncio.run(run())
