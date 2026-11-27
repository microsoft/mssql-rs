# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Bulk copy tests for DECIMAL data type."""
import pytest
import mssql_py_core
from decimal import Decimal, ROUND_DOWN, localcontext


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_basic(client_context):
    """Test cursor bulkcopy method with two decimal columns and explicit mappings."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with two decimal columns
    table_name = "BulkCopyTestTableDecimal"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(18, 4))")

    # Prepare test data - two columns, both decimal
    data = [
        (Decimal("1.00"), Decimal("100.5000")),
        (Decimal("2.50"), Decimal("200.7500")),
        (Decimal("3.99"), Decimal("300.9999")),
    ]

    # Execute bulk copy with explicit column mappings
    result = cursor.bulkcopy(
        table_name,
        iter(data),
        batch_size=1000,
        timeout=30,
        column_mappings=[
            (0, "id"),
            (1, "value"),
        ],  # Map tuple positions to columns
    )

    # Verify results
    assert result is not None
    assert result["rows_copied"] == 3
    assert result["batch_count"] == 1
    assert "elapsed_time" in result

    # Verify data was inserted correctly
    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == Decimal("1.00") and rows[0][1] == Decimal("100.5000")
    assert rows[1][0] == Decimal("2.50") and rows[1][1] == Decimal("200.7500")


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_auto_mapping(client_context):
    """Test cursor bulkcopy with automatic column mapping.

    Tests bulkcopy when no mappings are specified, including NULL value handling.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with two nullable decimal columns
    table_name = "BulkCopyAutoMapTableDecimal"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(18, 4))")

    # Prepare test data - two columns, both decimal, with NULL values
    data = [
        (Decimal("1.00"), Decimal("100.5000")),
        (Decimal("2.50"), None),  # NULL value in second column
        (None, Decimal("300.9999")),  # NULL value in first column
        (Decimal("4.75"), Decimal("400.1234")),
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

    # Verify data was inserted correctly, including NULL values
    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY COALESCE(id, 999)")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][0] == Decimal("1.00") and rows[0][1] == Decimal("100.5000")
    assert rows[1][0] == Decimal("2.50") and rows[1][1] is None  # Verify NULL in value column
    assert rows[2][0] == Decimal("4.75") and rows[2][1] == Decimal("400.1234")
    assert rows[3][0] is None and rows[3][1] == Decimal("300.9999")  # Verify NULL in id column

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_string_to_decimal_conversion(client_context):
    """Test cursor bulkcopy with string values that should convert to decimal columns.

    Tests type coercion when source data contains numeric strings but
    destination columns are DECIMAL type.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with two decimal columns
    table_name = "BulkCopyStringToDecimalTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(18, 4))")

    # Prepare test data - strings containing valid decimal numbers
    data = [
        ("1.00", "100.5000"),
        ("2.50", "200.7500"),
        ("3.99", "300.9999"),
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

    # Verify data was inserted correctly and converted to decimal
    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == Decimal("1.00") and rows[0][1] == Decimal("100.5000")
    assert rows[1][0] == Decimal("2.50") and rows[1][1] == Decimal("200.7500")
    assert rows[2][0] == Decimal("3.99") and rows[2][1] == Decimal("300.9999")

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_integer_to_decimal_conversion(client_context):
    """Test cursor bulkcopy with integer values that should convert to decimal columns.

    Tests type coercion when source data contains integers but
    destination columns are DECIMAL type.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with two decimal columns
    table_name = "BulkCopyIntToDecimalTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(18, 4))")

    # Prepare test data - integers that should convert to decimal
    data = [
        (1, 100),
        (2, 200),
        (3, 300),
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

    # Verify data was inserted correctly and converted to decimal
    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == Decimal("1.00") and rows[0][1] == Decimal("100.0000")
    assert rows[1][0] == Decimal("2.00") and rows[1][1] == Decimal("200.0000")
    assert rows[2][0] == Decimal("3.00") and rows[2][1] == Decimal("300.0000")

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_float_to_decimal_conversion(client_context):
    """Test cursor bulkcopy with float values that should convert to decimal columns.

    Tests type coercion when source data contains floats but
    destination columns are DECIMAL type.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with two decimal columns
    table_name = "BulkCopyFloatToDecimalTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(18, 4))")

    # Prepare test data - floats that should convert to decimal
    data = [
        (1.25, 100.5),
        (2.50, 200.75),
        (3.99, 300.9999),
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

    # Verify data was inserted correctly and converted to decimal
    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 3
    assert rows[0][0] == Decimal("1.25") and rows[0][1] == Decimal("100.5000")
    assert rows[1][0] == Decimal("2.50") and rows[1][1] == Decimal("200.7500")
    assert rows[2][0] == Decimal("3.99") and rows[2][1] == Decimal("300.9999")

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_null_to_non_nullable_column(client_context):
    """Test cursor bulkcopy with null value for non-nullable decimal column.

    Tests that the client-side metadata validation catches attempts to insert
    null into a non-nullable column and raises an appropriate conversion error.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a temp table with a single non-nullable decimal column
    table_name = "#BulkCopyNonNullableTableDecimal"
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2) NOT NULL)")

    # Prepare test data with a null value
    data = [
        (Decimal("1.00"),),
        (None,),  # This should trigger a conversion error
        (Decimal("3.00"),),
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
def test_cursor_bulkcopy_decimal_invalid_string_to_decimal_conversion(client_context):
    """Test cursor bulkcopy with invalid string that cannot be converted to decimal.

    Tests that client-side type coercion properly validates string-to-decimal conversion
    and fails with an appropriate error when the string is not a valid decimal number.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a temp table with decimal columns
    table_name = "#BulkCopyInvalidStringTableDecimal"
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(18, 4))")

    # Prepare test data with invalid string that cannot be parsed as decimal
    data = [
        ("1.00", "100.5000"),
        ("not_a_number", "200.7500"),  # This should trigger a conversion error
        ("3.99", "300.9999"),
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
    ), "Expected a ValueError to be raised for invalid string-to-decimal conversion"
    assert (
        "invalid decimal string" in error_message
    ), f"Expected 'invalid decimal string' message, got: {error_message}"
    # Verify that the original parse error message is preserved
    assert (
        "invalid" in error_message and "not_a_number" in error_message
    ), f"Expected original parse error details to be preserved, got: {error_message}"

    # Close connection - temp table will be automatically dropped
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_precision_overflow(client_context):
    """Test cursor bulkcopy with decimal value exceeding precision.

    When a decimal value has more digits than the column precision allows,
    it should be rejected during type coercion.

    Expected behavior:
    - Decimal value with precision <= column precision converts successfully
    - Decimal value with precision > column precision triggers validation error
    - Error raised: "Decimal value exceeds precision for DECIMAL column"
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a temp table with DECIMAL(10, 2) column (max 10 total digits, 2 after decimal)
    table_name = "#BulkCopyDecimalPrecisionOverflowTable"
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(10, 2))")

    # Prepare test data with decimal value exceeding precision
    # DECIMAL(10, 2) allows max: 99999999.99 (8 digits before, 2 after decimal)
    data = [
        (Decimal("1.00"), Decimal("100.50")),
        (Decimal("2.00"), Decimal("999999999.99")),  # 9 digits before decimal, exceeds DECIMAL(10, 2)
        (Decimal("3.00"), Decimal("300.99")),
    ]

    # Execute bulk copy and expect precision validation error
    error_raised = False
    error_message = ""
    try:
        result = cursor.bulkcopy(
            table_name, iter(data), batch_size=1000, timeout=30
        )
        # If we get here, no error was raised (unexpected)
        print(f"No error raised. Result: {result}")
    except (ValueError, RuntimeError) as e:
        error_raised = True
        error_message = str(e).lower()
        print(f"Expected error caught: {e}")

    # Verify that an error was raised about precision overflow
    assert (
        error_raised
    ), "Expected an error for decimal value exceeding column precision"
    assert (
        "precision" in error_message or "exceeds" in error_message or "overflow" in error_message
    ), f"Expected precision error, got: {error_message}"
    assert (
        "decimal" in error_message
    ), f"Expected DECIMAL column type in error, got: {error_message}"

    # Close connection - temp table will be automatically dropped
    conn.close()


