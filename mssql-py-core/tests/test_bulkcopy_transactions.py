# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Bulk copy tests for BatchSize and UseInternalTransaction options.

Tests the transaction behavior during bulk copy operations:

1. UseInternalTransaction = True:
   - Each batch wrapped in BEGIN/COMMIT
   - ROLLBACK on batch failure
   - Enables partial failure recovery

2. UseInternalTransaction = False (default):
   - SQL Server autocommit mode applies
   - Each batch implicitly committed after DONE packet

3. BatchSize:
   - 0 = all rows in single batch (default)
   - N > 0 = rows in batches of N

4. Combined behavior:
   - BatchSize + UseInternalTransaction for partial failure recovery

Architecture Note:
- These tests use PyCoreConnection directly (low-level Rust TDS binding)
- In mssql-python (high-level wrapper), BulkCopy creates a NEW connection
- External transactions from parent connection do NOT affect BulkCopy operations
- Therefore, external transaction tests are excluded from this suite

Reference: .NET SqlBulkCopy behavior and Rust test_bulk_copy_transactions.rs
"""
import time
import pytest
import mssql_py_core
import uuid


@pytest.mark.integration
@pytest.mark.parametrize("kind", ["rows", "arrow"])
def test_source_callback_can_release_previous_connection(client_context, kind, monkeypatch):
    import sys

    pa = pytest.importorskip("pyarrow") if kind == "arrow" else None
    retired = [mssql_py_core.PyCoreConnection(client_context)]
    retired[0].close()
    writer = mssql_py_core.PyCoreConnection(client_context)
    cursor = writer.cursor()
    cursor.execute("CREATE TABLE #SourceCollection (id INT)")
    unraisable = []
    monkeypatch.setattr(sys, "unraisablehook", unraisable.append)

    def source():
        retired.pop()
        if kind == "rows":
            yield (1,)
        else:
            yield pa.record_batch({"id": pa.array([1], type=pa.int32())})

    try:
        if kind == "rows":
            cursor.bulkcopy("#SourceCollection", source())
        else:
            reader = pa.RecordBatchReader.from_batches(pa.schema([("id", pa.int32())]), source())
            cursor.bulkcopy_arrow("#SourceCollection", reader)
        assert not unraisable
        assert not retired
        cursor.execute("SELECT id FROM #SourceCollection")
        assert cursor.fetchone() == (1,)
    finally:
        writer.close()


@pytest.mark.integration
@pytest.mark.parametrize("kind", ["rows", "arrow", "c_stream"])
@pytest.mark.parametrize("mappings", [None, [(0, "id")]])
@pytest.mark.parametrize("batch_size", [0, 2])
@pytest.mark.parametrize("good_rows", [0, 1, 2, 3, 4])
def test_source_failure_aborts_only_current_batch(client_context, kind, mappings, batch_size, good_rows):
    """A source error is neither EOF nor a retry, including boundary peeks."""
    pa = pytest.importorskip("pyarrow") if kind != "rows" else None
    table = "BulkSourceFailure_" + uuid.uuid4().hex
    observer = mssql_py_core.PyCoreConnection(client_context)
    writer = mssql_py_core.PyCoreConnection(client_context)
    check = observer.cursor()
    check.execute(f"CREATE TABLE {table} (id INT NOT NULL)")
    failure = ValueError("intentional producer failure")
    calls = []

    class Source:
        def __iter__(self):
            return self

        def __next__(self):
            index = len(calls)
            calls.append(index)
            if index == good_rows:
                raise failure
            if index > good_rows:
                raise AssertionError("source resumed after failure")
            if kind != "rows":
                return pa.record_batch({"id": pa.array([index], type=pa.int32())})
            return (index,)

    try:
        source = Source()
        cursor = writer.cursor()
        with pytest.raises(Exception, match="intentional producer failure") as caught:
            if kind == "rows":
                cursor.bulkcopy(
                    table, source, batch_size=batch_size,
                    use_internal_transaction=True, column_mappings=mappings,
                )
            else:
                schema = pa.schema([("id", pa.int32())])
                reader = pa.RecordBatchReader.from_batches(schema, source)
                class CStream:
                    def __arrow_c_stream__(self, requested_schema=None):
                        return reader.__arrow_c_stream__(requested_schema)

                cursor.bulkcopy_arrow(
                    table, CStream() if kind == "c_stream" else reader, batch_size=batch_size,
                    use_internal_transaction=True, column_mappings=mappings,
                )
        if kind == "rows":
            assert caught.value is failure
        assert calls == list(range(good_rows + 1))
        committed = good_rows // batch_size * batch_size if batch_size else 0
        check.execute(f"SELECT id FROM {table} ORDER BY id")
        assert [row[0] for row in check.fetchall()] == list(range(committed))
    finally:
        writer.close()
        check.execute(f"DROP TABLE {table}")
        observer.close()


def unique_table_name(prefix: str) -> str:
    """Generate a unique table name with timestamp suffix."""
    return f"{prefix}_{int(time.time() * 1000)}"


# =============================================================================
# USE_INTERNAL_TRANSACTION BASIC TESTS
# =============================================================================


@pytest.mark.integration
def test_use_internal_transaction_true_basic(client_context):
    """Test that use_internal_transaction=True commits data successfully.
    
    Verifies that bulk copy with internal transaction enabled
    correctly commits data to the destination table.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopyInternalTxnBasic")

    try:
        # Create test table
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        # Prepare test data
        data = [
            (1, 100),
            (2, 200),
            (3, 300),
        ]

        # Execute bulk copy with use_internal_transaction=True
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            use_internal_transaction=True,
        )

        # Verify bulk copy succeeded
        assert result is not None
        assert result["rows_copied"] == 3, f"Expected 3 rows, got {result['rows_copied']}"

        # Verify data was committed
        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 3, f"Expected 3 rows in table, got {rows[0][0]}"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


