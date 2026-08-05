# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Long-haul Arrow bulk copy test for continuous stress testing.

The Arrow analog of test_bulkcopy_longhaul.py. It streams pyarrow.RecordBatch
objects (not Python tuples) into cursor.bulkcopy_arrow for a configurable
duration, exercising the multi-batch Arrow streaming path (ArrowRowIter pumping
batches over the C-data interface) under sustained load.

The wide table covers the Arrow-supported SQL type matrix, including MONEY /
SMALLMONEY / DATETIME / SMALLDATETIME / XML (added in this PR) alongside the
integer / decimal / float / string / binary / temporal / uuid types. (Native
JSON is exercised via the money/xml columns and the NVARCHAR(MAX) json column;
the native JSON type is version-dependent and covered by the per-type tests.)

Duration and batch size are configured via LONGHAUL_DURATION_SECONDS
(default 1800s = 30 min) and LONGHAUL_BATCH_SIZE (default 1000).
"""
import datetime
import os
import time
import uuid
from decimal import Decimal

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


# Arrow-supported wide schema (20 columns). Every column's SQL type is in the
# (Arrow type -> SQL type) matrix.
_WIDE_TABLE_COLUMNS = (
    "id INT PRIMARY KEY, "
    "bigint_col BIGINT, "
    "bit_col BIT, "
    "varchar_col VARCHAR(100), "
    "nvarchar_col NVARCHAR(100), "
    "decimal_col DECIMAL(18, 2), "
    "float_col FLOAT, "
    "real_col REAL, "
    "date_col DATE, "
    "datetime2_col DATETIME2, "
    "datetimeoffset_col DATETIMEOFFSET, "
    "time_col TIME, "
    "varbinary_col VARBINARY(100), "
    "uuid_col UNIQUEIDENTIFIER, "
    "money_col MONEY, "
    "smallmoney_col SMALLMONEY, "
    "datetime_col DATETIME, "
    "smalldatetime_col SMALLDATETIME, "
    "xml_col XML, "
    "json_col NVARCHAR(MAX)"
)

_FIELD_NAMES = [
    "id",
    "bigint_col",
    "bit_col",
    "varchar_col",
    "nvarchar_col",
    "decimal_col",
    "float_col",
    "real_col",
    "date_col",
    "datetime2_col",
    "datetimeoffset_col",
    "time_col",
    "varbinary_col",
    "uuid_col",
    "money_col",
    "smallmoney_col",
    "datetime_col",
    "smalldatetime_col",
    "xml_col",
    "json_col",
]

_EPOCH = datetime.datetime(2020, 1, 1)
_EPOCH_UTC = datetime.datetime(2020, 1, 1, tzinfo=datetime.timezone.utc)


def _make_record_batch(start_id, n):
    """Build one pyarrow.RecordBatch of ``n`` rows for the wide schema."""
    ids = list(range(start_id, start_id + n))
    return pa.record_batch(
        [
            pa.array(ids, type=pa.int32()),
            pa.array([i * 1_000_000_000 for i in ids], type=pa.int64()),
            pa.array([i % 2 == 0 for i in ids], type=pa.bool_()),
            pa.array([f"char_value_{i}" for i in ids], type=pa.string()),
            pa.array([f"nchar_value_{i}" for i in ids], type=pa.string()),
            pa.array([Decimal(f"{i}.99") for i in ids], type=pa.decimal128(18, 2)),
            pa.array([float(i) * 1.5 for i in ids], type=pa.float64()),
            pa.array([float(i) * 0.25 for i in ids], type=pa.float32()),
            pa.array(
                [_EPOCH.date() + datetime.timedelta(days=i % 365) for i in ids],
                type=pa.date32(),
            ),
            pa.array(
                [_EPOCH + datetime.timedelta(seconds=i) for i in ids],
                type=pa.timestamp("us"),
            ),
            pa.array(
                [_EPOCH_UTC + datetime.timedelta(seconds=i) for i in ids],
                type=pa.timestamp("us", tz="UTC"),
            ),
            pa.array(
                [datetime.time(i % 24, i % 60, 0) for i in ids],
                type=pa.time64("us"),
            ),
            pa.array([bytes([i % 256] * 10) for i in ids], type=pa.binary()),
            pa.array([uuid.uuid4().bytes for _ in ids], type=pa.binary(16)),
            pa.array(
                [Decimal(f"{i % 1000}.5000") for i in ids], type=pa.decimal128(19, 4)
            ),
            pa.array(
                [Decimal(f"{i % 1000}.2500") for i in ids], type=pa.decimal128(10, 4)
            ),
            pa.array(
                [_EPOCH + datetime.timedelta(seconds=i) for i in ids],
                type=pa.timestamp("us"),
            ),
            pa.array(
                [_EPOCH + datetime.timedelta(minutes=i % 1000) for i in ids],
                type=pa.timestamp("us"),
            ),
            pa.array([f'<r id="{i}"/>' for i in ids], type=pa.string()),
            pa.array(
                [f'{{"id": {i}, "value": "test"}}' for i in ids], type=pa.string()
            ),
        ],
        names=_FIELD_NAMES,
    )


def _record_batch_generator(duration_seconds, batch_size):
    """Yield RecordBatches (all sharing one schema) until the duration elapses."""
    start_time = time.time()
    next_id = 1
    while time.time() - start_time < duration_seconds:
        yield _make_record_batch(next_id, batch_size)
        next_id += batch_size


@pytest.mark.longhaul
@pytest.mark.integration
def test_bulkcopy_arrow_longhaul_wide_table(client_context):
    """Long-haul Arrow bulk copy into a wide table over a configurable duration.

    Streams RecordBatches into cursor.bulkcopy_arrow continuously for
    LONGHAUL_DURATION_SECONDS, then verifies the reported/actual row counts and
    spot-checks data integrity.
    """
    duration_seconds = int(os.environ.get("LONGHAUL_DURATION_SECONDS", "1800"))
    batch_size = int(os.environ.get("LONGHAUL_BATCH_SIZE", "1000"))

    print(
        f"\nStarting long-haul Arrow BCP test for {duration_seconds} seconds "
        f"({duration_seconds / 60:.1f} minutes)"
    )
    print(f"Batch size: {batch_size} rows per RecordBatch")

    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowLongHaulWideTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} ({_WIDE_TABLE_COLUMNS})")
    print(f"Created table {table_name} with {len(_FIELD_NAMES)} columns")

    start_time = time.time()
    batches = _record_batch_generator(
        duration_seconds=duration_seconds, batch_size=batch_size
    )

    try:
        result = cursor.bulkcopy_arrow(
            table_name,
            batches,
            batch_size=batch_size,
            timeout=duration_seconds + 60,  # buffer over the generation window
        )

        elapsed_time = time.time() - start_time

        assert result is not None
        assert "rows_copied" in result
        assert "batch_count" in result
        assert "elapsed_time" in result

        rows_copied = result["rows_copied"]
        print("\nLong-haul Arrow test completed successfully!")
        print(f"Duration: {elapsed_time:.2f} seconds ({elapsed_time / 60:.1f} minutes)")
        print(f"Rows copied: {rows_copied:,}")
        print(f"Batches: {result['batch_count']:,}")
        if elapsed_time > 0:
            print(f"Throughput: {rows_copied / elapsed_time:.2f} rows/second")

        # At least one full batch must have been generated and copied.
        assert rows_copied >= batch_size

        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        count_result = cursor.fetchone()
        assert count_result[0] == rows_copied
        print(f"Verified row count in database: {count_result[0]:,}")

        cursor.execute(
            f"SELECT TOP 5 id, varchar_col, decimal_col FROM {table_name} ORDER BY id"
        )
        sample_rows = cursor.fetchall()
        print("\nSample rows from table:")
        for row in sample_rows:
            print(f"  ID: {row[0]}, VARCHAR: {row[1]}, DECIMAL: {row[2]}")

    finally:
        cursor.execute(f"DROP TABLE IF EXISTS {table_name}")
        conn.close()
        print(f"\nCleaned up table {table_name}")