@pytest.mark.integration
@pytest.mark.parametrize("sql_type", ["DECIMAL", "NUMERIC"])
@pytest.mark.parametrize("source_type", [Decimal, str])
def test_cursor_bulkcopy_decimal_rounds_to_destination(
    client_context, sql_type, source_type
):
    """Scale reduction is exact and independent of Python's decimal context."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()
    table_name = "#BulkCopyDecimalRounding"
    cases = [
        ("123.456", "123.46", "2.5", "3", "1.2", "1.2000"),
        ("-123.456", "-123.46", "-2.5", "-3", "-1.2", "-1.2000"),
        ("123.455", "123.46", "2.49", "2", "1.2345", "1.2345"),
        ("-123.455", "-123.46", "-2.49", "-2", "0", "0.0000"),
        ("123.454", "123.45", "0.5", "1", "-0", "0.0000"),
        ("-123.454", "-123.45", "-0.5", "-1", "1E+2", "100.0000"),
        ("1.230000", "1.23", "0.49", "0", "1.0000", "1.0000"),
        ("-0.000", "0.00", "-0.49", "0", "0.0000", "0.0000"),
        ("99.995", "100.00", "9.999", "10", "0.00005", "0.0001"),
    ]
    try:
        cursor.execute(
            f"CREATE TABLE {table_name} (id INT, value {sql_type}(10,2), "
            f"whole {sql_type}(10,0), padded {sql_type}(10,4), "
            f"wide {sql_type}(38,2))"
        )
        data = [
            (i, source_type(value), source_type(whole), source_type(padded),
             source_type("123456789012345678901234567890123456.785"))
            for i, (value, _, whole, _, padded, _) in enumerate(cases)
        ] + [(len(cases), None, None, None, None)]
        expected = [
            (i, Decimal(value), Decimal(whole), Decimal(padded),
             Decimal("123456789012345678901234567890123456.79"))
            for i, (_, value, _, whole, _, padded) in enumerate(cases)
        ] + [(len(cases), None, None, None, None)]
        with localcontext() as context:
            context.prec = 6
            context.rounding = ROUND_DOWN
            result = cursor.bulkcopy(
                table_name, iter(data), batch_size=3, timeout=30
            )
        assert result["rows_copied"] == len(data)
        assert result["batch_count"] == 4
        cursor.execute(f"SELECT * FROM {table_name} ORDER BY id")
        assert [tuple(row) for row in cursor.fetchall()] == expected
    finally:
        conn.close()


@pytest.mark.integration
@pytest.mark.parametrize("use_internal_transaction", [False, True])
@pytest.mark.parametrize("overflow", ["999.995", "-999.995"])
def test_cursor_bulkcopy_decimal_rounded_overflow(
    client_context, use_internal_transaction, overflow
):
    """A rounding carry must fail without undoing previously completed batches."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()
    table_name = "#BulkCopyDecimalRoundedOverflow"
    try:
        cursor.execute(f"CREATE TABLE {table_name} (id INT, value DECIMAL(5,2))")
        with pytest.raises(ValueError, match="exceeds target precision"):
            cursor.bulkcopy(
                table_name,
                iter([(0, Decimal("1.005")), (1, Decimal("-1.005")),
                      (2, Decimal("2.345")), (3, Decimal(overflow))]),
                batch_size=2,
                use_internal_transaction=use_internal_transaction,
            )
        cursor.execute(f"SELECT * FROM {table_name} ORDER BY id")
        assert [tuple(row) for row in cursor.fetchall()] == [
            (0, Decimal("1.01")), (1, Decimal("-1.01"))
        ]
        result = cursor.bulkcopy(
            table_name,
            iter([(3, Decimal("2.345"))]),
            use_internal_transaction=use_internal_transaction,
        )
        assert result["rows_copied"] == 1
        cursor.execute(f"SELECT value FROM {table_name} WHERE id = 3")
        assert cursor.fetchall()[0][0] == Decimal("2.35")
    finally:
        conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_negative_values(client_context):
    """Test cursor bulkcopy with negative decimal values.

    Tests that negative decimal values are correctly handled during bulk copy.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with decimal columns
    table_name = "BulkCopyNegativeDecimalTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(18, 4))")

    # Prepare test data with negative decimal values
    data = [
        (Decimal("-1.00"), Decimal("-100.5000")),
        (Decimal("-2.50"), Decimal("-200.7500")),
        (Decimal("3.99"), Decimal("-300.9999")),
        (Decimal("-4.75"), Decimal("400.1234")),
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
    cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
    rows = cursor.fetchall()
    assert len(rows) == 4
    assert rows[0][0] == Decimal("-4.75") and rows[0][1] == Decimal("400.1234")
    assert rows[1][0] == Decimal("-2.50") and rows[1][1] == Decimal("-200.7500")
    assert rows[2][0] == Decimal("-1.00") and rows[2][1] == Decimal("-100.5000")
    assert rows[3][0] == Decimal("3.99") and rows[3][1] == Decimal("-300.9999")

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()


@pytest.mark.integration
def test_cursor_bulkcopy_decimal_zero_values(client_context):
    """Test cursor bulkcopy with zero decimal values.

    Tests that zero decimal values with different representations are correctly handled.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    # Create a test table with decimal columns
    table_name = "BulkCopyZeroDecimalTable"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id DECIMAL(10, 2), value DECIMAL(18, 4))")

    # Prepare test data with zero decimal values
    data = [
        (Decimal("0.00"), Decimal("0.0000")),
        (Decimal("0"), Decimal("0")),
        (0, 0.0),
        ("0.00", "0.0000"),
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

    # Verify data was inserted correctly - all should be zero
    cursor.execute(f"SELECT id, value FROM {table_name}")
    rows = cursor.fetchall()
    assert len(rows) == 4
    for row in rows:
        assert row[0] == Decimal("0.00")
        assert row[1] == Decimal("0.0000")

    # Cleanup
    cursor.execute(f"DROP TABLE {table_name}")
    conn.close()
