# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for XML data type (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_xml.py. Arrow ``utf8``/``large_utf8`` maps to SQL ``XML``
(sent as text; the server parses/validates it).
"""
import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_xml_basic(client_context):
    """Arrow utf8 bulkcopy into an XML column."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowTestTableXml"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, doc XML)")

    docs = [
        "<root><item>1</item></root>",
        "<a><b c=\"d\">text</b></a>",
        "<empty />",
    ]
    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "doc": pa.array(docs),
        }
    )

    result = cursor.bulkcopy_arrow(
        table_name,
        source,
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "doc")],
    )

    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, CAST(doc AS NVARCHAR(MAX)) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert "<item>1</item>" in rows[0][1]
    assert rows[2][1] in ("<empty />", "<empty/>")

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_xml_auto_mapping(client_context):
    """Arrow bulkcopy with automatic column mapping and NULL handling."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyArrowAutoMapTableXml"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, doc XML)")

    source = pa.table(
        {
            "id": pa.array([1, 2, 3], type=pa.int32()),
            "doc": pa.array(["<x>1</x>", None, "<z>3</z>"]),
        }
    )

    result = cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)
    assert result["rows_copied"] == 3

    cursor.execute(f"SELECT id, CAST(doc AS NVARCHAR(MAX)) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0][1] == "<x>1</x>"
    assert rows[1][1] is None
    assert rows[2][1] == "<z>3</z>"

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_xml_invalid_raises(client_context):
    """Malformed XML is rejected by the server."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowXmlInvalid"
    cursor.execute(f"CREATE TABLE {table_name} (doc XML NOT NULL)")

    source = pa.table({"doc": pa.array(["<unclosed>"])})

    with pytest.raises(Exception):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_arrow_xml_null_to_non_nullable_column(client_context):
    """A NULL value into a non-nullable XML column must raise ValueError."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BCArrowNonNullableXml"
    cursor.execute(f"CREATE TABLE {table_name} (doc XML NOT NULL)")

    source = pa.table({"doc": pa.array(["<a/>", None])})

    with pytest.raises(ValueError, match="(?i)non-nullable"):
        cursor.bulkcopy_arrow(table_name, source, batch_size=1000, timeout=30)

    conn.close()
