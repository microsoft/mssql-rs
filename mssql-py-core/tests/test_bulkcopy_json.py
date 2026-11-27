# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Bulk copy tests for JSON data type."""
import pytest
import mssql_py_core
import json


def test_snapshot_carrier_is_frozen_and_defers_encoding():
    carrier = mssql_py_core.BulkCopyJson("\ud800")
    with pytest.raises(AttributeError):
        carrier.text = "{}"
    for value in [None, {}, [], b"{}"]:
        with pytest.raises(TypeError):
            mssql_py_core.BulkCopyJson(value)


@pytest.mark.integration
def test_snapshot_carrier_mapped_json_and_unmapped_payload(connection, monkeypatch):
    cursor = connection.cursor()
    table = "#BulkJsonSnapshot"
    cursor.execute(f"CREATE TABLE {table} (id INT, doc JSON NULL, tail NVARCHAR(20))")
    text = ' {"duplicate":1,"duplicate":2,"text":"\u4e2d\U0001f600","number":1.000} '
    carrier = mssql_py_core.BulkCopyJson(text)
    hidden = mssql_py_core.BulkCopyJson("\ud800")

    def unexpected_json_call(*args, **kwargs):
        raise AssertionError("snapshot must not be parsed or dumped")

    with monkeypatch.context() as patch:
        patch.setattr(json, "loads", unexpected_json_call)
        patch.setattr(json, "dumps", unexpected_json_call)
        result = cursor.bulkcopy(
            table, iter([(carrier, "plain", i, hidden) for i in range(3)]),
            column_mappings=[(2, "id"), (0, "doc"), (1, "tail")],
            batch_size=2, use_internal_transaction=True,
        )
    assert result["rows_copied"] == 3
    cursor.execute(f"SELECT id, JSON_VALUE(doc, '$.text'), tail FROM {table} ORDER BY id")
    assert cursor.fetchall() == [(i, "\u4e2d\U0001f600", "plain") for i in range(3)]


@pytest.mark.integration
@pytest.mark.parametrize("destination", ["INT", "NVARCHAR(MAX)", "XML"])
def test_snapshot_carrier_rejects_non_json_mapping(connection, destination):
    cursor = connection.cursor()
    cursor.execute(f"CREATE TABLE #WrongJsonDestination (value {destination})")
    with pytest.raises(ValueError, match="requires a JSON destination"):
        cursor.bulkcopy(
            "#WrongJsonDestination", iter([(mssql_py_core.BulkCopyJson("{}"),)]),
            use_internal_transaction=True,
        )


@pytest.mark.integration
def test_snapshot_carrier_large_unicode_and_null(connection):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #LargeJsonSnapshot (id INT, doc JSON NULL)")
    text = "\u4e2d\U0001f600" * 16000
    snapshot = mssql_py_core.BulkCopyJson('{"text":"' + text + '"}')
    cursor.bulkcopy(
        "#LargeJsonSnapshot", iter([(1, snapshot), (2, None)]),
        keep_nulls=True, use_internal_transaction=True,
    )
    cursor.execute("SELECT id, CAST(doc AS NVARCHAR(MAX)) FROM #LargeJsonSnapshot ORDER BY id")
    rows = cursor.fetchall()
    assert json.loads(rows[0][1])["text"] == text
    assert rows[1] == (2, None)


@pytest.mark.integration
def test_snapshot_carrier_coexists_with_legacy_vector(connection):
    cursor = connection.cursor()
    cursor.execute("CREATE TABLE #JsonVectorSnapshot (id INT, doc JSON, embedding VECTOR(3))")
    carrier = mssql_py_core.BulkCopyJson('{"value":1}')
    result = cursor.bulkcopy(
        "#JsonVectorSnapshot", iter([(1, carrier, [1.0, 2.0, 3.0])]),
        use_internal_transaction=True,
    )
    assert result["rows_copied"] == 1
    cursor.execute("SELECT id, JSON_VALUE(doc, '$.value'), embedding FROM #JsonVectorSnapshot")
    row = cursor.fetchone()
    assert row[:2] == (1, "1")
    assert list(row[2]) == [1.0, 2.0, 3.0]


@pytest.mark.integration
@pytest.mark.parametrize("batch_size", [0, 2])
@pytest.mark.parametrize("container", [dict, list])
def test_json_reused_mutable_cells_are_written_before_next_pull(connection, batch_size, container):
    cursor = connection.cursor()
    cursor.execute(
        "CREATE TABLE #ReusedJsonCells (id INT, doc JSON, snapshot JSON, absent JSON NULL)"
    )
    cell = container()

    def rows():
        for index in range(5):
            cell.clear()
            if isinstance(cell, dict):
                cell["index"] = index
            else:
                cell.append(index)
            snapshot = mssql_py_core.BulkCopyJson(json.dumps({"index": index}))
            yield (None, snapshot, cell, index)
        cell.clear()

    result = cursor.bulkcopy(
        "#ReusedJsonCells", rows(),
        column_mappings=[(3, "id"), (2, "doc"), (1, "snapshot"), (0, "absent")],
        batch_size=batch_size, keep_nulls=True, use_internal_transaction=True,
    )
    assert result["rows_copied"] == 5
    cursor.execute(
        "SELECT id, CAST(doc AS NVARCHAR(MAX)), JSON_VALUE(snapshot, '$.index'), absent "
        "FROM #ReusedJsonCells ORDER BY id"
    )
    assert [
        (row[0], json.loads(row[1]), row[2], row[3]) for row in cursor.fetchall()
    ] == [
        (i, {"index": i} if container is dict else [i], str(i), None) for i in range(5)
    ]


