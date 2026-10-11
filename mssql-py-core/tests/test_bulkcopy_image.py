# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Bulk copy tests for IMAGE data type with varying content sizes.

Note: IMAGE is a legacy data type (deprecated in favor of VARBINARY(MAX)).
However, these tests ensure backward compatibility with legacy databases.
"""
import pytest
import mssql_py_core


@pytest.mark.integration
def test_cursor_bulkcopy_image_basic(client_context):
    """Test cursor bulkcopy method with image columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with image columns
    table_name = "BulkCopyTestTableImage"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, data IMAGE, thumbnail IMAGE)"
    )

    # Prepare test data - byte arrays
    bytes16 = bytes([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                     0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10])
    bytes32 = bytes(range(32))
    bytes64 = bytes(range(64))
    
    data = [
        (1, bytes16, bytes32),
        (2, bytes32, bytes64),
        (3, bytes64, bytes16),
    ]

    # Execute bulk copy with explicit column mappings
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000,
        timeout=30,
        column_mappings=[
            (0, "id"),
            (1, "data"),
            (2, "thumbnail"),
        ],
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, data, thumbnail FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 1 and rows[0][1] == bytes16 and rows[0][2] == bytes32
    assert rows[1][0] == 2 and rows[1][1] == bytes32 and rows[1][2] == bytes64
    assert rows[2][0] == 3 and rows[2][1] == bytes64 and rows[2][2] == bytes16

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_image_auto_mapping(client_context):
    """Test cursor bulkcopy with automatic column mapping and NULL values.

    Tests bulkcopy when no mappings are specified, including NULL value handling.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with nullable image columns
    table_name = "BulkCopyAutoMapTableImage"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, data IMAGE)")

    # Prepare test data with NULL values
    bytes10 = bytes(range(10))
    bytes20 = bytes(range(20))
    
    data = [
        (1, bytes10),
        (2, None),  # NULL value in data column
        (None, bytes20),  # NULL value in id column
        (4, bytes10),
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
    cursor.execute(f"SELECT id, data FROM {table_name} ORDER BY COALESCE(id, 999)")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][0] == 1 and rows[0][1] == bytes10
    assert rows[1][0] == 2 and rows[1][1] is None  # Verify NULL in data column
    assert rows[2][0] == 4 and rows[2][1] == bytes10
    assert rows[3][0] is None and rows[3][1] == bytes20  # Verify NULL in id column

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_image_large_data(client_context):
    """Test cursor bulkcopy with IMAGE columns containing large binary data.

    Tests that large binary data is correctly inserted into IMAGE columns.
    IMAGE can store up to 2GB of binary data.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with IMAGE column
    table_name = "BulkCopyLargeDataImageTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, large_data IMAGE)"
    )

    # Prepare test data with large binary arrays
    # Creating binary data larger than typical VARBINARY sizes (8000 bytes)
    data = [
        (1, bytes(range(256)) * 40),   # 10,240 bytes (40 * 256)
        (2, bytes(range(256)) * 200),  # 51,200 bytes (200 * 256)
        (3, bytes(range(256)) * 400),  # 102,400 bytes (400 * 256)
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
        f"SELECT id, large_data, DATALENGTH(large_data) as byte_count FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 1
    assert rows[0][1] == bytes(range(256)) * 40
    assert rows[0][2] == 10240  # Byte count
    
    assert rows[1][0] == 2
    assert rows[1][1] == bytes(range(256)) * 200
    assert rows[1][2] == 51200
    
    assert rows[2][0] == 3
    assert rows[2][1] == bytes(range(256)) * 400
    assert rows[2][2] == 102400

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_image_to_varbinary(client_context):
    """Test cursor bulkcopy from byte arrays to VARBINARY(MAX) column.
    
    Tests that IMAGE type is compatible with VARBINARY(MAX) as they're both binary types.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with VARBINARY(MAX) column
    table_name = "BulkCopyImageToVarbinaryTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, data VARBINARY(MAX))"
    )

    # Prepare test data with byte arrays (would be IMAGE in source)
    bytes100 = bytes(range(100))
    bytes500 = bytes(range(256)) * 2  # 512 bytes
    
    data = [
        (1, bytes100),
        (2, bytes500),
    ]

    # Execute bulk copy - byte arrays should work with VARBINARY(MAX)
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 2
    assert result["batch_count"] == 1

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, data, DATALENGTH(data) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 2
    assert rows[0][0] == 1 and rows[0][1] == bytes100
    assert rows[0][2] == 100
    assert rows[1][0] == 2 and rows[1][1] == bytes500
    assert rows[1][2] == 512

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_varbinary_to_image(client_context):
    """Test cursor bulkcopy from byte arrays to IMAGE column.
    
    Tests that VARBINARY data can be bulk copied to IMAGE columns.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with IMAGE column
    table_name = "BulkCopyVarbinaryToImageTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, data IMAGE)"
    )

    # Prepare test data with byte arrays
    bytes100 = bytes(range(100))
    bytes500 = bytes(range(256)) * 2  # 512 bytes
    
    data = [
        (1, bytes100),
        (2, bytes500),
    ]

    # Execute bulk copy - byte arrays should work with IMAGE
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 2
    assert result["batch_count"] == 1

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, data, DATALENGTH(data) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 2
    assert rows[0][0] == 1 and rows[0][1] == bytes100
    assert rows[0][2] == 100
    assert rows[1][0] == 2 and rows[1][1] == bytes500
    assert rows[1][2] == 512

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_image_empty_bytes(client_context):
    """Test cursor bulkcopy with empty byte arrays to IMAGE columns.

    A zero-length IMAGE value is stored and read back as empty, not NULL:
    DATALENGTH() returns 0 rather than NULL (AB#49220).
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with IMAGE column
    table_name = "BulkCopyImageEmptyBytesTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, data IMAGE)")

    # Prepare test data with empty byte arrays
    empty_bytes = bytes()
    bytes10 = bytes(range(10))
    
    data = [
        (1, empty_bytes),
        (2, bytes10),
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 2
    assert result["batch_count"] == 1

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, data, DATALENGTH(data) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 2
    assert rows[0][0] == 1
    assert rows[0][1] == b""
    assert rows[0][2] == 0
    assert rows[1][0] == 2 and rows[1][1] == bytes10
    assert rows[1][2] == 10

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_image_null_to_non_nullable(client_context):
    """Test that NULL values to non-nullable IMAGE columns trigger an error.
    
    When attempting to insert NULL into a NOT NULL IMAGE column,
    the driver should raise a ValueError during type coercion.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with non-nullable IMAGE column
    table_name = "#BulkCopyImageNonNullableTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, image_col IMAGE NOT NULL)")

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
def test_cursor_bulkcopy_string_to_image_fails(client_context):
    """Test that strings cannot be directly sent as IMAGE data.
    
    Based on .NET SqlBulkCopy behavior, string-to-binary coercion should fail.
    Strings must be explicitly converted to bytes before bulk copy.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with IMAGE column
    table_name = "#BulkCopyStringToImageTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, image_col IMAGE)")

    # Prepare test data with string values (not bytes)
    data = [
        (1, "Hello World"),  # String instead of bytes - should fail
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
    assert error_raised, "Expected an error for string-to-IMAGE coercion"
    assert any(word in error_message for word in ["type", "mismatch", "string", "image"]), \
        f"Expected type conversion error, got: {error_message}"

    # Close connection
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_int_to_image_fails(client_context):
    """Test that integers cannot be directly sent as IMAGE data.
    
    Integers should not be implicitly converted to binary IMAGE data.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with IMAGE column
    table_name = "#BulkCopyIntToImageTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, image_col IMAGE)")

    # Prepare test data with integer value (not bytes)
    data = [
        (1, 12345),  # Integer instead of bytes - should fail
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
    assert error_raised, "Expected an error for int-to-IMAGE coercion"
    assert any(word in error_message for word in ["type", "mismatch", "int", "image"]), \
        f"Expected type conversion error, got: {error_message}"

    # Close connection
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_image_bytearray_type(client_context):
    """Test that Python bytearray (mutable bytes) works with IMAGE columns.
    
    bytearray is Python's mutable byte sequence type and should be
    handled the same as immutable bytes for IMAGE columns.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with IMAGE column
    table_name = "BulkCopyImageByteArrayTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, image_col IMAGE)")

    # Prepare test data using bytearray (mutable bytes)
    ba1 = bytearray([0x01, 0x02, 0x03, 0x04, 0x05])
    ba2 = bytearray(range(50))  # Larger bytearray
    
    data = [
        (1, ba1),
        (2, ba2),
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

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, image_col, DATALENGTH(image_col) FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 2
    
    # Row 1: bytearray should be stored as bytes
    assert rows[0][0] == 1
    assert rows[0][1] == bytes(ba1)
    assert rows[0][2] == 5
    
    # Row 2: Larger bytearray
    assert rows[1][0] == 2
    assert rows[1][1] == bytes(ba2)
    assert rows[1][2] == 50

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_image_multiple_columns(client_context):
    """Test bulk copy with multiple IMAGE columns in the same table.
    
    Tests that multiple IMAGE columns can be handled correctly in a single row.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with multiple IMAGE columns
    table_name = "BulkCopyImageMultiColTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(
        f"CREATE TABLE {table_name} (id INT, image1 IMAGE, image2 IMAGE, image3 IMAGE)"
    )

    # Prepare test data with different sizes per column
    bytes_small = bytes(range(10))
    bytes_medium = bytes(range(256)) * 10  # 2560 bytes
    bytes_large = bytes(range(256)) * 100  # 25600 bytes
    
    data = [
        (1, bytes_small, bytes_medium, bytes_large),
        (2, bytes_large, bytes_small, bytes_medium),
        (3, None, bytes_small, None),  # Mixed NULL values
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

    # Verify data was inserted correctly
    cursor.execute(
        f"SELECT id, DATALENGTH(image1), DATALENGTH(image2), DATALENGTH(image3) "
        f"FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()
    assert len(rows) == 3
    
    # Row 1: 10, 2560, 25600 bytes
    assert rows[0][0] == 1
    assert rows[0][1] == 10
    assert rows[0][2] == 2560
    assert rows[0][3] == 25600
    
    # Row 2: 25600, 10, 2560 bytes
    assert rows[1][0] == 2
    assert rows[1][1] == 25600
    assert rows[1][2] == 10
    assert rows[1][3] == 2560
    
    # Row 3: NULL, 10, NULL
    assert rows[2][0] == 3
    assert rows[2][1] is None  # NULL
    assert rows[2][2] == 10
    assert rows[2][3] is None  # NULL

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_image_large_batch(client_context):
    """Test bulk copy with a large batch of rows containing IMAGE data.
    
    Tests performance and correctness with 100 rows of IMAGE data.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyImageLargeBatchTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, image_col IMAGE)")

    # Generate 100 rows with varying IMAGE data sizes
    data = []
    for i in range(100):
        # Vary the size: some small, some medium, some large
        size = (i % 10 + 1) * 100  # 100 to 1000 bytes
        image_data = bytes(range(256)) * (size // 256 + 1)
        image_data = image_data[:size]  # Trim to exact size
        data.append((i, image_data))

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000, timeout=60,
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 100
    assert result["rows_per_second"] > 0

    # Verify count
    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    count = cursor.fetchone()[0]
    assert count == 100

    # Verify a sample of the data
    cursor.execute(f"SELECT id, DATALENGTH(image_col) FROM {table_name} WHERE id IN (0, 50, 99) ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == 0 and rows[0][1] == 100
    assert rows[1][0] == 50 and rows[1][1] == 100  # (50 % 10 + 1) * 100 = 100
    assert rows[2][0] == 99 and rows[2][1] == 1000  # (99 % 10 + 1) * 100 = 1000

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()
