# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Bulk copy tests for NVARCHAR data type with varying lengths."""
import pytest
import mssql_py_core


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_basic(client_context):
    """Test cursor bulkcopy method with nvarchar columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with nvarchar columns of different lengths
    table_name = "BulkCopyTestTableNVarchar"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, name NVARCHAR(50), description NVARCHAR(200))"
    )

    # Prepare test data - strings
    data = [
        (1, "John Doe", "First employee"),
        (2, "Jane Smith", "Second employee"),
        (3, "Bob Johnson", "Third employee"),
    ]

    # Execute bulk copy with explicit column mappings
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000,
        timeout=30,
        column_mappings=[
            (0, "id"),
            (1, "name"),
            (2, "description"),
        ],
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, name, description FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 1 and rows[0][1] == "John Doe" and rows[0][2] == "First employee"
    assert rows[1][0] == 2 and rows[1][1] == "Jane Smith"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_max(client_context):
    """Test cursor bulkcopy with NVARCHAR(MAX) column for large text data."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with NVARCHAR(MAX) column
    table_name = "BulkCopyTestTableNVarcharMax"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, content NVARCHAR(MAX))")

    # Prepare test data - including a large string
    large_text = "A" * 10000  # 10,000 characters
    data = [
        (1, "Short text"),
        (2, large_text),
        (3, "Another short text"),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, content FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 1 and rows[0][1] == "Short text"
    assert rows[1][0] == 2 and rows[1][1] == large_text
    assert rows[2][0] == 3 and rows[2][1] == "Another short text"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_auto_mapping(client_context):
    """Test cursor bulkcopy with automatic column mapping and NULL values.

    Tests bulkcopy when no mappings are specified, including NULL value handling.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with nullable nvarchar columns
    table_name = "BulkCopyAutoMapTableNVarchar"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, name NVARCHAR(100))")

    # Prepare test data with NULL values
    data = [
        (1, "Alice"),
        (2, None),  # NULL value in name column
        (None, "Bob"),  # NULL value in id column
        (4, "Charlie"),
    ]

    # Execute bulk copy WITHOUT column mappings
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly, including NULL values
    cursor.execute(f"SELECT id, name FROM {table_name} ORDER BY COALESCE(id, 999)")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][0] == 1 and rows[0][1] == "Alice"
    assert rows[1][0] == 2 and rows[1][1] is None  # Verify NULL in name column
    assert rows[2][0] == 4 and rows[2][1] == "Charlie"
    assert rows[3][0] is None and rows[3][1] == "Bob"  # Verify NULL in id column

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_numbers_as_strings(client_context):
    """Test cursor bulkcopy with numeric values as strings to NVARCHAR columns.

    Tests that numbers can be bulk copied into NVARCHAR columns by converting
    them to their string representations.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with NVARCHAR columns
    table_name = "BulkCopyNumbersToNVarcharTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id NVARCHAR(20), value NVARCHAR(50), decimal_val NVARCHAR(50))"
    )

    # Prepare test data - numeric values that should be converted to strings
    data = [
        (1, 100, 99.99),
        (2, 200, 199.99),
        (3, 300, 299.99),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly as strings
    cursor.execute(f"SELECT id, value, decimal_val FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == "1" and rows[0][1] == "100"
    assert rows[1][0] == "2" and rows[1][1] == "200"
    assert rows[2][0] == "3" and rows[2][1] == "300"
    # Verify decimal values were converted to strings
    assert "99.99" in rows[0][2]
    assert "199.99" in rows[1][2]

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_string_numbers_to_nvarchar(client_context):
    """Test cursor bulkcopy with string representations of numbers to NVARCHAR columns.

    Tests that string values containing numbers can be bulk copied to NVARCHAR columns.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with NVARCHAR columns
    table_name = "BulkCopyStringNumbersToNVarcharTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id NVARCHAR(20), value NVARCHAR(50))")

    # Prepare test data - strings containing numbers
    data = [
        ("1", "100"),
        ("2", "200"),
        ("3", "300"),
        ("123456789", "987654321"),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly as strings
    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][0] == "1" and rows[0][1] == "100"
    assert rows[1][0] == "123456789" and rows[1][1] == "987654321"
    assert rows[2][0] == "2" and rows[2][1] == "200"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_varying_lengths(client_context):
    """Test cursor bulkcopy with NVARCHAR columns of different lengths.

    Tests that data is correctly inserted into NVARCHAR columns with
    varying length specifications: small, medium, large, and MAX.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with NVARCHAR columns of varying lengths
    table_name = "BulkCopyVaryingLengthNVarcharTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"""CREATE TABLE {table_name} (
            id INT,
            small_text NVARCHAR(10),
            medium_text NVARCHAR(100),
            large_text NVARCHAR(4000),
            max_text NVARCHAR(MAX)
        )"""
    )

    # Prepare test data with varying string lengths
    data = [
        (1, "Short", "Medium length text here", "A" * 1000, "B" * 5000),
        (2, "Test", "Another medium text", "C" * 2000, "D" * 10000),
        (3, "End", "Final text", "E" * 500, "F" * 8000),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly
    cursor.execute(
        f"SELECT id, small_text, medium_text, large_text, max_text FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 1
    assert rows[0][1] == "Short"
    assert rows[0][2] == "Medium length text here"
    assert len(rows[0][3]) == 1000 and rows[0][3][0] == "A"
    assert len(rows[0][4]) == 5000 and rows[0][4][0] == "B"
    
    assert rows[1][0] == 2
    assert len(rows[1][3]) == 2000 and rows[1][3][0] == "C"
    assert len(rows[1][4]) == 10000 and rows[1][4][0] == "D"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_null_to_non_nullable_column(client_context):
    """Test cursor bulkcopy with null value for non-nullable NVARCHAR column.

    Tests that the client-side metadata validation catches attempts to insert
    null into a non-nullable column and raises an appropriate conversion error.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a temp table with a non-nullable NVARCHAR column
    table_name = "#BulkCopyNonNullableNVarcharTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, name NVARCHAR(100) NOT NULL)")

    # Prepare test data with a null value
    data = [
        (1, "Alice"),
        (2, None),  # This should trigger a conversion error
        (3, "Bob"),
    ]

    # Execute bulk copy and expect a ValueError
    error_raised = False
    error_message = ""
    try:
        result = cursor.bulkcopy(
            table_name, iter(data), batch_size=1000, timeout=30
        )
        # If we get here, no error was raised
        print(f"No error raised. Result: {result}")
    except ValueError as e:
        error_raised = True
        error_message = str(e).lower()
        print(f"Expected ValueError caught: {e}")

    # Verify that an error was raised with appropriate message
    assert (
        error_raised
    ), "Expected a ValueError to be raised for null value in non-nullable column"
    assert (
        "conversion" in error_message or "null" in error_message
    ), f"Expected conversion error, got: {error_message}"
    assert (
        "non-nullable" in error_message
    ), f"Expected 'non-nullable' in error message, got: {error_message}"

    # Close connection - temp table will be automatically dropped
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_exceeds_length(client_context):
    """Test cursor bulkcopy with string exceeding NVARCHAR column length.

    Tests that strings longer than the column's defined length are properly handled.
    This should either truncate or raise an error depending on implementation.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a temp table with a short NVARCHAR column
    table_name = "#BulkCopyNVarcharLengthExceedTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, name NVARCHAR(10))")

    # Prepare test data with string exceeding column length
    data = [
        (1, "Short"),
        (2, "ThisStringIsWayTooLongForTheColumn"),  # Exceeds NVARCHAR(10)
        (3, "Test"),
    ]

    # Execute bulk copy and expect an error or truncation
    # The exact behavior depends on implementation
    error_raised = False
    error_message = ""
    try:
        result = cursor.bulkcopy(
            table_name, iter(data), batch_size=1000, timeout=30
        )
        # If we get here, the operation succeeded (possibly with truncation)
        print(f"Operation completed. Result: {result}")
        
        # Check if data was truncated
        cursor.execute(f"SELECT id, name FROM {table_name} WHERE id = 2")
        row = cursor.fetchone()
        if row:
            print(f"Row 2 value: '{row[1]}' (length: {len(row[1])})")
            # Verify truncation occurred
            assert len(row[1]) <= 10, "String should be truncated to column length"
    except (ValueError, RuntimeError) as e:
        error_raised = True
        error_message = str(e).lower()
        print(f"Error caught: {e}")
        # If error is raised, verify it mentions length or truncation
        assert (
            "length" in error_message or "truncat" in error_message or "exceed" in error_message
        ), f"Expected length-related error, got: {error_message}"

    # Close connection - temp table will be automatically dropped
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_unicode_characters(client_context):
    """Test cursor bulkcopy with Unicode characters in NVARCHAR columns.

    Tests that Unicode characters (emoji, special characters, non-Latin scripts)
    are correctly stored in NVARCHAR columns.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with NVARCHAR columns
    table_name = "BulkCopyUnicodeNVarcharTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, text NVARCHAR(200))")

    # Prepare test data with various Unicode characters
    data = [
        (1, "Hello 世界"),  # Chinese characters
        (2, "Привет мир"),  # Cyrillic characters
        (3, "مرحبا بالعالم"),  # Arabic characters
        (4, "Hello 🌍🌎🌏"),  # Emoji
        (5, "Special: ©®™€£¥"),  # Special symbols
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 5
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly with Unicode preserved
    cursor.execute(f"SELECT id, text FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 5
    assert rows[0][0] == 1 and rows[0][1] == "Hello 世界"
    assert rows[1][0] == 2 and rows[1][1] == "Привет мир"
    assert rows[2][0] == 3 and rows[2][1] == "مرحبا بالعالم"
    assert rows[3][0] == 4 and rows[3][1] == "Hello 🌍🌎🌏"
    assert rows[4][0] == 5 and rows[4][1] == "Special: ©®™€£¥"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_empty_strings(client_context):
    """Test cursor bulkcopy with empty strings to NVARCHAR columns.

    Tests that empty strings are correctly distinguished from NULL values
    when bulk copying to NVARCHAR columns.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with nullable NVARCHAR column
    table_name = "BulkCopyEmptyStringNVarcharTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, text NVARCHAR(100))")

    # Prepare test data with empty strings and NULL values
    data = [
        (1, ""),  # Empty string
        (2, None),  # NULL
        (3, "  "),  # Whitespace
        (4, "Text"),  # Normal text
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, text FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][0] == 1 and rows[0][1] == ""  # Empty string preserved
    assert rows[1][0] == 2 and rows[1][1] is None  # NULL preserved
    assert rows[2][0] == 3 and rows[2][1] == "  "  # Whitespace preserved
    assert rows[3][0] == 4 and rows[3][1] == "Text"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_max_empty_strings(client_context):
    """Regression test for microsoft/mssql-python#547.

    Empty strings into NVARCHAR(MAX)/VARCHAR(MAX) columns must not desync the
    PLP stream. A zero-length PLP chunk header IS the PLP terminator per
    MS-TDS, so writing chunk_len=0 plus a separate terminator caused SQL error
    4804 ("premature end of message").
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyEmptyStringNVarcharMax"
    cursor.execute(
        f"CREATE TABLE {table_name} ("
        f"id INT, body NVARCHAR(MAX) NULL, "
        f"body_v VARCHAR(MAX) NULL, flag BIT NOT NULL)"
    )

    data = [
        (1, "hello", "hello", True),
        (2, "", "", True),
        (3, "world", "world", False),
        (4, None, None, True),
    ]

    result = cursor.bulkcopy(table_name, iter(data), batch_size=1000, timeout=30)

    assert result is not None
    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1

    cursor.execute(f"SELECT id, body, body_v, flag FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert rows[0] == (1, "hello", "hello", True)
    assert rows[1] == (2, "", "", True)
    assert rows[2] == (3, "world", "world", False)
    assert rows[3] == (4, None, None, True)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_mixed_types(client_context):
    """Test cursor bulkcopy with mixed data types converting to NVARCHAR columns.

    Tests that various Python types (int, float, bool, string) can all be
    converted to their string representations when targeting NVARCHAR columns.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with NVARCHAR columns
    table_name = "BulkCopyMixedTypesNVarcharTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, value NVARCHAR(100))")

    # Prepare test data with mixed types
    data = [
        (1, 123),  # Integer
        (2, 456.789),  # Float
        (3, True),  # Boolean
        (4, False),  # Boolean
        (5, "Regular string"),  # String
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 5
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was converted to strings correctly
    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 5
    assert rows[0][0] == 1 and rows[0][1] == "123"
    assert rows[1][0] == 2 and "456.789" in rows[1][1]  # Float may have slight variations
    assert rows[2][0] == 3 and rows[2][1] in ["True", "1"]  # Boolean representations
    assert rows[3][0] == 4 and rows[3][1] in ["False", "0"]  # Boolean representations
    assert rows[4][0] == 5 and rows[4][1] == "Regular string"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_odd_lengths(client_context):
    """Regression test: NVARCHAR(n) with odd n values.

    Previously, the COLMETADATA MaxLength was sent as n (char count) instead
    of 2n (byte count).  When n is odd, SQL Server rejects it with:
      "Unicode data is odd byte size for column N. Should be even byte size."
    All existing tests used even n values (10, 50, 100, 200), so the bug
    was never caught.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyNVarcharOddLengths"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"""CREATE TABLE {table_name} (
            id INT,
            col1 NVARCHAR(1),
            col3 NVARCHAR(3),
            col5 NVARCHAR(5),
            col7 NVARCHAR(7),
            col15 NVARCHAR(15),
            col255 NVARCHAR(255)
        )"""
    )

    data = [
        (1, "A", "ABC", "Hello", "Testing", "Odd length test", "X" * 100),
        (2, "Z", "XYZ", "World", "Columns", "Regression test", "Y" * 200),
        (3, None, None, None, None, None, None),
    ]

    result = cursor.bulkcopy(table_name, iter(data), batch_size=100, timeout=30)

    assert result is not None
    assert result["rows_copied"] == 3

    cursor.execute(
        f"SELECT id, col1, col3, col5, col7, col15, col255 FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()
    assert len(rows) == 3

    assert rows[0] == (1, "A", "ABC", "Hello", "Testing", "Odd length test", "X" * 100)
    assert rows[1] == (2, "Z", "XYZ", "World", "Columns", "Regression test", "Y" * 200)
    assert rows[2] == (3, None, None, None, None, None, None)

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_nvarchar_odd_lengths_near_boundary(client_context):
    """Test NVARCHAR(n) with odd n, inserting strings at the max boundary.

    Ensures that strings of exactly n characters work with odd-n NVARCHAR
    columns — not just short strings.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "BulkCopyNVarcharOddBoundary"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"""CREATE TABLE {table_name} (
            col1 NVARCHAR(1),
            col3 NVARCHAR(3),
            col5 NVARCHAR(5)
        )"""
    )

    data = [
        ("X", "ABC", "HELLO"),       # exactly at max length
        ("Y", "XY", "HI"),           # under max length
    ]

    result = cursor.bulkcopy(table_name, iter(data), batch_size=100, timeout=30)
    assert result["rows_copied"] == 2

    cursor.execute(f"SELECT col1, col3, col5 FROM {table_name} ORDER BY col1")
    rows = cursor.fetchall()
    assert rows[0] == ("X", "ABC", "HELLO")
    assert rows[1] == ("Y", "XY", "HI")

    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()