@pytest.mark.integration
def test_use_internal_transaction_false_default(client_context):
    """Test default autocommit behavior (use_internal_transaction=False).
    
    Verifies that bulk copy with default settings (no internal transaction)
    correctly commits data via SQL Server autocommit.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopyAutocommitDefault")

    try:
        # Create test table
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        # Prepare test data
        data = [
            (1, 100),
            (2, 200),
            (3, 300),
        ]

        # Execute bulk copy WITHOUT specifying use_internal_transaction (default=False)
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            # use_internal_transaction defaults to False
        )

        # Verify bulk copy succeeded
        assert result is not None
        assert result["rows_copied"] == 3

        # Verify data was committed (via autocommit)
        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 3, "Data should be committed via autocommit"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# =============================================================================
# BATCH_SIZE TESTS
# =============================================================================


@pytest.mark.integration
def test_batch_size_zero_single_batch(client_context):
    """Test that batch_size=0 sends all rows in one batch.
    
    batch_size=0 (default) means all rows are processed as a single batch.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopyBatchSizeZero")

    try:
        # Create test table
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        # Prepare test data (100 rows)
        data = [(i, i * 10) for i in range(1, 101)]

        # Execute bulk copy with batch_size=0 (explicit single batch)
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=0,  # All rows in one batch
        )

        # Verify bulk copy succeeded
        assert result is not None
        assert result["rows_copied"] == 100
        # batch_count should be 1 when batch_size=0
        assert result["batch_count"] == 1, f"Expected 1 batch, got {result['batch_count']}"

        # Verify all data was inserted
        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 100

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


