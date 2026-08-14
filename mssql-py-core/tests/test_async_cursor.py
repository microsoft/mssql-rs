# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for PyAsyncCursor: class registration and creation via PyAsyncConnection.

PyAsyncCursor is a preview scaffold in this PR — no execute/fetch methods yet.
These tests validate the surface that exists today: class existence, cursor()
creation, and closed-connection semantics.
"""

import asyncio
import warnings

import pytest
import mssql_py_core


def test_module_exposes_pyasynccursor():
    """PyAsyncCursor is registered on the extension module."""
    assert hasattr(mssql_py_core, "PyAsyncCursor")


@pytest.mark.integration
def test_conn_cursor_returns_pyasynccursor(client_context):
    """conn.cursor() is synchronous and returns a PyAsyncCursor instance."""
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
def test_two_cursors_can_be_created(client_context):
    """A second cursor on the same connection is allowed (documented behavior)."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            try:
                cur1 = conn.cursor()
                cur2 = conn.cursor()
                assert isinstance(cur1, mssql_py_core.PyAsyncCursor)
                assert isinstance(cur2, mssql_py_core.PyAsyncCursor)
                assert cur1 is not cur2
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_conn_cursor_after_close_raises_connection_closed(client_context):
    """cursor() on a closed connection raises RuntimeError."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
            await conn.close()
            with pytest.raises(RuntimeError, match="Connection is closed"):
                conn.cursor()

    asyncio.run(run())
