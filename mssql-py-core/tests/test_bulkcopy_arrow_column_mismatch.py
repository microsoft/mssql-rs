# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for column mapping / count-mismatch scenarios.

Mirrors test_bulkcopy_column_mismatch.py for cursor.bulkcopy_arrow. Adds the
Arrow-specific by-name mapping case (mappings match Arrow field names).

Behavior parity with the tuple path:
  * Source has more columns than mapped  -> extra columns dropped.
  * Source has fewer columns than table  -> unmapped columns get NULL/DEFAULT.
  * No mappings -> auto zip-by-ordinal up to min(arrow_fields, dest_columns).
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_bulkcopy_arrow_more_columns_than_table(client_context):
    """Source has more columns than the table; extras dropped via mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowMoreColumns"
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT PRIMARY KEY, value1 INT, value2 INT)"
    )

    source = pa.table(
        {
            "id": pa.array([1, 2, 3, 4], type=pa.int32()),
            "value1": pa.array([100, 200, 300, 400], type=pa.int32()),
            "value2": pa.array([30, 25, 35, 28], type=pa.int32()),
            "extra1": pa.array([999, 999, 999, 999], type=pa.int32()),
            "extra2": pa.array([888, 888, 888, 888], type=pa.int32()),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "value1"), (2, "value2")],
    )
    assert result["rows_copied"] == 4

    cursor.execute(f"SELECT id, value1, value2 FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0] == (1, 100, 30)
    assert rows[3] == (4, 400, 28)

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_fewer_columns_than_table(client_context):
    """Source has fewer columns; unmapped nullable column receives NULL."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowFewerColumns"
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT PRIMARY KEY, value1 INT, value2 INT NULL)"
    )

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "value1": pa.array([100, 200, 300], type=pa.int32()),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "value1")],
    )
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value1, value2 FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0] == (1, 100, None)
    assert rows[2] == (3, 300, None)

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_auto_mapping_with_extra_columns(client_context):
    """Auto-mapping uses the first N Arrow fields; extra fields ignored."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowAutoMapExtra"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value1 INT, value2 INT)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "value1": pa.array([100, 200, 300], type=pa.int32()),
            "value2": pa.array([30, 25, 35], type=pa.int32()),
            "extra": pa.array([777, 777, 777], type=pa.int32()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value1, value2 FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0] == (1, 100, 30)
    assert rows[2] == (3, 300, 35)

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_auto_mapping_fewer_columns(client_context):
    """Auto-mapping zips only the available Arrow fields; rest get NULL."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowAutoFewer"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value1 INT, value2 INT NULL)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "value1": pa.array([100, 200, 300], type=pa.int32()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value1, value2 FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0] == (1, 100, None)
    assert rows[2] == (3, 300, None)

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_partial_column_mapping(client_context):
    """Explicit mapping may skip a middle column, which receives NULL."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowPartialMap"
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT PRIMARY KEY, value1 INT NULL, value2 INT)"
    )

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "v2": pa.array([30, 25, 35], type=pa.int32()),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "value2")],  # value1 skipped
    )
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, value1, value2 FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0] == (1, None, 30)
    assert rows[2] == (3, None, 35)

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_column_mappings_by_name(client_context):
    """Arrow-specific: by-name mappings match Arrow field names, any order."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowMapByName"
    cursor.execute(f"CREATE TABLE {table_name} (a INT, b NVARCHAR(20) NULL)")

    # Arrow field names are "x" and "y"; map them positionally to a, b.
    source = pa.table(
        {
            "x": pa.array([1, 2], type=pa.int32()),
            "y": pa.array(["p", "q"]),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name, source, column_mappings=["a", "b"], batch_size=1000, timeout=30
    )
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT a, b FROM {table_name} ORDER BY a")
    rows = cursor.fetchall()
    assert rows[0][1] == "p" and rows[1][1] == "q"

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_with_default_values(client_context):
    """Unmapped columns with DEFAULT constraints use the defaults."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowDefaults"
    cursor.execute(
        f"""CREATE TABLE {table_name} (
            id INT PRIMARY KEY,
            value1 INT,
            status INT DEFAULT 999,
            counter INT DEFAULT 0
        )"""
    )

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "value1": pa.array([100, 200, 300], type=pa.int32()),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "value1")],
    )
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT status, counter FROM {table_name}")
    for row in cursor.fetchall():
        assert row[0] == 999 and row[1] == 0

    conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_empty_source(client_context):
    """An empty Arrow table copies zero rows without error."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowEmpty"
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT PRIMARY KEY, value1 INT, value2 INT)"
    )

    source = pa.table(
        {
            "id": pa.array([], type=pa.int32()),
            "value1": pa.array([], type=pa.int32()),
            "value2": pa.array([], type=pa.int32()),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 0

    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    assert cursor.fetchall()[0][0] == 0

    conn.close()
