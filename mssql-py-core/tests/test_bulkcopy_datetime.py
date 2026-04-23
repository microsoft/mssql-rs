# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Bulk copy tests for DATETIME data type."""
import pytest
import datetime
import struct
import mssql_py_core


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_basic(client_context):
    """Test cursor bulkcopy method with two datetime columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with two datetime columns
    table_name = "BulkCopyTestTableDateTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (start_datetime DATETIME, end_datetime DATETIME)")

    # Prepare test data - two columns, both datetime
    data = [
        (datetime.datetime(2024, 1, 15, 9, 30, 0), datetime.datetime(2024, 1, 15, 17, 45, 30)),
        (datetime.datetime(2024, 2, 20, 8, 15, 45), datetime.datetime(2024, 2, 20, 16, 30, 15)),
        (datetime.datetime(2024, 3, 10, 10, 0, 0), datetime.datetime(2024, 3, 10, 18, 0, 0)),
    ]

    # Execute bulk copy with explicit column mappings
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000,
        timeout=30,
        column_mappings=[
            (0, "start_datetime"),
            (1, "end_datetime"),
        ],  # Map tuple positions to columns
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted by checking the count
    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    count = cursor.fetchone()[0]
    cursor.fetchall()  # Consume remaining result sets before next execute
    assert count == 3

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_auto_mapping(client_context):
    """Test cursor bulkcopy with automatic column mapping.

    Tests bulkcopy when no mappings are specified, including NULL value handling.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with two nullable datetime columns
    table_name = "BulkCopyAutoMapTableDateTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (start_datetime DATETIME, end_datetime DATETIME)")

    # Prepare test data - two columns, both datetime, with NULL values
    data = [
        (datetime.datetime(2024, 1, 15, 9, 30, 0), datetime.datetime(2024, 1, 15, 17, 45, 30)),
        (datetime.datetime(2024, 2, 20, 8, 15, 45), None),  # NULL value in second column
        (None, datetime.datetime(2024, 2, 20, 16, 30, 15)),  # NULL value in first column
        (datetime.datetime(2024, 3, 10, 10, 0, 0), datetime.datetime(2024, 3, 10, 18, 0, 0)),
    ]

    # Execute bulk copy WITHOUT column mappings - should auto-generate
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 4
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted by checking the count
    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    count = cursor.fetchone()[0]
    cursor.fetchall()  # Consume remaining result sets before next execute
    assert count == 4

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_string_to_datetime_conversion(client_context):
    """Test cursor bulkcopy with string values that should convert to datetime columns.

    Tests type coercion when source data contains datetime strings but
    destination columns are DATETIME type.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with two datetime columns
    table_name = "BulkCopyStringToDateTimeTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (start_datetime DATETIME, end_datetime DATETIME)")

    # Prepare test data - strings containing valid datetimes in ISO format
    data = [
        ("2024-01-15 09:30:00", "2024-01-15 17:45:30"),
        ("2024-02-20 08:15:45", "2024-02-20 16:30:15"),
        ("2024-03-10 10:00:00", "2024-03-10 18:00:00"),
    ]

    # Execute bulk copy without explicit mappings
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted by checking the count
    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    count = cursor.fetchone()[0]
    cursor.fetchall()  # Consume remaining result sets before next execute
    assert count == 3

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_null_to_non_nullable_column(client_context):
    """Test cursor bulkcopy with null value for non-nullable datetime column.

    Tests that the client-side metadata validation catches attempts to insert
    null into a non-nullable column and raises an appropriate conversion error.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a temp table with a single non-nullable datetime column
    table_name = "#BulkCopyNonNullableDateTimeTable"
    cursor.execute(f"CREATE TABLE {table_name} (start_datetime DATETIME NOT NULL)")

    # Prepare test data with a null value
    data = [
        (datetime.datetime(2024, 1, 15, 9, 30, 0),),
        (None,),  # This should trigger a conversion error
        (datetime.datetime(2024, 3, 10, 10, 0, 0),),
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
def test_cursor_bulkcopy_datetime_invalid_string_to_datetime_conversion(client_context):
    """Test cursor bulkcopy with invalid string that cannot be converted to datetime.

    Tests that client-side type coercion properly validates string-to-datetime conversion
    and fails with an appropriate error when the string is not a valid datetime.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a temp table with datetime columns
    table_name = "#BulkCopyInvalidStringDateTimeTable"
    cursor.execute(f"CREATE TABLE {table_name} (start_datetime DATETIME, end_datetime DATETIME)")

    # Prepare test data with invalid string that cannot be parsed as datetime
    data = [
        ("2024-01-15 09:30:00", "2024-01-15 17:45:30"),
        ("not_a_datetime", "2024-02-20 16:30:15"),  # This should trigger a conversion error
        ("2024-03-10 10:00:00", "2024-03-10 18:00:00"),
    ]

    # Execute bulk copy and expect a client-side ValueError
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
        print(f"Client-side ValueError caught: {e}")

    # Verify that an error was raised with appropriate message about conversion failure
    assert (
        error_raised
    ), "Expected a ValueError to be raised for invalid string-to-datetime conversion"
    assert (
        "cannot convert" in error_message or "conversion" in error_message or "invalid" in error_message
    ), f"Expected conversion error message, got: {error_message}"
    # Verify that the original parse error message is preserved
    assert (
        "not_a_datetime" in error_message or "invalid" in error_message
    ), f"Expected original parse error details to be preserved, got: {error_message}"

    # Close connection - temp table will be automatically dropped
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_boundary_values(client_context):
    """Test cursor bulkcopy with boundary datetime values.

    Tests that realistic DATETIME values are properly handled.
    SQL Server DATETIME range: 1753-01-01 00:00:00 to 9999-12-31 23:59:59.997
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a temp table with DATETIME columns
    table_name = "#BulkCopyDateTimeBoundaryTable"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, start_datetime DATETIME)")

    # Prepare test data with realistic boundary datetimes
    data = [
        (1, datetime.datetime(1753, 1, 1, 0, 0, 0)),  # Minimum datetime
        (2, datetime.datetime(9999, 12, 31, 23, 59, 59)),  # Maximum datetime (excluding milliseconds)
        (3, datetime.datetime(2024, 6, 15, 12, 0, 0)),  # Regular datetime
        (4, datetime.datetime(1900, 1, 1, 0, 0, 0)),  # Early date
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

    # Verify data was inserted by checking the count
    cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
    count = cursor.fetchone()[0]
    cursor.fetchall()  # Consume remaining result sets before next execute
    assert count == 4

    # Verify the actual datetime values match what was sent
    cursor.execute(f"SELECT id, start_datetime FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    cursor.fetchall()  # Consume remaining result sets
    
    # Check each row's datetime value
    assert len(rows) == 4
    for i, (expected_id, expected_datetime) in enumerate(data):
        actual_id, actual_datetime = rows[i]
        assert actual_id == expected_id, f"Row {i}: ID mismatch - expected {expected_id}, got {actual_id}"
        assert actual_datetime == expected_datetime, f"Row {i}: DateTime mismatch - expected {expected_datetime}, got {actual_datetime}"

    # Close connection - temp table will be automatically dropped
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_with_milliseconds(client_context):
    """Test cursor bulkcopy with datetime values that include milliseconds.

    Tests that DATETIME values with fractional seconds are properly handled.
    SQL Server DATETIME has approximately 3.33ms precision (rounded to increments of .000, .003, or .007 seconds).
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with DATETIME columns
    table_name = "BulkCopyTestDateTimeMilliseconds"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, precise_datetime DATETIME)")

    # Test datetime values with milliseconds
    # Note: DATETIME rounds to .000, .003, or .007 increments
    data = [
        (1, datetime.datetime(2024, 1, 15, 9, 30, 15, 0)),      # .000 seconds
        (2, datetime.datetime(2024, 2, 20, 14, 45, 30, 3000)),  # .003 seconds
        (3, datetime.datetime(2024, 3, 10, 0, 0, 0, 7000)),     # .007 seconds
        (4, datetime.datetime(2024, 12, 31, 23, 59, 59, 997000)),  # .997 seconds
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify bulk copy succeeded
    assert result is not None
    assert result["rows_copied"] == 4

    # Verify the datetimes were inserted correctly
    cursor.execute(f"SELECT id, precise_datetime FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    assert len(rows) == 4
    # Note: SQL Server DATETIME may round milliseconds to nearest .000, .003, or .007
    for i, (expected_id, expected_datetime) in enumerate(data):
        actual_id, actual_datetime = rows[i]
        assert actual_id == expected_id, f"Row {i}: ID mismatch"
        # Compare date and time portions
        assert actual_datetime.year == expected_datetime.year
        assert actual_datetime.month == expected_datetime.month
        assert actual_datetime.day == expected_datetime.day
        assert actual_datetime.hour == expected_datetime.hour
        assert actual_datetime.minute == expected_datetime.minute
        assert actual_datetime.second == expected_datetime.second
        # Microseconds may be rounded due to DATETIME precision (~3.33ms)
        # Allow for rounding to .000, .003, or .007 seconds
        expected_ms = expected_datetime.microsecond // 1000
        actual_ms = actual_datetime.microsecond // 1000
        # Check that it's within reasonable rounding tolerance (within 4ms)
        assert abs(actual_ms - expected_ms) <= 4, \
            f"Row {i}: Milliseconds differ significantly - expected {expected_ms}ms, got {actual_ms}ms"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_max_boundary_9999_12_31(client_context):
    """Test bulk copy with the maximum valid DATETIME value: 9999-12-31 23:59:59.997.
    
    This test verifies that the maximum datetime value is handled correctly.
    SQL Server DATETIME type has a maximum value of 9999-12-31 23:59:59.997.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyTestMaxDateTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, test_datetime DATETIME)")

    # Test boundary datetimes
    data = [
        (1, datetime.datetime(1753, 1, 1, 0, 0, 0)),           # Minimum datetime
        (2, datetime.datetime(2024, 6, 15, 12, 30, 45)),        # Regular datetime
        (3, datetime.datetime(9999, 12, 31, 23, 59, 58)),        # One second before max
        (4, datetime.datetime(9999, 12, 31, 23, 59, 59, 997000)),  # Maximum datetime - critical test!
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify bulk copy succeeded
    assert result is not None
    assert result["rows_copied"] == 4

    # Verify the datetimes were inserted correctly
    cursor.execute(f"SELECT id, test_datetime FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    assert len(rows) == 4
    assert rows[0][1] == datetime.datetime(1753, 1, 1, 0, 0, 0), "Minimum datetime should be inserted correctly!"
    assert rows[1][1].year == 2024 and rows[1][1].month == 6 and rows[1][1].day == 15
    assert rows[2][1] == datetime.datetime(9999, 12, 31, 23, 59, 58), "Near-max datetime should be inserted correctly!"
    # For the maximum datetime, check date and time separately due to potential rounding
    assert rows[3][1].year == 9999, "Max datetime year should be 9999!"
    assert rows[3][1].month == 12, "Max datetime month should be 12!"
    assert rows[3][1].day == 31, "Max datetime day should be 31!"
    assert rows[3][1].hour == 23, "Max datetime hour should be 23!"
    assert rows[3][1].minute == 59, "Max datetime minute should be 59!"
    assert rows[3][1].second == 59, "Max datetime second should be 59!"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_date_only(client_context):
    """Test bulk copy with DATETIME values containing only dates (no time component).
    
    Tests that datetime values with midnight time (00:00:00) are handled correctly.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyTestDateTimeDateOnly"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, test_datetime DATETIME)")

    # Test data with date-only values (time component is midnight)
    data = [
        (1, datetime.datetime(2024, 1, 1, 0, 0, 0)),
        (2, datetime.datetime(2024, 6, 15, 0, 0, 0)),
        (3, datetime.datetime(2024, 12, 31, 0, 0, 0)),
        (4, datetime.datetime(2000, 2, 29, 0, 0, 0)),  # Leap year
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify bulk copy succeeded
    assert result is not None
    assert result["rows_copied"] == 4

    # Verify the datetimes were inserted correctly
    cursor.execute(f"SELECT id, test_datetime FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    assert len(rows) == 4
    for i, (expected_id, expected_datetime) in enumerate(data):
        actual_id, actual_datetime = rows[i]
        assert actual_id == expected_id, f"Row {i}: ID mismatch"
        assert actual_datetime == expected_datetime, f"Row {i}: DateTime mismatch"
        # Verify time component is midnight
        assert actual_datetime.hour == 0 and actual_datetime.minute == 0 and actual_datetime.second == 0

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_various_years(client_context):
    """Test bulk copy with DATETIME values spanning various years.
    
    Tests that datetime values across different centuries and millennia are handled correctly.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyTestDateTimeYears"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, test_datetime DATETIME)")

    # Test data with dates spanning different centuries
    data = [
        (1, datetime.datetime(1753, 1, 1, 12, 0, 0)),   # Minimum year
        (2, datetime.datetime(1800, 7, 4, 15, 30, 0)),  # 19th century
        (3, datetime.datetime(1900, 1, 1, 0, 0, 0)),    # Turn of 20th century
        (4, datetime.datetime(2000, 1, 1, 0, 0, 0)),    # Y2K
        (5, datetime.datetime(2024, 6, 15, 12, 30, 45)), # Current era
        (6, datetime.datetime(2100, 12, 31, 23, 59, 59)), # 22nd century
        (7, datetime.datetime(9999, 12, 31, 23, 59, 59)), # Maximum year
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify bulk copy succeeded
    assert result is not None
    assert result["rows_copied"] == 7

    # Verify the datetimes were inserted correctly
    cursor.execute(f"SELECT id, test_datetime FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    assert len(rows) == 7
    for i, (expected_id, expected_datetime) in enumerate(data):
        actual_id, actual_datetime = rows[i]
        assert actual_id == expected_id, f"Row {i}: ID mismatch"
        assert actual_datetime.year == expected_datetime.year, f"Row {i}: Year mismatch"
        assert actual_datetime.month == expected_datetime.month, f"Row {i}: Month mismatch"
        assert actual_datetime.day == expected_datetime.day, f"Row {i}: Day mismatch"
        assert actual_datetime.hour == expected_datetime.hour, f"Row {i}: Hour mismatch"
        assert actual_datetime.minute == expected_datetime.minute, f"Row {i}: Minute mismatch"
        assert actual_datetime.second == expected_datetime.second, f"Row {i}: Second mismatch"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_leap_years(client_context):
    """Test bulk copy with DATETIME values on leap year dates.
    
    Tests that February 29th dates are handled correctly for leap years.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table
    table_name = "BulkCopyTestDateTimeLeapYears"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, test_datetime DATETIME)")

    # Test data with leap year dates (February 29th)
    data = [
        (1, datetime.datetime(2000, 2, 29, 12, 0, 0)),  # Leap year (divisible by 400)
        (2, datetime.datetime(2004, 2, 29, 15, 30, 0)),  # Regular leap year
        (3, datetime.datetime(2020, 2, 29, 8, 45, 30)),  # Recent leap year
        (4, datetime.datetime(2024, 2, 29, 23, 59, 59)),  # Leap year
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify bulk copy succeeded
    assert result is not None
    assert result["rows_copied"] == 4

    # Verify the datetimes were inserted correctly
    cursor.execute(f"SELECT id, test_datetime FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    assert len(rows) == 4
    for i, (expected_id, expected_datetime) in enumerate(data):
        actual_id, actual_datetime = rows[i]
        assert actual_id == expected_id, f"Row {i}: ID mismatch"
        assert actual_datetime == expected_datetime, f"Row {i}: DateTime mismatch"
        # Verify it's February 29th
        assert actual_datetime.month == 2 and actual_datetime.day == 29, \
            f"Row {i}: Expected February 29th, got {actual_datetime.month}/{actual_datetime.day}"

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_mixed_data_types(client_context):
    """Test bulk copy with mixed data types including datetime.
    
    Tests that datetime columns work correctly when mixed with other simple data types.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with mixed column types (INT, BIT, DATETIME)
    table_name = "BulkCopyTestMixedWithDateTime"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"""
        CREATE TABLE {table_name} (
            id INT,
            created_at DATETIME,
            is_active BIT,
            modified_at DATETIME
        )
    """)

    # Test data with mixed types (INT, DATETIME, BIT, DATETIME)
    data = [
        (1, datetime.datetime(2024, 1, 15, 9, 30, 0), True, datetime.datetime(2024, 1, 15, 10, 0, 0)),
        (2, datetime.datetime(2024, 2, 20, 8, 15, 45), False, datetime.datetime(2024, 2, 20, 14, 30, 0)),
        (3, datetime.datetime(2024, 3, 10, 10, 0, 0), True, None),  # NULL datetime
    ]

    # Execute bulk copy
    result = cursor.bulkcopy(
        table_name, iter(data), batch_size=1000, timeout=30
    )

    # Verify bulk copy succeeded
    assert result is not None
    assert result["rows_copied"] == 3

    # Verify the data was inserted correctly
    cursor.execute(f"SELECT id, created_at, is_active, modified_at FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    
    assert len(rows) == 3
    # Verify first row
    assert rows[0][0] == 1
    assert rows[0][1] == datetime.datetime(2024, 1, 15, 9, 30, 0)
    assert rows[0][2] == True
    assert rows[0][3] == datetime.datetime(2024, 1, 15, 10, 0, 0)
    
    # Verify second row
    assert rows[1][0] == 2
    assert rows[1][1] == datetime.datetime(2024, 2, 20, 8, 15, 45)
    assert rows[1][2] == False
    assert rows[1][3] == datetime.datetime(2024, 2, 20, 14, 30, 0)
    
    # Verify third row (with NULL datetime)
    assert rows[2][0] == 3
    assert rows[2][1] == datetime.datetime(2024, 3, 10, 10, 0, 0)
    assert rows[2][2] == True
    assert rows[2][3] is None  # NULL modified_at

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_tick_rounding(client_context):
    """Verify bulk copy rounds datetime to the nearest 1/300s tick.

    Regression test for https://github.com/microsoft/mssql-python/issues/516:
    bulk copy was truncating instead of rounding, producing values one tick
    lower than expected.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = "#BulkCopyDateTimeTickRounding"
    cursor.execute(f"CREATE TABLE {table_name} (id INT, dt DATETIME)")

    # Test values chosen at tick boundaries:
    #   tick = round(microsecond * 300 / 1_000_000)
    #   One SQL datetime tick = 1/300 s ≈ 3333.33 µs
    data = [
        # 123000 µs → 36.9 ticks → rounds to 37 → stored as .1233333
        (1, datetime.datetime(2025, 5, 16, 16, 33, 33, 123000)),
        # 1666 µs → 0.4998 ticks → rounds to 0
        (2, datetime.datetime(2025, 1, 1, 0, 0, 0, 1666)),
        # 1667 µs → 0.5001 ticks → rounds to 1
        (3, datetime.datetime(2025, 1, 1, 0, 0, 0, 1667)),
        # 5000 µs → exactly 1.5 ticks → rounds to 2
        (4, datetime.datetime(2025, 1, 1, 0, 0, 0, 5000)),
        # 0 µs → exact zero
        (5, datetime.datetime(2025, 6, 15, 12, 0, 0, 0)),
    ]

    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000,
        timeout=30,
        column_mappings=[(0, "id"), (1, "dt")],
    )
    assert result["rows_copied"] == len(data)

    # Read back the raw tick values via CONVERT(varbinary) and validate
    cursor.execute(
        f"SELECT id, CONVERT(varbinary(8), dt) FROM {table_name} ORDER BY id"
    )
    rows = cursor.fetchall()

    for row_id, raw_bytes in rows:
        _days, ticks = struct.unpack(">iI", raw_bytes)
        if row_id == 1:
            assert ticks == 17_883_937, f"Row 1: expected tick 17883937, got {ticks}"
        elif row_id == 2:
            frac_ticks = ticks % 300
            assert frac_ticks == 0, f"Row 2: expected 0 fractional ticks, got {frac_ticks}"
        elif row_id == 3:
            frac_ticks = ticks % 300
            assert frac_ticks == 1, f"Row 3: expected 1 fractional tick, got {frac_ticks}"
        elif row_id == 4:
            frac_ticks = ticks % 300
            assert frac_ticks == 2, f"Row 4: expected 2 fractional ticks, got {frac_ticks}"
        elif row_id == 5:
            assert ticks % 300 == 0, f"Row 5: expected 0 fractional ticks"

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_matches_execute(client_context):
    """Verify bulk copy and execute produce the same stored datetime value."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    bc_table = "#BulkCopyDateTimeMatchBC"
    ex_table = "#BulkCopyDateTimeMatchEX"
    for t in [bc_table, ex_table]:
        cursor.execute(f"CREATE TABLE {t} (dt DATETIME)")

    value = datetime.datetime(2025, 5, 16, 16, 33, 33, 123000)

    # Insert via bulk copy
    cursor.bulkcopy(bc_table, iter([(value,)]), batch_size=1, timeout=30)

    # Insert via execute (SQL Server handles rounding server-side)
    cursor.execute(
        f"INSERT INTO {ex_table} (dt) VALUES (CONVERT(datetime, '{value.strftime('%Y-%m-%d %H:%M:%S')}.{value.microsecond // 1000:03d}', 121))"
    )

    # Compare raw binary representations
    cursor.execute(f"SELECT CONVERT(varbinary(8), dt) FROM {bc_table}")
    bc_raw = cursor.fetchone()[0]
    cursor.fetchall()

    cursor.execute(f"SELECT CONVERT(varbinary(8), dt) FROM {ex_table}")
    ex_raw = cursor.fetchone()[0]
    cursor.fetchall()

    assert bc_raw == ex_raw, (
        f"Bulk copy and execute produced different datetime values: "
        f"bulk_copy={bc_raw.hex()}, execute={ex_raw.hex()}"
    )

    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_datetime_matches_mssql_python(client_context):
    """Verify mssql_py_core bulk copy matches mssql_python parameterized execute."""
    mssql_python = pytest.importorskip("mssql_python")

    conn_core = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn_core.cursor()

    value = datetime.datetime(2025, 5, 16, 16, 33, 33, 123000)

    # Insert via mssql_py_core bulk copy
    cursor.execute("CREATE TABLE #CoreDriverCmp (dt DATETIME)")
    cursor.bulkcopy("#CoreDriverCmp", iter([(value,)]), batch_size=1, timeout=30)
    cursor.execute("SELECT CONVERT(varbinary(8), dt) FROM #CoreDriverCmp")
    core_raw = cursor.fetchone()[0]
    cursor.fetchall()
    conn_core.close()

    # Insert via mssql_python parameterized execute
    server = client_context.get("server", "localhost")
    password = client_context.get("password", "")
    username = client_context.get("user_name", "sa")
    conn_str = (
        f"Server={server};Database=master;UID={username};PWD={password};"
        f"TrustServerCertificate=yes;Encrypt=Optional"
    )
    with mssql_python.connect(conn_str) as conn_py:
        with conn_py.cursor() as cur_py:
            cur_py.execute("CREATE TABLE #PyDriverCmp (dt DATETIME)")
            cur_py.execute(
                "INSERT INTO #PyDriverCmp (dt) VALUES (?)", (value,)
            )
        conn_py.commit()
        with conn_py.cursor() as cur_py:
            cur_py.execute("SELECT CONVERT(varbinary(8), dt) FROM #PyDriverCmp")
            py_raw = cur_py.fetchone()[0]

    assert core_raw == py_raw, (
        f"mssql_py_core and mssql_python produced different datetime values: "
        f"core={core_raw.hex()}, python={py_raw.hex()}"
    )
