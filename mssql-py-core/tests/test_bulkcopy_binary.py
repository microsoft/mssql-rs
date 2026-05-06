# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Bulk copy tests for BINARY and VARBINARY data types."""

import pytest
import mssql_py_core


@pytest.mark.integration
def test_cursor_bulkcopy_binary_basic(client_context):
    """Test cursor bulkcopy with byte arrays to BINARY and VARBINARY columns."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with BINARY and VARBINARY columns
    table_name = "BulkCopyBinaryTestTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, binary_col BINARY(16), varbinary_col VARBINARY(100))"
    )

    # Prepare test data - byte arrays of exact size
    bytes16 = bytes([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                     0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10])
    bytes4 = bytes([0x01, 0x02, 0x03, 0x04])
    bytes20 = bytes([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                     0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10,
                     0x11, 0x12, 0x13, 0x14])
    
    data = [
        (1, bytes16, bytes4),  # 16 bytes to BINARY(16), 4 bytes to VARBINARY
        (2, bytes16, bytes20),  # 16 bytes to both columns
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=30,
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 2
    assert result["batch_count"] == 1

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, binary_col, varbinary_col, DATALENGTH(varbinary_col) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 2
    
    # Row 1: 16 bytes in BINARY(16), 4 bytes in VARBINARY
    assert rows[0][0] == 1
    assert rows[0][1] == bytes16
    assert rows[0][2] == bytes4
    assert rows[0][3] == 4  # VARBINARY length should be 4
    
    # Row 2: 16 bytes in both columns
    assert rows[1][0] == 2
    assert rows[1][1] == bytes16
    assert rows[1][2] == bytes20
    assert rows[1][3] == 20  # VARBINARY length should be 20

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_binary_padding(client_context):
    """Test that smaller byte arrays get zero-padded when sent to BINARY(n) columns.
    
    BINARY(n) is a fixed-length type, so SQL Server pads smaller values with zeros.
    This test verifies that our bulk copy implementation handles this correctly.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with BINARY(16) column
    table_name = "BulkCopyBinaryPaddingTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, binary_col BINARY(16))")

    # Prepare test data - byte array smaller than column size
    bytes4 = bytes([0x01, 0x02, 0x03, 0x04])
    
    data = [
        (1, bytes4),  # 4 bytes to BINARY(16) - should be padded
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=30,
    )

    # Verify the bulk copy succeeded
    assert result is not None
    assert result["rows_copied"] == 1

    # Verify data was padded correctly
    cursor.execute(f"SELECT binary_col FROM {table_name}")
    rows = cursor.fetchall()
    assert len(rows) == 1
    
    # Should be padded with zeros: 01-02-03-04-00-00-00-00-00-00-00-00-00-00-00-00
    expected = bytes4 + bytes(12)  # Original 4 bytes + 12 zero bytes
    assert rows[0][0] == expected
    assert len(rows[0][0]) == 16

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_binary_varbinary_no_padding(client_context):
    """Test that VARBINARY columns store exact byte length without padding."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with VARBINARY column
    table_name = "BulkCopyVarbinaryNoPaddingTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, varbinary_col VARBINARY(100))")

    # Prepare test data with different sizes
    bytes4 = bytes([0x01, 0x02, 0x03, 0x04])
    bytes10 = bytes(range(10))
    
    data = [
        (1, bytes4),   # 4 bytes
        (2, bytes10),  # 10 bytes
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=30,
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 2

    # Verify exact lengths are preserved (no padding)
    cursor.execute(f"SELECT id, varbinary_col, DATALENGTH(varbinary_col) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 2
    
    # Row 1: 4 bytes stored as-is
    assert rows[0][0] == 1
    assert rows[0][1] == bytes4
    assert rows[0][2] == 4  # Exact length
    
    # Row 2: 10 bytes stored as-is
    assert rows[1][0] == 2
    assert rows[1][1] == bytes10
    assert rows[1][2] == 10  # Exact length

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_binary_too_large(client_context):
    """Test that byte arrays larger than BINARY(n) column size trigger an error.
    
    When source data exceeds the target BINARY column size, an error should be raised
    during type coercion or validation.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with BINARY(8) column (small size)
    table_name = "#BulkCopyBinaryTooLargeTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, binary_col BINARY(8))")

    # Prepare test data - byte array larger than column size
    bytes16 = bytes([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                     0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10])
    
    data = [
        (1, bytes16),  # 16 bytes to BINARY(8) - should fail
    ]

    # Execute bulk copy and expect an error
    error_raised = False
    error_message = ""
    try:
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=1000, timeout=30,
        )
        print(f"No error raised. Result: {result}")
    except (ValueError, RuntimeError) as e:
        error_raised = True
        error_message = str(e).lower()
        print(f"Expected error caught: {e}")

    # Verify that an error was raised about size mismatch
    assert error_raised, "Expected an error for byte array exceeding BINARY column size"
    assert any(word in error_message for word in ["length", "size", "too large", "exceeds"]), \
        f"Expected size/length error, got: {error_message}"

    # Close connection - temp table will be automatically dropped
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_varbinary_too_large(client_context):
    """Test that byte arrays larger than VARBINARY(n) column size trigger an error."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with VARBINARY(10) column (small size)
    table_name = "#BulkCopyVarbinaryTooLargeTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, varbinary_col VARBINARY(10))")

    # Prepare test data - byte array larger than column max size
    bytes20 = bytes(range(20))
    
    data = [
        (1, bytes20),  # 20 bytes to VARBINARY(10) - should fail
    ]

    # Execute bulk copy and expect an error
    error_raised = False
    error_message = ""
    try:
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=1000, timeout=30,
        )
        print(f"No error raised. Result: {result}")
    except (ValueError, RuntimeError) as e:
        error_raised = True
        error_message = str(e).lower()
        print(f"Expected error caught: {e}")

    # Verify that an error was raised about size mismatch
    assert error_raised, "Expected an error for byte array exceeding VARBINARY column size"
    assert any(word in error_message for word in ["length", "size", "too large", "exceeds"]), \
        f"Expected size/length error, got: {error_message}"

    # Close connection
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_binary_null_handling(client_context):
    """Test NULL value handling for nullable BINARY and VARBINARY columns."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with nullable binary columns
    table_name = "BulkCopyBinaryNullTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, binary_col BINARY(16), varbinary_col VARBINARY(100))"
    )

    # Prepare test data with NULL values
    bytes16 = bytes([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                     0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10])
    
    data = [
        (1, bytes16, bytes16),      # Both non-null
        (2, None, bytes16),         # NULL in binary_col
        (3, bytes16, None),         # NULL in varbinary_col
        (4, None, None),            # Both NULL
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=30,
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 4

    # Verify NULL values were handled correctly
    cursor.execute(f"SELECT id, binary_col, varbinary_col FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 4
    
    assert rows[0][0] == 1 and rows[0][1] is not None and rows[0][2] is not None
    assert rows[1][0] == 2 and rows[1][1] is None and rows[1][2] is not None
    assert rows[2][0] == 3 and rows[2][1] is not None and rows[2][2] is None
    assert rows[3][0] == 4 and rows[3][1] is None and rows[3][2] is None

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_binary_null_to_non_nullable(client_context):
    """Test that NULL values to non-nullable BINARY columns trigger an error."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with non-nullable binary column
    table_name = "#BulkCopyBinaryNonNullableTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, binary_col BINARY(16) NOT NULL)")

    # Prepare test data with NULL value
    bytes16 = bytes([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                     0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10])
    
    data = [
        (1, bytes16),
        (2, None),  # NULL to NOT NULL column - should fail
        (3, bytes16),
    ]

    # Execute bulk copy and expect an error
    error_raised = False
    error_message = ""
    try:
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=1000, timeout=30,
        )
        print(f"No error raised. Result: {result}")
    except ValueError as e:
        error_raised = True
        error_message = str(e).lower()
        print(f"Expected ValueError caught: {e}")

    # Verify that an error was raised about NULL/non-nullable
    assert error_raised, "Expected a ValueError for NULL value in non-nullable column"
    assert "non-nullable" in error_message or "not null" in error_message, \
        f"Expected non-nullable error, got: {error_message}"

    # Close connection
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_string_to_binary_fails(client_context):
    """Test that strings cannot be directly sent as binary data.
    
    Based on .NET SqlBulkCopy behavior, string-to-binary coercion should fail.
    Strings must be explicitly converted to bytes before bulk copy.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with binary column
    table_name = "#BulkCopyStringToBinaryTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, binary_col BINARY(16))")

    # Prepare test data with string values (not bytes)
    data = [
        (1, "Hello"),  # String instead of bytes - should fail
    ]

    # Execute bulk copy and expect an error
    error_raised = False
    error_message = ""
    try:
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=1000, timeout=30,
        )
        print(f"No error raised. Result: {result}")
    except (ValueError, TypeError, RuntimeError) as e:
        error_raised = True
        error_message = str(e).lower()
        print(f"Expected error caught: {e}")

    # Verify that an error was raised about type mismatch
    assert error_raised, "Expected an error for string-to-binary coercion"
    assert any(word in error_message for word in ["type", "convert", "string", "binary", "bytes"]), \
        f"Expected type conversion error, got: {error_message}"

    # Close connection
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_varbinary_max(client_context):
    """Test bulk copy with VARBINARY(MAX) column to handle large binary data.
    
    VARBINARY(MAX) can store up to 2GB of binary data and should handle
    various sizes from small to large without any padding or truncation.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with VARBINARY(MAX) column
    table_name = "BulkCopyVarbinaryMaxTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, varbinary_max_col VARBINARY(MAX))")

    # Prepare test data with various sizes
    bytes_small = bytes([0x01, 0x02, 0x03, 0x04])
    bytes_medium = bytes(range(256))  # 256 bytes
    bytes_large = bytes([i % 256 for i in range(10000)])  # 10KB
    bytes_very_large = bytes([i % 256 for i in range(100000)])  # 100KB
    
    data = [
        (1, bytes_small),      # 4 bytes
        (2, bytes_medium),     # 256 bytes
        (3, bytes_large),      # 10KB
        (4, bytes_very_large), # 100KB
        (5, None),             # NULL value
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=30,
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 5

    # Verify all data sizes are preserved correctly
    cursor.execute(f"SELECT id, varbinary_max_col, DATALENGTH(varbinary_max_col) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 5
    
    # Row 1: Small data (4 bytes)
    assert rows[0][0] == 1
    assert rows[0][1] == bytes_small
    assert rows[0][2] == 4
    
    # Row 2: Medium data (256 bytes)
    assert rows[1][0] == 2
    assert rows[1][1] == bytes_medium
    assert rows[1][2] == 256
    
    # Row 3: Large data (10KB)
    assert rows[2][0] == 3
    assert rows[2][1] == bytes_large
    assert rows[2][2] == 10000
    
    # Row 4: Very large data (100KB)
    assert rows[3][0] == 4
    assert rows[3][1] == bytes_very_large
    assert rows[3][2] == 100000
    
    # Row 5: NULL value
    assert rows[4][0] == 5
    assert rows[4][1] is None
    assert rows[4][2] is None

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_varbinary_max_empty_bytes(client_context):
    """Regression test for microsoft/mssql-python#547 (VARBINARY(MAX) variant).

    Empty byte arrays into VARBINARY(MAX) previously desynced the PLP stream:
    a 4-byte zero-length PLP chunk header IS the PLP terminator per MS-TDS,
    so writing chunk_len=0 plus an explicit terminator caused the server to
    misread the next column (SQL error 4815 "invalid column length" or 4804).

    Includes a trailing NOT NULL column to detect any residual stream
    misalignment after an empty PLP value.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyEmptyVarbinaryMax"
    cursor.execute(
        f"CREATE TABLE {table_name} ("
        f"id INT, vb_max VARBINARY(MAX) NULL, tail INT NOT NULL)"
    )

    bytes4 = bytes([0xDE, 0xAD, 0xBE, 0xEF])
    empty_bytes = bytes()

    data = [
        (1, bytes4, 100),
        (2, empty_bytes, 200),
        (3, None, 300),
        (4, bytes4, 400),
    ]

    result = cursor.bulkcopy(table_name, iter(data), batch_size=1000, timeout=30)
    assert result is not None
    assert result["rows_copied"] == 4

    cursor.execute(
        f"SELECT id, vb_max, DATALENGTH(vb_max), tail FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()

    assert rows[0] == (1, bytes4, 4, 100)
    # SQL Server stores empty VARBINARY(MAX) but DATALENGTH may report 0
    # while the value round-trips as an empty bytes object.
    assert rows[1][0] == 2
    assert rows[1][1] == empty_bytes or rows[1][1] is None
    assert rows[1][2] == 0
    assert rows[1][3] == 200
    assert rows[2] == (3, None, None, 300)
    assert rows[3] == (4, bytes4, 4, 400)

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_int_to_binary_fails(client_context):
    """Test that integers cannot be directly sent as binary data.
    
    Based on .NET SqlBulkCopy behavior, int-to-binary coercion should fail.
    Numbers must be explicitly converted to bytes before bulk copy.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with binary column
    table_name = "#BulkCopyIntToBinaryTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, binary_col BINARY(16))")

    # Prepare test data with integer values (not bytes)
    data = [
        (1, 42),  # Integer instead of bytes - should fail
    ]

    # Execute bulk copy and expect an error
    error_raised = False
    error_message = ""
    try:
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=1000, timeout=30,
        )
        print(f"No error raised. Result: {result}")
    except (ValueError, TypeError, RuntimeError) as e:
        error_raised = True
        error_message = str(e).lower()
        print(f"Expected error caught: {e}")

    # Verify that an error was raised about type mismatch
    assert error_raised, "Expected an error for int-to-binary coercion"
    assert any(word in error_message for word in ["type", "convert", "int", "binary", "bytes"]), \
        f"Expected type conversion error, got: {error_message}"

    # Close connection
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_binary_with_explicit_encoding(client_context):
    """Test bulk copy with string data explicitly encoded to bytes.
    
    Demonstrates the correct way to send string data to binary columns:
    explicitly encode strings to bytes using UTF-8 or other encoding.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with varbinary column
    table_name = "BulkCopyBinaryEncodedStringTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, varbinary_col VARBINARY(100))")

    # Prepare test data - strings explicitly encoded to bytes
    data = [
        (1, "Hello".encode('utf-8')),           # UTF-8 encoding
        (2, "World".encode('utf-8')),           # UTF-8 encoding
        (3, "Binary Data".encode('ascii')),     # ASCII encoding
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=30,
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3

    # Verify data was stored as bytes
    cursor.execute(f"SELECT id, varbinary_col FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    
    assert rows[0][0] == 1 and rows[0][1] == b"Hello"
    assert rows[1][0] == 2 and rows[1][1] == b"World"
    assert rows[2][0] == 3 and rows[2][1] == b"Binary Data"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_binary_empty_bytes(client_context):
    """Test bulk copy with empty byte arrays (zero-length)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyBinaryEmptyTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, binary_col BINARY(16), varbinary_col VARBINARY(100))"
    )

    # Prepare test data with empty byte arrays
    empty_bytes = bytes()
    bytes16 = bytes([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                     0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10])
    
    data = [
        (1, bytes16, empty_bytes),     # Empty to VARBINARY
        (2, empty_bytes, bytes16),     # Empty to BINARY(16) - should be zero-padded
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=30,
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 2

    # Verify data
    cursor.execute(f"SELECT id, binary_col, varbinary_col, DATALENGTH(varbinary_col) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 2
    
    # Row 1: Empty VARBINARY should be stored as zero-length
    assert rows[0][0] == 1
    assert rows[0][2] == empty_bytes
    assert rows[0][3] == 0  # VARBINARY length = 0
    
    # Row 2: Empty to BINARY(16) should be all zeros
    assert rows[1][0] == 2
    assert rows[1][1] == bytes(16)  # All zeros
    assert len(rows[1][1]) == 16

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_binary_bytearray_type(client_context):
    """Test that bytearray type (mutable) also works, not just bytes (immutable)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyBinaryByteArrayTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, varbinary_col VARBINARY(100))")

    # Prepare test data using bytearray instead of bytes
    data = [
        (1, bytearray([0x01, 0x02, 0x03, 0x04])),
        (2, bytearray(b"Hello")),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=30,
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 2

    # Verify data
    cursor.execute(f"SELECT id, varbinary_col FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 2
    
    assert rows[0][0] == 1 and rows[0][1] == bytes([0x01, 0x02, 0x03, 0x04])
    assert rows[1][0] == 2 and rows[1][1] == b"Hello"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()
