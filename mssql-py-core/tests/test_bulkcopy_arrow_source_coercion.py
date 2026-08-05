# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy source-coercion coverage (cursor.bulkcopy_arrow).

The per-type test files always pass a ``pyarrow.Table`` (the C-stream path).
These tests drive the other accepted source shapes that ``resolve_arrow_reader``
handles — a single ``RecordBatch``, a ``RecordBatchReader``, and an arbitrary
iterable of batches — plus the empty-iterable and unsupported-source error
arms.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


def _batch(ids):
    return pa.record_batch({"id": pa.array(ids, type=pa.int32())})


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_source_record_batch(client_context):
    """A single pyarrow.RecordBatch is accepted as a source."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSrcBatch"
    cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL)")

    result = cursor.bulkcopy_arrow(
        table_name, _batch([1, 2, 3]), batch_size=1000, timeout=30
    )
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id FROM {table_name} ORDER BY id")
    assert [r[0] for r in cursor.fetchall()] == [1, 2, 3]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_source_record_batch_reader(client_context):
    """A pyarrow.RecordBatchReader is accepted as a source."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSrcReader"
    cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL)")

    batch = _batch([10, 20])
    reader = pa.RecordBatchReader.from_batches(batch.schema, [batch])

    result = cursor.bulkcopy_arrow(table_name, reader, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT id FROM {table_name} ORDER BY id")
    assert [r[0] for r in cursor.fetchall()] == [10, 20]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_source_iterable_of_batches(client_context):
    """An arbitrary iterable of RecordBatches is accepted as a source."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSrcIterable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL)")

    batches = [_batch([1, 2]), _batch([3, 4])]

    result = cursor.bulkcopy_arrow(table_name, batches, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 4

    cursor.execute(f"SELECT id FROM {table_name} ORDER BY id")
    assert [r[0] for r in cursor.fetchall()] == [1, 2, 3, 4]

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_source_empty_iterable_raises(client_context):
    """An empty iterable has no schema to infer and must raise TypeError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSrcEmpty"
    cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL)")

    with pytest.raises(TypeError, match="(?i)empty"):
        cursor.bulkcopy_arrow(table_name, [], batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_source_unsupported_raises(client_context):
    """A non-Arrow, non-iterable source must raise TypeError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowSrcUnsupported"
    cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL)")

    with pytest.raises(TypeError):
        cursor.bulkcopy_arrow(table_name, 42, batch_size=1000, timeout=30)

    conn.close()
