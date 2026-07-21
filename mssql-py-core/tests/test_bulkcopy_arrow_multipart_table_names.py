# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for multipart table names (database.schema.table).

Mirrors test_bulkcopy_multipart_table_names.py for cursor.bulkcopy_arrow. Table
name parsing is source-agnostic, but these cases are cheap and confirm the
Arrow entry point forwards the name correctly.
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


def _source():
    return pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "value": pa.array([100, 200, 300], type=pa.int32()),
        }
    )


@pytest.mark.integration
def test_bulkcopy_arrow_one_part_table_name(client_context):
    """One-part table name (Table)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "ArrowTestTable1Part"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value INT)")

    result = cursor.bulkcopy_arrow(table_name, _source(), batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0] == (1, 100) and rows[2] == (3, 300)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_two_part_table_name(client_context):
    """Two-part table name (schema.Table)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "dbo.ArrowTestTable2Part"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value INT)")

    result = cursor.bulkcopy_arrow(table_name, _source(), batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    assert cursor.fetchall()[0] == (1, 100)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_three_part_table_name(client_context):
    """Three-part table name (database.schema.Table)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    cursor.execute("SELECT DB_NAME()")
    current_db = cursor.fetchone()[0]
    conn.close()

    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    simple_name = "dbo.ArrowTestTable3Part"
    table_name = f"{current_db}.{simple_name}"
    cursor.execute(
        f"IF OBJECT_ID('{simple_name}', 'U') IS NOT NULL DROP TABLE {simple_name}"
    )
    cursor.execute(f"CREATE TABLE {simple_name} (id INT, value INT)")

    result = cursor.bulkcopy_arrow(table_name, _source(), batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT COUNT(*) FROM {simple_name}")
    assert cursor.fetchall()[0][0] == 3

    cursor.execute(f"DROP TABLE {simple_name}")
    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_bracketed_table_name(client_context):
    """Bracket-quoted multipart name ([schema].[Table])."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    simple_name = "dbo.ArrowTestTableBracketed"
    table_name = "[dbo].[ArrowTestTableBracketed]"
    cursor.execute(
        f"IF OBJECT_ID('{simple_name}', 'U') IS NOT NULL DROP TABLE {simple_name}"
    )
    cursor.execute(f"CREATE TABLE {simple_name} (id INT, value INT)")

    result = cursor.bulkcopy_arrow(table_name, _source(), batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT COUNT(*) FROM {simple_name}")
    assert cursor.fetchall()[0][0] == 3

    cursor.execute(f"DROP TABLE {simple_name}")
    conn.close()