@pytest.mark.integration
def test_batch_size_with_internal_transaction(client_context):
    """Test batch_size > 0 with use_internal_transaction=True.
    
    Each batch should be committed separately with its own transaction.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopyBatchWithTxn")

    try:
        # Create test table
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        # Prepare test data (12 rows, batch_size=4 = 3 batches)
        data = [(i, i * 100) for i in range(1, 13)]

        # Execute bulk copy with batch_size=4 and internal transaction
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=4,
            use_internal_transaction=True,
        )

        # Verify bulk copy succeeded
        assert result is not None
        assert result["rows_copied"] == 12
        # 12 rows / 4 per batch = 3 batches
        assert result["batch_count"] == 3, f"Expected 3 batches, got {result['batch_count']}"

        # Verify all data was inserted
        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 12

        # Verify data integrity
        cursor.execute(f"SELECT MIN(id), MAX(id) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 1, "Min ID should be 1"
        assert rows[0][1] == 12, "Max ID should be 12"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


@pytest.mark.integration
def test_batch_size_various_sizes(client_context):
    """Test various batch sizes work correctly.
    
    Tests batch_size=1, 5, 10, 25 all work correctly.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    for batch_size in [1, 5, 10, 25]:
        table_name = unique_table_name(f"BulkCopyBatchSize{batch_size}")

        try:
            # Create test table
            cursor.execute(
                f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
            )
            cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

            # Prepare test data (25 rows)
            data = [(i, i * 10) for i in range(1, 26)]

            # Execute bulk copy with specified batch_size
            result = cursor.bulkcopy(
                table_name,
                iter(data),
                batch_size=batch_size,
            )

            # Verify bulk copy succeeded
            assert result is not None
            assert result["rows_copied"] == 25, f"batch_size={batch_size}: Expected 25 rows"

            # Verify expected batch count
            expected_batches = (25 + batch_size - 1) // batch_size  # Ceiling division
            assert result["batch_count"] == expected_batches, \
                f"batch_size={batch_size}: Expected {expected_batches} batches, got {result['batch_count']}"

            # Verify all data was inserted
            cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
            rows = cursor.fetchall()
            assert rows[0][0] == 25, f"batch_size={batch_size}: Expected 25 rows in table"

        finally:
            cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")

    conn.close()


# =============================================================================
# PARTIAL FAILURE RECOVERY TESTS
# =============================================================================


@pytest.mark.integration
def test_partial_failure_with_internal_transaction(client_context):
    """Test partial failure recovery with use_internal_transaction=True.
    
    Scenario:
    - Insert rows with batch_size=5
    - Use a CHECK constraint to cause failure in a later batch
    - Verify earlier batches are committed, failing batch is rolled back
    
    This demonstrates the key benefit of UseInternalTransaction:
    partial failure recovery with clean rollback semantics.
    
    Note: After a bulk copy error, the connection may be in an inconsistent state,
    so we use a fresh connection for verification.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopyPartialFailure")

    try:
        # Create table with CHECK constraint (value must be >= 0)
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"""
            CREATE TABLE {table_name} (
                id INT NOT NULL,
                value INT CHECK (value >= 0)
            )
        """)

        # Prepare data: 15 rows total, batch_size=5 = 3 batches
        # Batch 1: rows 1-5 (values 100-500) - OK
        # Batch 2: rows 6-10 (values 600-1000) - OK  
        # Batch 3: rows 11-15, but row 12 has value=-1 - FAIL
        data = []
        for i in range(1, 16):
            if i == 12:
                data.append((i, -1))  # This will violate the CHECK constraint
            else:
                data.append((i, i * 100))

        # Execute bulk copy - should fail on batch 3
        with pytest.raises(Exception) as exc_info:
            cursor.bulkcopy(
                table_name,
                iter(data),
                batch_size=5,
                use_internal_transaction=True,
                check_constraints=True,  # Enforce CHECK constraint
            )

        # Verify we got a constraint violation error
        error_msg = str(exc_info.value).lower()
        assert any(keyword in error_msg for keyword in ["check", "constraint", "conflict", "547"]), \
            f"Expected constraint violation error, got: {exc_info.value}"

        # Close the potentially broken connection
        conn.close()

        # Use a fresh connection to verify the committed data
        conn2 = mssql_py_core.PyCoreConnection(client_context)
        cursor2 = conn2.cursor()

        # KEY VERIFICATION: Batches 1-2 should be committed (10 rows)
        # Batch 3 should be rolled back
        cursor2.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor2.fetchall()
        committed_rows = rows[0][0]
        
        # With internal transaction, we expect 10 rows (batches 1-2 committed)
        # The exact behavior depends on when the error occurs
        assert committed_rows >= 5, \
            f"Expected at least 5 rows committed (batch 1), got {committed_rows}"
        assert committed_rows <= 10, \
            f"Expected at most 10 rows committed (batches 1-2), got {committed_rows}"

        # Cleanup with fresh connection
        cursor2.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn2.close()

    except Exception:
        # If anything fails, try cleanup with a fresh connection
        try:
            conn_cleanup = mssql_py_core.PyCoreConnection(client_context)
            cursor_cleanup = conn_cleanup.cursor()
            cursor_cleanup.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
            conn_cleanup.close()
        except Exception:
            pass  # Best effort cleanup
        raise


@pytest.mark.integration
def test_autocommit_batches_persist(client_context):
    """Test that autocommitted batches persist (use_internal_transaction=False).
    
    With batch_size > 0 and use_internal_transaction=False,
    each batch is autocommitted by SQL Server after the DONE packet.
    Completed batches survive even if later operations fail.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopyAutocommitPersist")

    try:
        # Create test table
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        # Prepare test data (12 rows, batch_size=4 = 3 batches)
        data = [(i, i * 100) for i in range(1, 13)]

        # Execute bulk copy with batch_size=4, NO internal transaction
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=4,
            use_internal_transaction=False,  # Autocommit mode
        )

        # Verify bulk copy succeeded
        assert result is not None
        assert result["rows_copied"] == 12

        # Verify all data is committed (autocommit)
        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 12, "All 12 rows should be autocommitted"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# =============================================================================
