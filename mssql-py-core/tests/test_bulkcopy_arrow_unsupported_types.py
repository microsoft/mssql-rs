# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy rejection tests for SQL types outside the v1 matrix.

These are the Arrow counterparts of the tuple-path type files (datetime,
smalldatetime, money, xml, variant, ...). For the tuple path those types load
positively; for bulkcopy_arrow they are intentionally **rejected** — the v1
type matrix (design doc §6.1/§6.2) fails fast on any unsupported
``(Arrow type, SQL type)`` pair rather than silently coercing.

Each case builds a plausible Arrow column for the destination and asserts the
operation raises with a clear "not supported" message naming the mismatch.
"""
import datetime
from decimal import Decimal

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


# (label, sql_column_type, pyarrow array factory) for destinations that are
# deliberately outside the v1 matrix.
_UNSUPPORTED_CASES = [
    ("datetime", "DATETIME", lambda: pa.array(
        [datetime.datetime(2020, 1, 1, 0, 0, 0)], type=pa.timestamp("us"))),
    ("smalldatetime", "SMALLDATETIME", lambda: pa.array(
        [datetime.datetime(2020, 1, 1, 0, 0, 0)], type=pa.timestamp("us"))),
    ("money", "MONEY", lambda: pa.array(
        [Decimal("12.3400")], type=pa.decimal128(19, 4))),
    ("smallmoney", "SMALLMONEY", lambda: pa.array(
        [Decimal("12.3400")], type=pa.decimal128(10, 4))),
    ("xml", "XML", lambda: pa.array(["<r/>"])),
    ("sql_variant", "SQL_VARIANT", lambda: pa.array([1], type=pa.int32())),
]


@pytest.mark.integration
@pytest.mark.parametrize(
    "label,sql_type,arr_factory",
    _UNSUPPORTED_CASES,
    ids=[c[0] for c in _UNSUPPORTED_CASES],
)
def test_bulkcopy_arrow_unsupported_destination_type_rejected(
    client_context, label, sql_type, arr_factory
):
    """Arrow -> unsupported SQL destination type must fail fast, not coerce."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = f"#BCArrowUnsupported_{label}"
    cursor.execute(f"CREATE TABLE {table_name} (c {sql_type} NULL)")

    source = pa.table({"c": arr_factory()})

    with pytest.raises(ValueError, match="(?i)not supported"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_tz_naive_timestamp_to_datetime_rejected(client_context):
    """Even a tz-naive Arrow timestamp is rejected for a DATETIME column.

    Only DATETIME2 (tz-naive) and DATETIMEOFFSET (tz-aware) are in the v1 matrix;
    legacy DATETIME is not. This complements the tz-aware -> DATETIME2 rejection.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowDatetimeReject"
    cursor.execute(f"CREATE TABLE {table_name} (ts DATETIME NULL)")

    source = pa.table(
        {"ts": pa.array([datetime.datetime(2020, 1, 1)], type=pa.timestamp("us"))}
    )

    with pytest.raises(ValueError, match="(?i)not supported"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
