# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for NCHAR data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_nchar.py. Arrow ``utf8`` maps to SQL ``NCHAR``. NCHAR is
fixed-width and space-padded on read, so round-trip assertions use ``rstrip``.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_nchar_basic(client_context):
    """Arrow bulkcopy with nchar columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableNChar"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, code NCHAR(10))")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "code": pa.array(["AB", "CDE", "FghIJ"]),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "code")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, code FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1].rstrip() == "AB"
    assert rows[1][1].rstrip() == "CDE"
    assert rows[2][1].rstrip() == "FghIJ"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_nchar_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableNChar"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, code NCHAR(5))")

    source = pa.table(
        {
            "id": pa.array([1, 2, 4], type=pa.int32()),
            "code": pa.array(["AA", None, "CC"]),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, code FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1].rstrip() == "AA"
    assert rows[1][1] is None
    assert rows[2][1].rstrip() == "CC"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_nchar_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable NCHAR column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyArrowNonNullableNChar"
    cursor.execute(f"CREATE TABLE {table_name} (code NCHAR(5) NOT NULL)")

    source = pa.table({"code": pa.array(["AA", None, "CC"])})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