@pytest.mark.integration
def test_cursor_bulkcopy_json_basic(client_context):
    """Test cursor bulkcopy method with JSON column and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with JSON column
    # Note: SQL Server 2025's native JSON type (0xF4) has issues with bulk copy.
    # Using JSON instead for now. Python dict/list will be serialized to JSON.
    table_name = "BulkCopyJsonTestTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data - JSON as strings
    data = [
        (1, '{"name": "Alice", "age": 30}'),
        (2, '{"name": "Bob", "age": 25}'),
        (3, '{"name": "Charlie", "age": 35}'),
    ]

    # Execute bulk copy with explicit column mappings
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000,
        timeout=30,
        column_mappings=[
            (0, "id"),
            (1, "json_data"),
        ],
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly and is valid JSON
    cursor.execute(f"SELECT id, json_data FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 1
    json_obj = json.loads(rows[0][1])
    assert json_obj["name"] == "Alice"
    assert json_obj["age"] == 30

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_from_dict(client_context):
    """Test cursor bulkcopy with Python dict that should be serialized to JSON string.

    Tests type coercion when source data contains Python dicts but
    destination column is JSON for JSON storage.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with JSON column
    table_name = "BulkCopyJsonFromDictTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data - Python dicts that should be serialized to JSON
    data = [
        (1, {"name": "Alice", "age": 30, "active": True}),
        (2, {"name": "Bob", "age": 25, "active": False}),
        (3, {"name": "Charlie", "age": 35, "scores": [90, 85, 92]}),
    ]

    # Execute bulk copy without explicit type conversion
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly as JSON strings
    cursor.execute(f"SELECT id, json_data FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    
    # Verify first row
    json_obj1 = json.loads(rows[0][1])
    assert json_obj1["name"] == "Alice"
    assert json_obj1["age"] == 30
    assert json_obj1["active"] is True
    
    # Verify third row with array
    json_obj3 = json.loads(rows[2][1])
    assert json_obj3["name"] == "Charlie"
    assert json_obj3["scores"] == [90, 85, 92]

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_from_list(client_context):
    """Test cursor bulkcopy with Python list that should be serialized to JSON array.

    Tests type coercion when source data contains Python lists but
    destination column is JSON for JSON storage.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with JSON column
    table_name = "BulkCopyJsonFromListTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_array JSON)")

    # Prepare test data - Python lists that should be serialized to JSON arrays
    data = [
        (1, [1, 2, 3, 4, 5]),
        (2, ["apple", "banana", "cherry"]),
        (3, [{"id": 1, "name": "Item1"}, {"id": 2, "name": "Item2"}]),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1

    # Verify data was inserted correctly as JSON arrays
    cursor.execute(f"SELECT id, json_array FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    
    # Verify numeric array
    json_arr1 = json.loads(rows[0][1])
    assert json_arr1 == [1, 2, 3, 4, 5]
    
    # Verify string array
    json_arr2 = json.loads(rows[1][1])
    assert json_arr2 == ["apple", "banana", "cherry"]
    
    # Verify array of objects
    json_arr3 = json.loads(rows[2][1])
    assert len(json_arr3) == 2
    assert json_arr3[0]["name"] == "Item1"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_null_values(client_context):
    """Test cursor bulkcopy with NULL JSON values.

    Tests that NULL values are properly handled for nullable JSON columns.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with nullable JSON column
    table_name = "BulkCopyJsonNullTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data with NULL values
    data = [
        (1, '{"name": "Alice"}'),
        (2, None),  # NULL value
        (3, {"name": "Charlie"}),
        (4, None),  # Another NULL
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1

    # Verify data including NULLs
    cursor.execute(f"SELECT id, json_data FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][1] is not None
    assert rows[1][1] is None  # Verify NULL
    assert rows[2][1] is not None
    assert rows[3][1] is None  # Verify NULL

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_nested_objects(client_context):
    """Test cursor bulkcopy with nested JSON objects.

    Tests serialization of complex nested Python data structures.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyJsonNestedTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data with nested structures
    data = [
        (
            1,
            {
                "user": {
                    "name": "Alice",
                    "address": {"city": "Seattle", "zip": "98101"},
                    "tags": ["developer", "python"],
                }
            },
        ),
        (
            2,
            {
                "user": {
                    "name": "Bob",
                    "address": {"city": "Portland", "zip": "97201"},
                    "tags": ["designer", "javascript"],
                }
            },
        ),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 2

    # Verify nested structure is preserved
    cursor.execute(f"SELECT json_data FROM {table_name} WHERE id = 1")
    row = cursor.fetchone()
    json_obj = json.loads(row[0])
    assert json_obj["user"]["name"] == "Alice"
    assert json_obj["user"]["address"]["city"] == "Seattle"
    assert "python" in json_obj["user"]["tags"]

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_with_isjson_check(client_context):
    """Test that bulk copied JSON data passes SQL Server's ISJSON validation.

    This test verifies that the JSON strings produced by bulk copy are
    valid according to SQL Server's ISJSON() function.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyJsonValidationTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data with various Python types
    data = [
        (1, {"string": "value", "number": 42, "boolean": True, "null": None}),
        (2, [1, 2, 3, 4, 5]),
        (3, {"nested": {"deep": {"value": "found"}}}),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    assert result["rows_copied"] == 3

    # Use SQL Server's ISJSON() to validate all rows contain valid JSON
    cursor.execute(
        f"SELECT id, ISJSON(json_data) as is_valid FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert all(row[1] == 1 for row in rows), "All JSON data should be valid"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_special_characters(client_context):
    """Test cursor bulkcopy with JSON containing special characters.

    Tests proper escaping and encoding of special characters in JSON strings.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyJsonSpecialCharsTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data with special characters
    data = [
        (1, {"message": 'Hello "World"'}),  # Quotes
        (2, {"path": "C:\\Users\\test\\file.txt"}),  # Backslashes
        (3, {"text": "Line1\nLine2\tTabbed"}),  # Newlines and tabs
        (4, {"emoji": "Hello 👋 World 🌍"}),  # Unicode/emoji
        (5, {"xml": "<root><child>value</child></root>"}),  # XML-like content
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result["rows_copied"] == 5

    # Verify special characters are preserved
    cursor.execute(f"SELECT id, json_data FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    json_obj1 = json.loads(rows[0][1])
    assert json_obj1["message"] == 'Hello "World"'
    
    json_obj2 = json.loads(rows[1][1])
    assert json_obj2["path"] == "C:\\Users\\test\\file.txt"
    
    json_obj4 = json.loads(rows[3][1])
    assert json_obj4["emoji"] == "Hello 👋 World 🌍"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_empty_structures(client_context):
    """Test cursor bulkcopy with empty JSON objects and arrays.

    Tests serialization of empty dicts and lists.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyJsonEmptyTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data with empty structures
    data = [
        (1, {}),  # Empty dict
        (2, []),  # Empty list
        (3, {"empty_obj": {}, "empty_arr": []}),  # Nested empty structures
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result["rows_copied"] == 3

    # Verify empty structures
    cursor.execute(f"SELECT id, json_data FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    json_obj1 = json.loads(rows[0][1])
    assert json_obj1 == {}
    
    json_obj2 = json.loads(rows[1][1])
    assert json_obj2 == []
    
    json_obj3 = json.loads(rows[2][1])
    assert json_obj3["empty_obj"] == {}
    assert json_obj3["empty_arr"] == []

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_numeric_types(client_context):
    """Test cursor bulkcopy with various Python numeric types in JSON.

    Tests serialization of int, float, and other numeric types.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyJsonNumericTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data with various numeric types
    data = [
        (1, {"int": 42, "float": 3.14159, "negative": -100, "zero": 0}),
        (2, {"large_int": 9223372036854775807}),  # Large integer
        (3, {"scientific": 1.23e-10, "infinity": float('inf')}),  # Note: JSON doesn't support Infinity
    ]

    # Execute bulk copy - note that infinity might cause issues
    try:
        result = cursor.bulkcopy(
            table_name, iter(data), batch_size=1000, timeout=30
        )
        rows_copied = result["rows_copied"]
    except (ValueError, OverflowError, RuntimeError) as e:
        # If infinity causes an error, that's expected behavior
        print(f"Expected error with infinity: {e}")
        # Try again without infinity
        data = [
            (1, {"int": 42, "float": 3.14159, "negative": -100, "zero": 0}),
            (2, {"large_int": 9223372036854775807}),
            (3, {"scientific": 1.23e-10}),
        ]
        result = cursor.bulkcopy(
            table_name, iter(data), batch_size=1000, timeout=30
        )
        rows_copied = result["rows_copied"]

    # Verify at least some rows were copied
    assert rows_copied >= 2

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_json_boolean_values(client_context):
    """Test cursor bulkcopy with Python boolean values in JSON.

    Tests that Python True/False are correctly serialized to JSON true/false.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyJsonBooleanTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, json_data JSON)")

    # Prepare test data with boolean values
    data = [
        (1, {"active": True, "deleted": False}),
        (2, {"flags": [True, False, True, True]}),
        (3, {"config": {"enabled": True, "debug": False, "verbose": True}}),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result["rows_copied"] == 3

    # Verify boolean values are preserved
    cursor.execute(f"SELECT id, json_data FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    json_obj1 = json.loads(rows[0][1])
    assert json_obj1["active"] is True
    assert json_obj1["deleted"] is False
    
    json_obj2 = json.loads(rows[1][1])
    assert json_obj2["flags"] == [True, False, True, True]

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()
