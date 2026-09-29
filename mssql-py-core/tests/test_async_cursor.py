# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for asynchronous cursor registration, creation, and lifecycle."""

import asyncio
import gc
import warnings

import mssql_py_core
import pytest


async def connect(client_context, *, autocommit=True):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await mssql_py_core.PyAsyncConnection.connect(
            client_context, autocommit=autocommit
        )


@pytest.mark.integration
def test_cursor_close_drains_results_and_rejects_further_use(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT CAST(? AS int)", 1)
            assert await cursor.close() is None
            assert await cursor.close() is None
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await cursor.execute("SELECT 1")
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                cursor.setinputsizes([(4, 0, 0)])
        finally:
            await conn.close()

    asyncio.run(run())


def test_cursor_close_after_connection_close_is_noop(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        cursor = conn.cursor()
        await conn.close()

        assert await cursor.close() is None
        assert await cursor.close() is None
        with pytest.raises(RuntimeError, match="Cursor is closed"):
            await cursor.execute("SELECT 1")

    asyncio.run(run())


def test_cursor_close_without_execute_is_idempotent(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            assert await cursor.close() is None
            assert await cursor.close() is None
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await cursor.execute("SELECT 1")
        finally:
            await conn.close()

    asyncio.run(run())


def test_cursor_close_can_retry_after_another_cursor_releases_results(
    mock_client_context,
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            owner = conn.cursor()
            blocked = conn.cursor()
            await owner.execute("SELECT 1", use_prepare=False)

            with pytest.raises(RuntimeError, match="busy with another cursor"):
                blocked.close()

            await owner.close()
            assert await blocked.close() is None
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                blocked.setinputsizes([(4, 0, 0)])
        finally:
            await conn.close()

    asyncio.run(run())


def test_dropped_cursor_with_unread_rows_does_not_leave_connection_busy(
    mock_client_context,
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT 1", use_prepare=False)
            del cursor
            gc.collect()

            for _ in range(100):
                await asyncio.sleep(0.01)
                probe = conn.cursor()
                try:
                    await probe.execute("SET NOCOUNT ON", use_prepare=False)
                except RuntimeError as error:
                    if "busy" in str(error).lower():
                        continue
                    assert "broken" in str(error).lower()
                    break
                else:
                    await probe.close()
                    break
            else:
                pytest.fail("Dropped cursor left the connection permanently busy")
        finally:
            await conn.close()

    asyncio.run(run())


def test_dropped_idle_cursor_does_not_break_another_cursor(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            idle = conn.cursor()
            await idle.execute("SET NOCOUNT ON", use_prepare=False)

            owner = conn.cursor()
            await owner.execute("SELECT 1", use_prepare=False)

            del idle
            gc.collect()

            await owner.close()
            probe = conn.cursor()
            await probe.execute("SET NOCOUNT ON", use_prepare=False)
            await probe.close()
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("transaction_method", ["commit", "rollback"])
def test_transaction_operation_rejects_pending_cursor_results(
    client_context, transaction_method
):
    async def run():
        conn = await connect(client_context, autocommit=False)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT 1", use_prepare=False)
            with pytest.raises(RuntimeError, match="busy with another operation"):
                await getattr(conn, transaction_method)()
            await cursor.close()
            assert await getattr(conn, transaction_method)() is None
        finally:
            await conn.close()

    asyncio.run(run())


def test_module_exposes_pyasynccursor():
    """PyAsyncCursor is registered on the extension module."""
    assert hasattr(mssql_py_core, "PyAsyncCursor")
    assert hasattr(mssql_py_core.PyAsyncCursor, "setinputsizes")
    assert hasattr(mssql_py_core.PyAsyncCursor, "executemany")
    assert hasattr(mssql_py_core.PyAsyncCursor, "rowcount")
    assert hasattr(mssql_py_core.PyAsyncCursor, "close")
    assert hasattr(mssql_py_core, "TableValuedParameter")
    assert mssql_py_core.SQL_XML == 241
    assert mssql_py_core.SQL_JSON == 244
    assert mssql_py_core.SQL_VECTOR == 245


def test_async_api_exposes_user_facing_docstrings():
    cursor_doc = " ".join(mssql_py_core.PyAsyncCursor.__doc__.split())
    assert "only one cursor may own an active batch" in cursor_doc
    assert "row-producing execute retains ownership" in cursor_doc
    assert "`commit()` or `rollback()` report busy" in cursor_doc
    assert "Positional parameters use `?` markers" in mssql_py_core.PyAsyncCursor.execute.__doc__
    assert "consumed only after" in mssql_py_core.PyAsyncCursor.setinputsizes.__doc__
    close_doc = " ".join(mssql_py_core.PyAsyncCursor.close.__doc__.split())
    assert "Closing an already closed cursor is a no-op" in close_doc
    assert "table-valued parameter" in mssql_py_core.TableValuedParameter.__doc__


def test_two_cursors_can_be_created(mock_client_context):
    """A second cursor on the same connection is allowed (documented behavior)."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(mock_client_context)
            try:
                cur1 = conn.cursor()
                cur2 = conn.cursor()
                assert isinstance(cur1, mssql_py_core.PyAsyncCursor)
                assert isinstance(cur2, mssql_py_core.PyAsyncCursor)
                assert cur1 is not cur2
            finally:
                await conn.close()

    asyncio.run(run())


def test_cursor_snapshots_connection_timeout(mock_client_context):
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(mock_client_context)
            try:
                conn.timeout = 30
                first_cursor = conn.cursor()
                assert first_cursor.timeout == 30

                conn.timeout = 5
                assert first_cursor.timeout == 30
                assert conn.cursor().timeout == 5
            finally:
                await conn.close()

    asyncio.run(run())


def test_conn_cursor_after_close_raises_connection_closed(mock_client_context):
    """cursor() on a closed connection raises RuntimeError."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(mock_client_context)
            await conn.close()
            with pytest.raises(RuntimeError, match="Connection is closed"):
                conn.cursor()

    asyncio.run(run())
