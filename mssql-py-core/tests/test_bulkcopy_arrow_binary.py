# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for BINARY / VARBINARY data types (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_binary.py. Arrow ``binary`` maps to SQL ``VARBINARY`` /
``BINARY``. BINARY(n) is fixed-width and right-padded with zero bytes on read.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_varbinary_basic(client_context):
    """Arrow binary bulkcopy into a VARBINARY column preserves length."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableVarBinary"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, data VARBINARY(100))")

    b4 = bytes([0x01, 0x02, 0x03, 0x04])
    b8 = bytes([0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x99, 0x88])
    source = pa.table(
        {
            "id": pa.array([1, 2], type=pa.int32()),
            "data": pa.array([b4, b8], type=pa.binary()),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "data")],
    )

    assert result["rows_copied"] == 2
    assert result["batch_count"] == 1

    cursor.execute(
        f"SELECT id, data, DATALENGTH(data) FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()
    assert rows[0][1] == b4 and rows[0][2] == 4
    assert rows[1][1] == b8 and rows[1][2] == 8

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_binary_fixed_width(client_context):
    """Arrow binary bulkcopy into a fixed-width BINARY(16) column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowBinaryFixed"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, data BINARY(16))")

    b16 = bytes(range(1, 17))
    source = pa.table(
        {
            "id": pa.array([1], type=pa.int32()),
            "data": pa.array([b16], type=pa.binary()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 1

    cursor.execute(f"SELECT data FROM {table_name}")
    assert cursor.fetchone()[0] == b16

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_binary_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableBinary"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, data VARBINARY(50))")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "data": pa.array([b"\x01\x02", None, b"\x0a\x0b\x0c"], type=pa.binary()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, data FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == b"\x01\x02"
    assert rows[1][1] is None
    assert rows[2][1] == b"\x0a\x0b\x0c"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_binary_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable VARBINARY column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableBinary"
    cursor.execute(f"CREATE TABLE {table_name} (data VARBINARY(50) NOT NULL)")

    source = pa.table({"data": pa.array([b"\x01", None, b"\x03"], type=pa.binary())})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_fixed_size_binary(client_context):
    """Arrow fixed_size_binary(N) (N != 16) loads into BINARY(N)/VARBINARY(N).

    Regression test: the planner must accept any fixed-size binary width for
    BINARY/VARBINARY, not only width 16.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowFixedSizeBinary"
    cursor.execute(f"CREATE TABLE {table_name} (b4 BINARY(4), vb4 VARBINARY(4))")

    values = [b"\x01\x02\x03\x04", b"\xaa\xbb\xcc\xdd"]
    source = pa.table(
        {
            "b4": pa.array(values, type=pa.binary(4)),
            "vb4": pa.array(values, type=pa.binary(4)),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "b4"), (1, "vb4")],
    )
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT b4, vb4 FROM {table_name} ORDER BY b4")
    rows = cursor.fetchall()
    assert rows[0][0] == b"\x01\x02\x03\x04" and rows[0][1] == b"\x01\x02\x03\x04"
    assert rows[1][0] == b"\xaa\xbb\xcc\xdd" and rows[1][1] == b"\xaa\xbb\xcc\xdd"

    conn.close()
