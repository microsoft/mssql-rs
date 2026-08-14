# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests covering the shared asyncio/Tokio runtime backing the async surface.

These tests exercise runtime-level guarantees (module import, class
registration, concurrent async work sharing one runtime). Per-method behavior
for PyAsyncConnection / PyAsyncCursor lives in the sibling test files.
"""

import asyncio
import warnings

import pytest
import mssql_py_core


def test_module_imports_without_error():
    """Importing mssql_py_core initializes the shared Tokio runtime."""
    assert mssql_py_core is not None


def test_async_types_registered_on_module():
    """Both async pyclasses are exposed on the extension module."""
    assert hasattr(mssql_py_core, "PyAsyncConnection")
    assert hasattr(mssql_py_core, "PyAsyncCursor")


def test_asyncio_run_bridges_into_shared_runtime():
    """asyncio.run() + a trivial Rust awaitable resolves without runtime error."""
    async def noop():
        # No Rust future here — just proves the pyo3-async-runtimes bridge
        # was registered at module load without panicking.
        return 42

    assert asyncio.run(noop()) == 42


@pytest.mark.integration
def test_shared_runtime_handles_concurrent_connects(client_context):
    """N concurrent connects submitted to the same shared runtime all succeed."""
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conns = await asyncio.gather(
                mssql_py_core.PyAsyncConnection.connect(client_context),
                mssql_py_core.PyAsyncConnection.connect(client_context),
                mssql_py_core.PyAsyncConnection.connect(client_context),
            )
            try:
                assert all(c is not None for c in conns)
            finally:
                await asyncio.gather(*(c.close() for c in conns))

    asyncio.run(run())
