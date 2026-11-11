# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Live AQE Entra authentication validation.

Enable with AQE_ENTRA_TEST=1 and set SQL_SERVER and SQL_DATABASE. Service
Principal also requires AZURE_CLIENT_ID and AZURE_CLIENT_SECRET. Managed Identity
requires an Azure identity endpoint; MI_CLIENT_ID selects a user-assigned identity.
No token acquisition or credential construction is mocked.
"""

import asyncio
import warnings

import mssql_py_core
import pytest


@pytest.mark.integration
def test_builtin_entra_login_and_query(async_entra_live_context):
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            connection = await asyncio.wait_for(
                mssql_py_core.PyAsyncConnection.connect(
                    async_entra_live_context, autocommit=True
                ),
                40,
            )
        try:
            cursor = connection.cursor()
            try:
                await cursor.execute(
                    "SELECT CAST(1 AS int), DB_NAME(), ORIGINAL_LOGIN()",
                    use_prepare=False,
                )
                row = await cursor.fetchone()
                assert row is not None
                assert row[0] == 1
                assert row[1].casefold() == async_entra_live_context["database"].casefold()
                assert isinstance(row[2], str) and row[2]
                assert await cursor.fetchone() is None
            finally:
                await cursor.close()
        finally:
            await connection.close()
        assert connection.closed

    asyncio.run(run())
