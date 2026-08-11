# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Regression tests for bulkcopy row-count accuracy (issue #209).

`rows_copied` must equal the client-side count of rows serialized to the wire,
and `batch_count` must equal the number of batches derived from that count and
the batch size, on every engine. Distributed engines (Fabric Warehouse)
acknowledge one load with multiple DONE_COUNT tokens; the core must not sum them
into a doubled count.
"""
import pytest


@pytest.mark.integration
@pytest.mark.parametrize(
    "row_count,batch_size,expected_batches",
    [
        (1, 0, 1),
        (1, 1000, 1),
        (1000, 0, 1),
        (5000, 1000, 5),
    ],
)
def test_bulkcopy_rows_copied_matches_actual(
    connection, row_count, batch_size, expected_batches
):
    cursor = connection.cursor()

    table_name = "#BulkCopyRowsCopiedAccuracy"
    cursor.execute(f"CREATE TABLE {table_name} (id BIGINT)")

    data = [(i,) for i in range(row_count)]

    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=batch_size, timeout=60
    )

    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    actual = cursor.fetchone()[0]

    assert actual == row_count
    assert result["rows_copied"] == actual
    assert result["batch_count"] == expected_batches


@pytest.mark.integration
def test_bulkcopy_arrow_rows_copied_matches_actual(connection):
    pa = pytest.importorskip("pyarrow")

    cursor = connection.cursor()

    table_name = "#BulkCopyArrowRowsCopiedAccuracy"
    cursor.execute(f"CREATE TABLE {table_name} (id BIGINT)")

    row_count = 5000
    table = pa.table({"id": pa.array(list(range(row_count)), pa.int64())})

    result = cursor.bulkcopy_arrow(
        table_name, table, batch_size=1000, timeout=60
    )

    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    actual = cursor.fetchone()[0]

    assert actual == row_count
    assert result["rows_copied"] == actual
    assert result["batch_count"] == 5
