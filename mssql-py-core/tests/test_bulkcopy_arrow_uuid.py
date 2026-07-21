# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for UNIQUEIDENTIFIER data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_uuid.py. Arrow ``fixed_size_binary(16)`` maps to SQL
``UNIQUEIDENTIFIER``. The 16 bytes are the UUID's raw big-endian
(RFC 4122) representation, i.e. ``uuid.UUID.bytes``.
"""
import uuid

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_uuid_basic(client_context):
    """Arrow fixed_size_binary(16) bulkcopy into a UNIQUEIDENTIFIER column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowUuidTest"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, uuid_col UNIQUEIDENTIFIER)")

    u1 = uuid.UUID("6F9619FF-8B86-D011-B42D-00C04FC964FF")
    u2 = uuid.UUID("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11")
    u3 = uuid.UUID("00000000-0000-0000-0000-000000000000")  # NIL UUID
    u4 = uuid.uuid4()

    source = pa.table(
        {
            "id": pa.array([1, 2, 3, 4], type=pa.int32()),
            "uuid_col": pa.array(
                [u1.bytes, u2.bytes, u3.bytes, u4.bytes], type=pa.binary(16)
            ),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "uuid_col")],
    )

    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, uuid_col FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == u1
    assert rows[1][1] == u2
    assert rows[2][1] == u3
    assert rows[3][1] == u4

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_uuid_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableUuid"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, uuid_col UNIQUEIDENTIFIER)")

    u1 = uuid.uuid4()
    u3 = uuid.uuid4()
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "uuid_col": pa.array([u1.bytes, None, u3.bytes], type=pa.binary(16)),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, uuid_col FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == u1
    assert rows[1][1] is None
    assert rows[2][1] == u3

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_uuid_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable UNIQUEIDENTIFIER column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableUuid"
    cursor.execute(f"CREATE TABLE {table_name} (uuid_col UNIQUEIDENTIFIER NOT NULL)")

    u1 = uuid.uuid4()
    source = pa.table(
        {"uuid_col": pa.array([u1.bytes, None, u1.bytes], type=pa.binary(16))}
    )

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