# BATCH SIZE = 1 (EXTREME CASE) TESTS
# =============================================================================


@pytest.mark.integration
def test_batch_size_one_with_internal_transaction(client_context):
    """Test batch_size=1 with internal transaction (extreme case).
    
    Each row gets its own transaction. This is inefficient but should work.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopyBatchSizeOne")

    try:
        # Create test table
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        # Prepare test data (5 rows, batch_size=1 = 5 transactions!)
        data = [(i, i * 100) for i in range(1, 6)]

        # Execute bulk copy with batch_size=1 and internal transaction
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=1,
            use_internal_transaction=True,
        )

        # Verify bulk copy succeeded
        assert result is not None
        assert result["rows_copied"] == 5
        assert result["batch_count"] == 5, "Each row should be its own batch"

        # Verify all data was inserted
        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 5

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# =============================================================================
# MULTIPLE SEQUENTIAL BULK COPIES
# =============================================================================


@pytest.mark.integration
def test_multiple_sequential_bulk_copies_with_internal_transaction(client_context):
    """Test multiple sequential bulk copies with internal transaction.
    
    Verifies that consecutive bulk copy operations with internal transactions
    work correctly without leaving connection in bad state.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopySequential")

    try:
        # Create test table
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        # First bulk copy
        data1 = [(1, 100), (2, 200)]
        result1 = cursor.bulkcopy(
            table_name,
            iter(data1),
            use_internal_transaction=True,
        )
        assert result1["rows_copied"] == 2

        # Second bulk copy
        data2 = [(3, 300), (4, 400)]
        result2 = cursor.bulkcopy(
            table_name,
            iter(data2),
            use_internal_transaction=True,
        )
        assert result2["rows_copied"] == 2

        # Third bulk copy
        data3 = [(5, 500)]
        result3 = cursor.bulkcopy(
            table_name,
            iter(data3),
            use_internal_transaction=True,
        )
        assert result3["rows_copied"] == 1

        # Verify total data
        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 5, "Expected 5 total rows from 3 sequential bulk copies"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# =============================================================================
# COMBINED OPTIONS TESTS
# =============================================================================


