# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for asynchronous DB-API cursor result-set metadata."""

import asyncio
import datetime
import inspect
import uuid
import warnings
from decimal import Decimal

import mssql_py_core
import pytest


async def connect(client_context):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await mssql_py_core.PyAsyncConnection.connect(
            client_context, autocommit=True
        )


def test_description_is_exposed_read_only_and_initially_none(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            assert cursor.description is None
            with pytest.raises(AttributeError):
                cursor.description = []
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_description_matches_sync_cursor_contract(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                "SELECT database_id, name FROM sys.databases", use_prepare=False
            )
            assert cursor.description == [
                ("database_id", int, None, 10, 10, 0, False),
                ("name", str, None, 128, 128, 0, False),
            ]
            assert cursor.description is cursor.description
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_description_type_size_precision_scale_and_nullability(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                SELECT
                    CAST(NULL AS bit) AS bit_value,
                    CAST(NULL AS tinyint) AS tinyint_value,
                    CAST(NULL AS smallint) AS smallint_value,
                    CAST(NULL AS int) AS int_value,
                    CAST(NULL AS bigint) AS bigint_value,
                    CAST(NULL AS real) AS real_value,
                    CAST(NULL AS float) AS float_value,
                    CAST(NULL AS decimal(38, 7)) AS decimal_value,
                    CAST(NULL AS money) AS money_value,
                    CAST(NULL AS smallmoney) AS smallmoney_value,
                    CAST(NULL AS nvarchar(100)) AS text_value,
                    CAST(NULL AS varchar(max)) AS max_text_value,
                    CAST(NULL AS varbinary(40)) AS binary_value,
                    CAST(NULL AS varbinary(max)) AS max_binary_value,
                    CAST(NULL AS date) AS date_value,
                    CAST(NULL AS time(6)) AS time_value,
                    CAST(NULL AS datetime) AS datetime_value,
                    CAST(NULL AS smalldatetime) AS smalldatetime_value,
                    CAST(NULL AS datetime2(6)) AS datetime2_value,
                    CAST(NULL AS datetimeoffset(6)) AS offset_value,
                    CAST(NULL AS uniqueidentifier) AS guid_value,
                    CAST(NULL AS xml) AS xml_value
                """,
                use_prepare=False,
            )

            description = cursor.description
            assert description is not None
            assert [column[1] for column in description] == [
                bool,
                int,
                int,
                int,
                int,
                float,
                float,
                Decimal,
                Decimal,
                Decimal,
                str,
                str,
                bytes,
                bytes,
                datetime.date,
                datetime.time,
                datetime.datetime,
                datetime.datetime,
                datetime.datetime,
                datetime.datetime,
                uuid.UUID,
                str,
            ]
            assert all(inspect.isclass(column[1]) for column in description)
            assert [column[3:] for column in description] == [
                (1, 1, 0, True),
                (3, 3, 0, True),
                (5, 5, 0, True),
                (10, 10, 0, True),
                (19, 19, 0, True),
                (7, 7, 0, True),
                (15, 15, 0, True),
                (38, 38, 7, True),
                (19, 19, 4, True),
                (10, 10, 4, True),
                (100, 100, 0, True),
                (0, 0, 0, True),
                (40, 40, 0, True),
                (0, 0, 0, True),
                (10, 10, 0, True),
                (15, 15, 6, True),
                (23, 23, 3, True),
                (16, 16, 0, True),
                (26, 26, 6, True),
                (33, 33, 6, True),
                (16, 16, 0, True),
                (0, 0, 0, True),
            ]
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_description_survives_fetch_empty_result_exhaustion_and_close(client_context):
    async def run():
        conn = await connect(client_context)
        cursor = conn.cursor()
        try:
            await cursor.execute(
                "SELECT CAST(1 AS int) AS value", use_prepare=False
            )
            description = cursor.description
            assert await cursor.fetchone() == (1,)
            assert await cursor.fetchone() is None
            assert cursor.description == description

            await cursor.execute(
                "SELECT CAST(1 AS int) AS value WHERE 1 = 0", use_prepare=False
            )
            description = cursor.description
            assert description == [("value", int, None, 10, 10, 0, True)]
            assert await cursor.fetchone() is None
            assert await cursor.fetchone() is None
            assert cursor.description == description
            await cursor.close()
            assert cursor.description == description
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_description_is_replaced_or_cleared_by_execute(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT 1 AS first_value", use_prepare=False)
            assert cursor.description[0][0] == "first_value"

            await cursor.execute("SELECT N'x' AS second_value", use_prepare=False)
            assert cursor.description[0][:2] == ("second_value", str)

            await cursor.execute("SET NOCOUNT ON", use_prepare=False)
            assert cursor.description is None

            await cursor.execute("SELECT 1 AS recovered", use_prepare=False)
            with pytest.raises(RuntimeError, match="Query execution failed"):
                await cursor.execute("SELECT * FROM __missing_async_description_table__")
            assert cursor.description is None
        finally:
            await conn.close()

    asyncio.run(run())
