# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for PyAsyncCursor registration, creation, and execution."""

import asyncio
import datetime
import gc
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


def test_execute_returns_same_cursor(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            result = await cursor.execute("SET NOCOUNT ON", use_prepare=False)
            assert result is cursor
        finally:
            await conn.close()

    asyncio.run(run())


def test_execute_drains_same_cursor_previous_results(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute("SELECT 1", use_prepare=False)
            assert await cursor.execute("SET NOCOUNT ON", use_prepare=False) is cursor
        finally:
            await conn.close()

    asyncio.run(run())


def test_execute_rejects_another_cursor_while_results_are_pending(mock_client_context):
    # TODO: Extend this to a multi-packet result, drain the first cursor to
    # exhaustion, and verify the second cursor can then execute. Closing the
    # first cursor is already covered by test_cursor_close_drains_results.
    async def run():
        conn = await connect(mock_client_context)
        try:
            first = conn.cursor()
            second = conn.cursor()
            await first.execute("SELECT 1", use_prepare=False)
            with pytest.raises(RuntimeError, match="busy with another cursor"):
                await second.execute("SELECT 2")
        finally:
            await conn.close()

    asyncio.run(run())


def test_execute_after_connection_close_raises(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
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


@pytest.mark.parametrize(
    ("operation", "parameters", "message"),
    [
        ("SELECT ?", {"value": 1}, "positional placeholders"),
        ("SELECT %(value)s", (1,), "named placeholders"),
        ("SELECT :value", {"value": 1}, "Named parameters use the %\\(name\\)s style"),
        ("SELECT %s", {"value": 1}, "Named parameters use the %\\(name\\)s style"),
        ("SELECT ?, ?", (1,), "2 parameter markers, but 1 parameters"),
        ("SELECT ?", (1, 2), "1 parameter markers, but 2 parameters"),
    ],
)
def test_execute_rejects_parameter_style_and_arity_mismatches(
    mock_client_context, operation, parameters, message
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match=message):
                cursor.execute(operation, parameters)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("operation", "message"),
    [
        ("SELECT ?", "1 parameter markers, but 0 parameters"),
        ("SELECT %(value)s", "named placeholders"),
    ],
)
def test_execute_rejects_markers_without_arguments(
    mock_client_context, operation, message
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match=message):
                cursor.execute(operation)
        finally:
            await conn.close()

    asyncio.run(run())


def test_execute_accepts_empty_mapping_without_markers(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            await conn.cursor().execute("SELECT 1", {}, use_prepare=False)
        finally:
            await conn.close()

    asyncio.run(run())


def test_execute_rejects_missing_named_parameter(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
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


def test_execute_rejects_unsupported_parameter_value(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            with pytest.raises(TypeError, match="Unsupported Python type"):
                cursor.execute("SELECT ?", {1, 2, 3})
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("sizes", "message"),
    [
        ([(999999, 0, 0)], "Invalid SQL type"),
        ([(4, 0, 0, 0)], "must contain"),  # SQL_INTEGER
        ([(3, 39, 0)], "precision/scale"),  # SQL_DECIMAL
        ([(3, 2, 3)], "precision/scale"),
        ([(3, 38, 39)], "precision/scale"),
        ([(3, 10, 11)], "precision/scale"),
        ([(93, 0, 8)], "Invalid temporal scale"),  # SQL_TYPE_TIMESTAMP
    ],
)
def test_setinputsizes_rejects_invalid_hints(mock_client_context, sizes, message):
    async def run():
        conn = await connect(mock_client_context)
        try:
            with pytest.raises(ValueError, match=message):
                conn.cursor().setinputsizes(sizes)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("operation", "parameters", "sizes", "message"),
    [
        ("SELECT ?, ?", (1, 2), [(4, 0, 0)], "1 hints, but.*2 parameter markers"),
        ("SELECT ?", (1,), [(4, 0, 0), (4, 0, 0)], "2 hints, but.*1 parameter markers"),
        (
            "SELECT %(value)s, %(value)s",
            {"value": 1},
            [(4, 0, 0)],
            "1 hints, but.*2 parameter markers",
        ),
        (
            "SELECT %(value)s",
            {"value": 1},
            [(4, 0, 0), (4, 0, 0)],
            "2 hints, but.*1 parameter markers",
        ),
    ],
)
def test_setinputsizes_rejects_hint_count_mismatch(
    mock_client_context, operation, parameters, sizes, message
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes(sizes)
            with pytest.raises(TypeError, match=message):
                cursor.execute(operation, parameters)
        finally:
            await conn.close()

    asyncio.run(run())


def test_setinputsizes_survives_synchronous_binding_failure(mock_client_context):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-151, 0, 0)])  # SQL_SS_UDT
            with pytest.raises(TypeError, match="parameter markers"):
                cursor.execute("SELECT ?, ?", "only one")

            with pytest.raises(TypeError, match="require a server UDT type name"):
                cursor.execute("SELECT ?", b"serialized-udt")
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
def test_setinputsizes_longvarchar_uses_varchar_max(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            statement = "DECLARE @length int = LEN(?)"
            for value in ("value", None):
                cursor.setinputsizes([(-1, 0, 0)])  # SQL_LONGVARCHAR
                await cursor.execute(statement, value, use_prepare=use_prepare)
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
@pytest.mark.parametrize(
    ("value", "precision", "scale"),
    [
        (Decimal("123.4500"), 7, 4),
        (Decimal("1E+3"), 4, 0),
        (Decimal("1E-3"), 3, 3),
    ],
)
def test_execute_infers_decimal_precision_and_scale(
    client_context, use_prepare, value, precision, scale
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            await cursor.execute(
                """
                DECLARE @value sql_variant = CAST(? AS sql_variant)
                IF SQL_VARIANT_PROPERTY(@value, 'BaseType') <> 'numeric'
                    OR SQL_VARIANT_PROPERTY(@value, 'Precision') <> ?
                    OR SQL_VARIANT_PROPERTY(@value, 'Scale') <> ?
                    THROW 50000, 'Unexpected decimal parameter metadata', 1
                """,
                value,
                precision,
                scale,
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
@pytest.mark.parametrize(
    ("offset", "expected_minutes"),
    [
        (datetime.timedelta(hours=-14), -840),
        (datetime.timedelta(hours=5, minutes=30), 330),
        (datetime.timedelta(hours=14), 840),
    ],
)
def test_execute_preserves_datetimeoffset(
    client_context, use_prepare, offset, expected_minutes
):
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
                tzinfo=datetime.timezone(offset),
            )
            await cursor.execute(
                f"""
                IF DATEPART(TZOFFSET, CAST(? AS datetimeoffset)) <> {expected_minutes}
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
@pytest.mark.parametrize("scale", [0, 3, 7])
@pytest.mark.parametrize(
    ("hint", "value", "expected_type"),
    [
        ((92, 0), datetime.time(12, 34, 56), "time"),
        ((93, 0), datetime.datetime(2026, 8, 20, 12, 34, 56), "datetime2"),
        (
            (-155, 0),
            datetime.datetime(
                2026, 8, 20, 12, 34, 56, tzinfo=datetime.timezone.utc
            ),
            "datetimeoffset",
        ),
    ],
)
def test_setinputsizes_preserves_temporal_scale(
    client_context, use_prepare, scale, hint, value, expected_type
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(hint[0], hint[1], scale)])
            await cursor.execute(
                f"""
                DECLARE @value sql_variant = CAST(? AS sql_variant)
                IF SQL_VARIANT_PROPERTY(@value, 'BaseType') <> '{expected_type}'
                    OR SQL_VARIANT_PROPERTY(@value, 'Scale') <> {scale}
                    THROW 50000, 'Unexpected temporal parameter metadata', 1
                """,
                value,
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize("use_prepare", [True, False])
@pytest.mark.parametrize("offset_seconds", [30, -30])
def test_execute_rejects_sub_minute_datetimeoffset(
    mock_client_context, use_prepare, offset_seconds
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            value = datetime.datetime(
                2026,
                8,
                20,
                tzinfo=datetime.timezone(datetime.timedelta(seconds=offset_seconds)),
            )
            with pytest.raises(TypeError, match="whole-minute UTC offset"):
                cursor.execute("SELECT ?", value, use_prepare=use_prepare)
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
def test_setinputsizes_uses_default_numeric_precision(client_context, use_prepare):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(3, 0, 2)])  # SQL_DECIMAL, default precision
            await cursor.execute(
                """
                DECLARE @value sql_variant = CAST(? AS sql_variant)
                IF SQL_VARIANT_PROPERTY(@value, 'BaseType') <> 'decimal'
                    OR SQL_VARIANT_PROPERTY(@value, 'Precision') <> 18
                    OR SQL_VARIANT_PROPERTY(@value, 'Scale') <> 2
                    THROW 50000, 'Unexpected decimal parameter metadata', 1
                """,
                Decimal("1.23"),
                use_prepare=use_prepare,
            )
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    ("hint", "message"),
    [
        ((3, 9, 2), "require precision 18 and scale 10"),
        ((92, 0, 3), "require scale 7"),
        ((93, 0, 3), "require scale 7"),
        ((-155, 0, 3), "require scale 7"),
    ],
)
def test_setinputsizes_rejects_unrepresentable_typed_null_metadata(
    mock_client_context, hint, message
):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([hint])
            with pytest.raises(TypeError, match=message):
                cursor.execute("SELECT ?", None)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.parametrize("value", [None, b"serialized-udt"])
def test_setinputsizes_rejects_udt_without_type_name(mock_client_context, value):
    async def run():
        conn = await connect(mock_client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([(-151, 0, 0)])  # SQL_SS_UDT
            with pytest.raises(TypeError, match="require a server UDT type name"):
                cursor.execute("SELECT ?", value)
        finally:
            await conn.close()

    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("use_prepare", [True, False])
@pytest.mark.parametrize(
    "hint",
    [
        (3, 18, 10),
        (92, 0, 7),
        (93, 0, 7),
        (-155, 0, 7),
    ],
)
def test_setinputsizes_accepts_representable_typed_null_metadata(
    client_context, use_prepare, hint
):
    async def run():
        conn = await connect(client_context)
        try:
            cursor = conn.cursor()
            cursor.setinputsizes([hint])
            await cursor.execute("SELECT ?", None, use_prepare=use_prepare)
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
                f"""
                IF SQL_VARIANT_PROPERTY(CAST(? AS sql_variant), 'BaseType') <> '{expected_type}'
                    THROW 50000, 'Unexpected money type', 1
                """,
                Decimal("123.4500"),
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
    # The py-core unit test pins the state-reuse decision; mssql-tds separately
    # asserts that a retained live handle emits sp_execute rather than sp_prepexec.
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