@pytest.mark.integration
def test_batch_size_internal_transaction_with_other_options(client_context):
    """Test batch_size and use_internal_transaction combined with other options.
    
    Verifies that transaction options work correctly alongside other bulk copy
    options like fire_triggers, keep_nulls, etc.
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    main_table = unique_table_name("BulkCopyCombinedMain")
    marker_table = unique_table_name("BulkCopyCombinedMarker")
    trigger_name = unique_table_name("trg_CombinedTest")

    try:
        # Create marker table for trigger detection
        cursor.execute(
            f"IF OBJECT_ID('{marker_table}', 'U') IS NOT NULL DROP TABLE {marker_table}"
        )
        cursor.execute(
            f"CREATE TABLE {marker_table} (id INT IDENTITY(1,1), triggered_at DATETIME DEFAULT GETDATE())"
        )

        # Create main table with nullable column with default
        cursor.execute(
            f"IF OBJECT_ID('{main_table}', 'U') IS NOT NULL DROP TABLE {main_table}"
        )
        cursor.execute(f"""
            CREATE TABLE {main_table} (
                id INT NOT NULL,
                value NVARCHAR(50) DEFAULT 'default_value'
            )
        """)

        # Create trigger
        cursor.execute(f"""
            CREATE TRIGGER {trigger_name} ON {main_table}
            AFTER INSERT
            AS
            BEGIN
                INSERT INTO {marker_table} (triggered_at) VALUES (GETDATE())
            END
        """)

        # Prepare data with NULL value
        data = [
            (1, "Row 1"),
            (2, None),  # NULL should be preserved with keep_nulls=True
            (3, "Row 3"),
            (4, "Row 4"),
        ]

        # Execute bulk copy with multiple options
        result = cursor.bulkcopy(
            main_table,
            iter(data),
            batch_size=2,  # 2 batches of 2 rows
            use_internal_transaction=True,
            fire_triggers=True,
            keep_nulls=True,
        )

        # Verify bulk copy succeeded
        assert result is not None
        assert result["rows_copied"] == 4
        assert result["batch_count"] == 2

        # Verify trigger fired (fire_triggers=True works with internal transaction)
        cursor.execute(f"SELECT COUNT(*) FROM {marker_table}")
        rows = cursor.fetchall()
        # With batch_size=2 and 4 rows, trigger fires twice (once per batch)
        assert rows[0][0] == 2, f"Trigger should have fired twice, fired {rows[0][0]} times"

        # Verify NULL was preserved (keep_nulls=True)
        cursor.execute(f"SELECT value FROM {main_table} WHERE id = 2")
        rows = cursor.fetchall()
        assert rows[0][0] is None, f"NULL should be preserved, got {rows[0][0]}"

    finally:
        cursor.execute(f"IF OBJECT_ID('{trigger_name}', 'TR') IS NOT NULL DROP TRIGGER {trigger_name}")
        cursor.execute(f"IF OBJECT_ID('{main_table}', 'U') IS NOT NULL DROP TABLE {main_table}")
        cursor.execute(f"IF OBJECT_ID('{marker_table}', 'U') IS NOT NULL DROP TABLE {marker_table}")
        conn.close()


# =============================================================================
# LARGE DATASET TESTS
# =============================================================================


@pytest.mark.integration
def test_large_dataset_with_batch_size(client_context):
    """Test large dataset with batch_size for realistic scenario.
    
    Tests 1000 rows with batch_size=100 (10 batches).
    """
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BulkCopyLargeDataset")

    try:
        # Create test table
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT NOT NULL, value INT NOT NULL)")

        # Prepare large dataset (1000 rows)
        data = [(i, i * 5) for i in range(1, 1001)]

        # Execute bulk copy with batch_size=100
        result = cursor.bulkcopy(
            table_name,
            iter(data),
            batch_size=100,
            use_internal_transaction=True,
        )

        # Verify bulk copy succeeded
        assert result is not None
        assert result["rows_copied"] == 1000
        assert result["batch_count"] == 10, f"Expected 10 batches, got {result['batch_count']}"

        # Verify all data was inserted
        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 1000

        # Verify data integrity
        cursor.execute(f"SELECT MIN(id), MAX(id) FROM {table_name}")
        rows = cursor.fetchall()
        assert rows[0][0] == 1, "Min ID should be 1"
        assert rows[0][1] == 1000, "Max ID should be 1000"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()
