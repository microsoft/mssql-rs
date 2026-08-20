# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for PyAsyncCursor registration, creation, and execution."""

import asyncio
import datetime
import uuid
import warnings
from decimal import Decimal

import pytest
import mssql_py_core


async def connect(client_context, *, autocommit=True):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return await mssql_py_core.PyAsyncConnection.connect(
            client_context, autocommit=autocommit
        )


@pytest.mark.integration
def test_execute_returns_same_cursor(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            result = await cursor.execute("SET NOCOUNT ON")
            assert result is cursor
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_drains_same_cursor_previous_results(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT 1")
            assert await cursor.execute("SET NOCOUNT ON") is cursor
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_rejects_another_cursor_while_results_are_pending(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            first = conn.cursor()
            second = conn.cursor()
            await first.execute("SELECT 1")
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await second.execute("SELECT 2")
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_after_connection_close_raises(client_context):
    async def run():
        conn = await connect(client_context)
        cursor = conn.cursor()
        await conn.close()
        with pytest.raises(RuntimeError, match="Connection is closed"):
            await cursor.execute("SELECT 1")

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_execute_binds_positional_parameters(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            assert (
                await cursor.execute(
                    "SELECT CAST(? AS int)",
                    42,
                    use_prepare=use_prepare,
                )
                is cursor
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_binds_named_parameters(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            assert (
                await cursor.execute(
                    "SELECT N'東京', %(café)s, %(café)s",
                    {"café": "named"},
                )
                is cursor
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("container", [(10, 20), [10, 20]])
def test_execute_unwraps_single_positional_container(client_context, container):
    async def run():
        conn = await connect(client_context)
        try:
            await conn.cursor().execute(
                "IF ? + ? <> 30 THROW 50000, 'Unexpected positional values', 1",
                container,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_binds_bytearray_as_single_parameter(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            await conn.cursor().execute(
                "IF ? <> 0x010203 THROW 50000, 'Unexpected binary value', 1",
                bytearray([1, 2, 3]),
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_ignores_markers_in_quoted_contexts(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            await conn.cursor().execute(
                """
                SELECT '?', [q?mark], "q?mark"
                FROM (VALUES (CAST(%(value)s AS int))) AS source([q?mark])
                WHERE source.[q?mark] = %(value)s -- ignored ?
                  AND 'ignored %(value)s' = 'ignored %(value)s'
                  /* ignored ? and %(value)s */
                """,
                {"value": 42},
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_ignores_markers_in_nested_block_comments(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            await conn.cursor().execute(
                "SELECT ? /* outer /* inner */ ignored ? still outer */",
                42,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize(
    ("operation", "parameters", "message"),
    [
        ("SELECT ?", {"value": 1}, "positional placeholders"),
        ("SELECT %(value)s", (1,), "named placeholders"),
        ("SELECT ?, ?", (1,), "2 parameter markers, but 1 parameters"),
        ("SELECT ?", (1, 2), "1 parameter markers, but 2 parameters"),
    ],
)
def test_execute_rejects_parameter_style_and_arity_mismatches(
    client_context, operation, parameters, message
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match=message):
                cursor.execute(operation, parameters)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_rejects_missing_named_parameter(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(KeyError, match="name"):
                cursor.execute(
                    "SELECT %(id)s, %(name)s",
                    {"id": 1},
                )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_ignores_extra_named_parameters(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            await conn.cursor().execute(
                "IF %(value)s <> 42 THROW 50000, 'Unexpected named value', 1",
                {"value": 42, "extra": "ignored"},
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_rejects_unsupported_parameter_value(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match="Unsupported Python type"):
                cursor.execute("SELECT ?", {1, 2, 3})
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize(
    ("sizes", "message"),
    [
        ([(999999, 0, 0)], "Invalid SQL type"),
        ([(4, 0, 0, 0)], "must contain"),  # SQL_INTEGER
        ([(3, 39, 0)], "precision/scale"),  # SQL_DECIMAL
        ([(3, 10, 11)], "precision/scale"),
    ],
)
def test_setinputsizes_rejects_invalid_hints(client_context, sizes, message):
    async def run():
        conn = await connect(client_context)
        try:
            with pytest.raises(ValueError, match=message):
                conn.cursor().setinputsizes(sizes)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_setinputsizes_survives_synchronous_binding_failure(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-9, 20, 0)])  # SQL_WVARCHAR
            with pytest.raises(TypeError, match="parameter markers"):
                cursor.execute("SELECT ?, ?", "only one")

            await cursor.execute(
                """
                IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> 'nvarchar'
                    THROW 50000, 'Input size was consumed by failed binding', 1
                """,
                "ascii",
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_setinputsizes_survives_asynchronous_execution_failure(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-9, 20, 0)])  # SQL_WVARCHAR
            with pytest.raises(RuntimeError, match="expected failure"):
                await cursor.execute("THROW 50000, 'expected failure', 1; SELECT ?", "ascii")

            await cursor.execute(
                """
                IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> 'nvarchar'
                    THROW 50000, 'Input size was consumed by failed execute', 1
                """,
                "ascii",
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
@pytest.mark.parametrize(
    ("value", "expected_type"),
    [
        (0, "tinyint"),
        (-1, "smallint"),
        (32768, "int"),
        (2147483648, "bigint"),
        ("ascii", "varchar"),
        ("caf\u00e9", "nvarchar"),
        (datetime.datetime(2026, 8, 20, 12, 34, 56, 123456), "datetime2"),
        (
            datetime.datetime(
                2026,
                8,
                20,
                12,
                34,
                56,
                123456,
                tzinfo=datetime.timezone(datetime.timedelta(hours=5, minutes=30)),
            ),
            "datetimeoffset",
        ),
        (datetime.time(12, 34, 56, 123456), "time"),
    ],
)
def test_execute_infers_parameter_sql_type(
    client_context, use_prepare, value, expected_type
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                IF CONVERT(sysname, SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType')) <> ?
                    THROW 50000, 'Unexpected inferred parameter type', 1
                """,
                value,
                expected_type,
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_execute_infers_decimal_precision_and_scale(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                DECLARE @value sql_variant = CAST(? AS sql_variant)
                IF SQL_VARIANT_PROPERTY(@value, 'BaseType') <> 'numeric'
                    OR SQL_VARIANT_PROPERTY(@value, 'Precision') <> 7
                    OR SQL_VARIANT_PROPERTY(@value, 'Scale') <> 4
                    THROW 50000, 'Unexpected decimal parameter metadata', 1
                """,
                Decimal("123.4500"),
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_execute_preserves_datetimeoffset(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            value = datetime.datetime(
                2026,
                8,
                20,
                12,
                34,
                56,
                123456,
                tzinfo=datetime.timezone(datetime.timedelta(hours=5, minutes=30)),
            )
            await cursor.execute(
                """
                IF DATEPART(TZOFFSET, CAST(? AS datetimeoffset)) <> 330
                    THROW 50000, 'Unexpected datetime offset', 1
                """,
                value,
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_setinputsizes_binds_typed_null(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-3, 100, 0)])  # SQL_VARBINARY
            await cursor.execute(
                "IF ? + 0x01 IS NOT NULL THROW 50000, 'Expected NULL', 1",
                None,
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_setinputsizes_is_consumed_after_successful_execute(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-9, 20, 0)])  # SQL_WVARCHAR
            await cursor.execute(
                """
                IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> 'nvarchar'
                    THROW 50000, 'Expected hinted nvarchar', 1
                """,
                "ascii",
                use_prepare=use_prepare,
            )
            await cursor.execute(
                """
                IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> 'varchar'
                    THROW 50000, 'Expected inferred varchar', 1
                """,
                "ascii",
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_setinputsizes_binds_xml(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([mssql_py_core.SQL_XML])
            await cursor.execute(
                "DECLARE @value xml = ?; IF @value.exist('/root') <> 1 THROW 50000, 'Invalid XML', 1",
                "<root />",
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_setinputsizes_binds_json(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([mssql_py_core.SQL_JSON])
            await cursor.execute(
                "IF JSON_VALUE(?, '$.answer') <> 42 THROW 50000, 'Invalid JSON', 1",
                ({"answer": 42},),
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
def test_setinputsizes_binds_vector(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes(
                [(mssql_py_core.SQL_VECTOR, 3, 0), (mssql_py_core.SQL_VECTOR, 3, 0)]
            )
            await cursor.execute(
                "IF VECTOR_DISTANCE('euclidean', ?, ?) <> 0 THROW 50000, 'Invalid VECTOR', 1",
                ([1.0, 2.0, 3.0], [1.0, 2.0, 3.0]),
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize(
    ("sql_type", "expected_type"),
    [
        (mssql_py_core.SQL_MONEY, "money"),
        (mssql_py_core.SQL_SMALLMONEY, "smallmoney"),
    ],
)
def test_setinputsizes_binds_money(client_context, sql_type, expected_type):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([sql_type])
            await cursor.execute(
                """
                IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> ?
                    THROW 50000, 'Unexpected money type', 1
                """,
                Decimal("123.4500"),
                expected_type,
            )
        finally:
            await conn.close()

    asyncio.run(run())


def test_table_valued_parameter_surface_and_validation():
    tvp = mssql_py_core.TableValuedParameter(
        "dbo.OrderLinesType",
        [(4, 0, 0), (-9, 50, 0)],
        [(1, "first"), (2, None)],
    )
    assert tvp.schema == "dbo"
    assert tvp.type_name == "OrderLinesType"
    assert tvp.column_count == 2
    assert tvp.row_count == 2
    assert tvp.is_null is False

    null_tvp = mssql_py_core.TableValuedParameter("dbo.OrderLinesType")
    assert null_tvp.is_null is True
    assert null_tvp.column_count == 0
    assert null_tvp.row_count == 0

    with pytest.raises(ValueError, match="requires column definitions"):
        mssql_py_core.TableValuedParameter("dbo.OrderLinesType", rows=[(1,)])
    with pytest.raises(ValueError, match="has 1 values but 2 columns"):
        mssql_py_core.TableValuedParameter(
            "dbo.OrderLinesType",
            [(4, 0, 0), (-9, 50, 0)],
            [(1,)],
        )
    with pytest.raises(ValueError, match="either in type_name or with schema"):
        mssql_py_core.TableValuedParameter(
            "dbo.OrderLinesType", schema="other_schema"
        )
    with pytest.raises(ValueError, match="must be 'TypeName' or 'schema.TypeName'"):
        mssql_py_core.TableValuedParameter("database.dbo.OrderLinesType")
    with pytest.raises(ValueError, match="must not contain ']'"):
        mssql_py_core.TableValuedParameter("dbo.Order]LinesType")


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
@pytest.mark.parametrize("rows", [[(1, "first"), (2, "second")], [], None])
def test_execute_binds_table_valued_parameter(client_context, use_prepare, rows):
    async def run():
        conn = await connect(client_context)
        type_name = f"PyAsyncTvp_{uuid.uuid4().hex}"
        qualified_type_name = f"dbo.{type_name}"
        cursor = conn.cursor()
        try:
            await cursor.execute(
                f"CREATE TYPE dbo.[{type_name}] AS TABLE (id INT, value NVARCHAR(50))"
            )
            tvp = mssql_py_core.TableValuedParameter(
                qualified_type_name,
                [(4, 0, 0), (-9, 50, 0)] if rows is not None else None,
                rows,
            )
            expected_count = 0 if rows is None else len(rows)
            await cursor.execute(
                f"""
                IF (SELECT COUNT(*) FROM ?) <> {expected_count}
                    THROW 50000, 'Unexpected TVP row count', 1
                """,
                tvp,
                use_prepare=use_prepare,
            )
        finally:
            try:
                await cursor.execute(f"DROP TYPE IF EXISTS dbo.[{type_name}]")
            finally:
                await conn.close()

    asyncio.run(run())


@pytest.mark.integration
def test_execute_reuses_prepared_statement_when_reset_cursor_is_false(client_context):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            sql = "SELECT CAST(? AS int)"
            await cursor.execute(sql, 1)
            assert await cursor.execute(sql, 2, reset_cursor=False) is cursor
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("reset_cursor", [True, False])
def test_execute_reprepares_when_parameter_metadata_changes(client_context, reset_cursor):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            sql = "IF LEN(?) <> ? THROW 50000, 'parameter metadata was stale', 1"
            await cursor.execute(sql, "a", 1, reset_cursor=reset_cursor)
            await cursor.execute(sql, "abcdef", 6, reset_cursor=reset_cursor)
        finally:
            await conn.close()

    asyncio.run(run())


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
    assert hasattr(mssql_py_core.PyAsyncCursor, "close")
    assert hasattr(mssql_py_core, "TableValuedParameter")
    assert mssql_py_core.SQL_XML == 241
    assert mssql_py_core.SQL_JSON == 244
    assert mssql_py_core.SQL_VECTOR == 245


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
def test_cursor_snapshots_connection_timeout(client_context):
    async def run():
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            conn = await mssql_py_core.PyAsyncConnection.connect(client_context)
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
