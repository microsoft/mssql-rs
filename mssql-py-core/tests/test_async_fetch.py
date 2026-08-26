# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for asynchronous cursor row fetching."""

import asyncio
import warnings

import mssql_py_core
import pytest


async def connect(client_context):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await mssql_py_core.PyAsyncConnection.connect(
            client_context, autocommit=True
        )


def test_module_exposes_fetchone():
    assert hasattr(mssql_py_core.PyAsyncCursor, "fetchone")


def test_fetchone_without_result_set_raises(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(RuntimeError, match="No active result set"):
                await cursor.fetchone()
        finally:
            await conn.close()

    asyncio.run(run())


def test_fetchone_after_cursor_close_raises(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            await cursor.close()
            with pytest.raises(RuntimeError, match="Cursor is closed"):
                await cursor.fetchone()
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_returns_rows_and_releases_final_exhaustion(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute(
                "SELECT value FROM (VALUES (1), (2)) AS rows(value) ORDER BY value",
                use_prepare=False,
            )

            assert await owner.fetchone() == (1,)
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await other.execute("SELECT 3", use_prepare=False)
            assert await owner.fetchone() == (2,)
            assert await owner.fetchone() is None
            assert await owner.fetchone() is None

            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_empty_result_releases_finished_batch(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute("SELECT 1 WHERE 1 = 0", use_prepare=False)

            assert await owner.fetchone() is None
            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_fetchone_preserves_later_result_sets(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            owner = conn.cursor()
            other = conn.cursor()
            await owner.execute("SELECT 1; SELECT 2", use_prepare=False)

            assert await owner.fetchone() == (1,)
            assert await owner.fetchone() is None
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await other.execute("SELECT 3", use_prepare=False)

            await owner.close()
            await other.execute("SET NOCOUNT ON", use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())