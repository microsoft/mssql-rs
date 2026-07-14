# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for batch_size and use_internal_transaction.

Mirrors test_bulkcopy_transactions.py for cursor.bulkcopy_arrow.

use_internal_transaction is a source-agnostic TDS behavior (each batch wrapped
in BEGIN/COMMIT), so it gets basic forwarding coverage here. batch_size is
Arrow-relevant and tested in full: the TDS commit cadence is independent of the
Arrow RecordBatch boundaries, so a single large Arrow batch still commits every
`batch_size` rows and reports the corresponding batch_count.
"""
import time

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


def unique_table_name(prefix: str) -> str:
    """Generate a unique table name with timestamp suffix."""
    return f"{prefix}_{int(time.time() * 1000)}"


# ── use_internal_transaction ─────────────────────────────────────────────────


@pytest.mark.integration
def test_bulkcopy_arrow_internal_transaction_true(client_context):
    """use_internal_transaction=True commits Arrow data successfully."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowInternalTxn")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        source = pa.table(
            {
                "id": pa.array([1, 2, 3], type=pa.int32()),
                "value": pa.array([100, 200, 300], type=pa.int32()),
            }
        )
        result = cursor.bulkcopy_arrow(table_name, source, use_internal_transaction=True)
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        assert cursor.fetchall()[0][0] == 3

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_internal_transaction_false_default(client_context):
    """Default autocommit (use_internal_transaction=False) commits Arrow data."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowAutocommit")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        source = pa.table(
            {
                "id": pa.array([1, 2, 3], type=pa.int32()),
                "value": pa.array([100, 200, 300], type=pa.int32()),
            }
        )
        result = cursor.bulkcopy_arrow(table_name, source)
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        assert cursor.fetchall()[0][0] == 3

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# ── batch_size (Arrow-relevant: decoupled from Arrow batch boundaries) ────────


@pytest.mark.integration
def test_bulkcopy_arrow_batch_size_splits_single_arrow_batch(client_context):
    """batch_size is honored independently of the Arrow RecordBatch layout.

    10 rows arrive as a single Arrow RecordBatch; with batch_size=3 the reported
    batch_count reflects the requested batch size (ceil(10/3) = 4), not the
    single incoming Arrow batch, and every row lands. (batch_count is derived
    from rows_affected and batch_size, so this asserts the reported value and
    full row delivery, not the on-wire commit boundaries, which aren't
    observable from the client.)
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowBatchSize")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL)")

        source = pa.table({"id": pa.array(list(range(1, 11)), type=pa.int32())})
        result = cursor.bulkcopy_arrow(table_name, source, batch_size=3)
        assert result["rows_copied"] == 10
        assert result["batch_count"] == 4  # ceil(10 / 3)

        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        assert cursor.fetchall()[0][0] == 10

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_batch_size_zero_single_batch(client_context):
    """batch_size=0 (server optimal) reports a single batch."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowBatchZero")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL)")

        source = pa.table({"id": pa.array(list(range(1, 6)), type=pa.int32())})
        result = cursor.bulkcopy_arrow(table_name, source, batch_size=0)
        assert result["rows_copied"] == 5
        assert result["batch_count"] == 1

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()
