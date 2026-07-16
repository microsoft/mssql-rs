# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy rejection tests for SQL types outside the v1 matrix.

Most SQL types that ``bulkcopy`` supports are now also supported by
``bulkcopy_arrow`` (money, smallmoney, datetime, smalldatetime, xml, json have
their own per-type test files). The types below remain outside the Arrow v1
matrix and must fail fast — the mapping raises rather than silently coercing:

  * ``SQL_VARIANT`` — the engine has no columnar value representation for it.
  * tz-aware timestamp → legacy ``DATETIME`` would drop the offset (rejected).
"""
import datetime

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_bulkcopy_arrow_sql_variant_rejected(client_context):
    """Arrow -> SQL_VARIANT must fail fast (no columnar variant representation)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowUnsupportedVariant"
    cursor.execute(f"CREATE TABLE {table_name} (c SQL_VARIANT NULL)")

    source = pa.table({"c": pa.array([1], type=pa.int32())})

    with pytest.raises(ValueError, match="(?i)not supported"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_tz_aware_timestamp_to_datetime_rejected(client_context):
    """A tz-aware Arrow timestamp is rejected for a legacy DATETIME column.

    tz-naive timestamps now map to DATETIME/SMALLDATETIME, but a tz-aware value
    would silently drop its offset, so it is rejected and steered toward
    datetimeoffset (mirrors the tz-aware -> DATETIME2 rejection).
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowDatetimeTzReject"
    cursor.execute(f"CREATE TABLE {table_name} (ts DATETIME NULL)")

    ts = pa.array(
        [datetime.datetime(2020, 1, 1, tzinfo=datetime.timezone.utc)],
        type=pa.timestamp("us", tz="UTC"),
    )
    source = pa.table({"ts": ts})

    with pytest.raises(ValueError, match="(?i)datetimeoffset"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
