# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Arrow bulk copy tests for bulk-copy options (cursor.bulkcopy_arrow).

Mirrors test_bulkcopy_options.py, but the source is a pyarrow.Table. The
options themselves are server-side BCP hints applied identically regardless of
whether the rows originate from Python tuples or Arrow buffers — they feed the
same Rust `BulkCopy` builder. Accordingly:

  * keep_nulls / keep_identity / batch_size are tested in full because the Arrow
    source materially changes the path (validity bitmap, identity column
    binding, TDS commit cadence vs Arrow batch boundaries).
  * fire_triggers / check_constraints have clear observable server effects and
    are exercised here too.
  * table_lock is a source-agnostic hint and gets a single smoke test (its deep
    behavior is covered by the tuple test_bulkcopy_options.py suite).
"""
import time

import pytest
import mssql_py_core

pa = pytest.importorskip("pyarrow")


def unique_table_name(prefix: str) -> str:
    """Generate a unique table name with timestamp suffix."""
    return f"{prefix}_{int(time.time() * 1000)}"


# ── fire_triggers ────────────────────────────────────────────────────────────


@pytest.mark.integration
def test_bulkcopy_arrow_fire_triggers_true(client_context):
    """fire_triggers=True causes INSERT triggers to fire (Arrow source)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    main_table = unique_table_name("BCArrowFireTriggersMain")
    marker_table = unique_table_name("BCArrowFireTriggersMarker")
    trigger_name = unique_table_name("trg_ArrowFireTriggers")

    try:
        cursor.execute(
            f"IF OBJECT_ID('{marker_table}', 'U') IS NOT NULL DROP TABLE {marker_table}"
        )
        cursor.execute(
            f"CREATE TABLE {marker_table} (id INT IDENTITY(1,1), triggered_at DATETIME DEFAULT GETDATE())"
        )
        cursor.execute(
            f"IF OBJECT_ID('{main_table}', 'U') IS NOT NULL DROP TABLE {main_table}"
        )
        cursor.execute(f"CREATE TABLE {main_table} (id INT, value NVARCHAR(50))")
        cursor.execute(f"""
            CREATE TRIGGER {trigger_name} ON {main_table}
            AFTER INSERT
            AS
            BEGIN
                INSERT INTO {marker_table} (triggered_at) VALUES (GETDATE())
            END
        """)

        source = pa.table(
            {
                "id": pa.array([1, 2, 3], type=pa.int32()),
                "value": pa.array(["Row 1", "Row 2", "Row 3"]),
            }
        )

        result = cursor.bulkcopy_arrow(
            main_table, source, fire_triggers=True, batch_size=1000
        )
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT COUNT(*) FROM {marker_table}")
        assert cursor.fetchall()[0][0] == 1, "Trigger should have fired once for the batch"

    finally:
        cursor.execute(f"IF OBJECT_ID('{trigger_name}', 'TR') IS NOT NULL DROP TRIGGER {trigger_name}")
        cursor.execute(f"IF OBJECT_ID('{main_table}', 'U') IS NOT NULL DROP TABLE {main_table}")
        cursor.execute(f"IF OBJECT_ID('{marker_table}', 'U') IS NOT NULL DROP TABLE {marker_table}")
        conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_fire_triggers_default_skips(client_context):
    """Omitting fire_triggers defaults to False (triggers skipped)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    main_table = unique_table_name("BCArrowDefTriggersMain")
    marker_table = unique_table_name("BCArrowDefTriggersMarker")
    trigger_name = unique_table_name("trg_ArrowDefTriggers")

    try:
        cursor.execute(
            f"IF OBJECT_ID('{marker_table}', 'U') IS NOT NULL DROP TABLE {marker_table}"
        )
        cursor.execute(
            f"CREATE TABLE {marker_table} (id INT IDENTITY(1,1), triggered_at DATETIME DEFAULT GETDATE())"
        )
        cursor.execute(
            f"IF OBJECT_ID('{main_table}', 'U') IS NOT NULL DROP TABLE {main_table}"
        )
        cursor.execute(f"CREATE TABLE {main_table} (id INT, value NVARCHAR(50))")
        cursor.execute(f"""
            CREATE TRIGGER {trigger_name} ON {main_table}
            AFTER INSERT
            AS
            BEGIN
                INSERT INTO {marker_table} (triggered_at) VALUES (GETDATE())
            END
        """)

        source = pa.table(
            {
                "id": pa.array([1, 2], type=pa.int32()),
                "value": pa.array(["Row 1", "Row 2"]),
            }
        )
        result = cursor.bulkcopy_arrow(main_table, source, batch_size=1000)
        assert result["rows_copied"] == 2

        cursor.execute(f"SELECT COUNT(*) FROM {marker_table}")
        assert cursor.fetchall()[0][0] == 0, "Trigger should NOT fire by default"

    finally:
        cursor.execute(f"IF OBJECT_ID('{trigger_name}', 'TR') IS NOT NULL DROP TRIGGER {trigger_name}")
        cursor.execute(f"IF OBJECT_ID('{main_table}', 'U') IS NOT NULL DROP TABLE {main_table}")
        cursor.execute(f"IF OBJECT_ID('{marker_table}', 'U') IS NOT NULL DROP TABLE {marker_table}")
        conn.close()


# ── check_constraints ────────────────────────────────────────────────────────


@pytest.mark.integration
def test_bulkcopy_arrow_check_constraints_true_enforces(client_context):
    """check_constraints=True enforces CHECK constraints (violation raises)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowCheckConstraints")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT, value INT CHECK (value > 0))")

        source = pa.table(
            {
                "id": pa.array([1, 2, 3], type=pa.int32()),
                "value": pa.array([100, -50, 200], type=pa.int32()),  # -50 violates
            }
        )

        with pytest.raises(Exception) as exc_info:
            cursor.bulkcopy_arrow(
                table_name, source, check_constraints=True, batch_size=1000
            )
        assert "conflicted with the check constraint" in str(exc_info.value).lower()

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_check_constraints_false_allows(client_context):
    """check_constraints=False (default) allows constraint-violating rows."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowNoCheckConstraints")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT, value INT CHECK (value > 0))")

        source = pa.table(
            {
                "id": pa.array([1, 2, 3], type=pa.int32()),
                "value": pa.array([100, -50, 200], type=pa.int32()),
            }
        )
        result = cursor.bulkcopy_arrow(
            table_name, source, check_constraints=False, batch_size=1000
        )
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT value FROM {table_name} WHERE id = 2")
        assert cursor.fetchall()[0][0] == -50

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# ── keep_nulls ───────────────────────────────────────────────────────────────


@pytest.mark.integration
def test_bulkcopy_arrow_keep_nulls_true_preserves(client_context):
    """keep_nulls=True preserves Arrow NULLs over the column default."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowKeepNulls")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT, value INT DEFAULT 999)")

        source = pa.table(
            {
                "id": pa.array([1, 2, 3], type=pa.int32()),
                "value": pa.array([100, None, 200], type=pa.int32()),
            }
        )
        result = cursor.bulkcopy_arrow(
            table_name, source, keep_nulls=True, batch_size=1000
        )
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT value FROM {table_name} WHERE id = 2")
        assert cursor.fetchall()[0][0] is None, "NULL preserved when keep_nulls=True"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_keep_nulls_false_uses_default(client_context):
    """keep_nulls=False replaces Arrow NULLs with the column default."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowNoKeepNulls")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT, value INT DEFAULT 999)")

        source = pa.table(
            {
                "id": pa.array([1, 2, 3], type=pa.int32()),
                "value": pa.array([100, None, 200], type=pa.int32()),
            }
        )
        result = cursor.bulkcopy_arrow(
            table_name, source, keep_nulls=False, batch_size=1000
        )
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT value FROM {table_name} WHERE id = 2")
        assert cursor.fetchall()[0][0] == 999, "default applied when keep_nulls=False"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# ── keep_identity ────────────────────────────────────────────────────────────


@pytest.mark.integration
def test_bulkcopy_arrow_keep_identity_true_uses_explicit(client_context):
    """keep_identity=True binds explicit IDENTITY values from the Arrow source."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowKeepIdentity")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(
            f"CREATE TABLE {table_name} (id INT IDENTITY(1,1), value NVARCHAR(50))"
        )

        source = pa.table(
            {
                "id": pa.array([100, 200, 300], type=pa.int32()),
                "value": pa.array(["Row at 100", "Row at 200", "Row at 300"]),
            }
        )
        result = cursor.bulkcopy_arrow(
            table_name, source, keep_identity=True, batch_size=1000
        )
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
        rows = cursor.fetchall()
        assert [r[0] for r in rows] == [100, 200, 300]
        assert rows[0][1] == "Row at 100"

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


@pytest.mark.integration
def test_bulkcopy_arrow_keep_identity_false_auto_generates(client_context):
    """keep_identity=False lets SQL Server auto-generate IDENTITY values."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowNoKeepIdentity")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(
            f"CREATE TABLE {table_name} (id INT IDENTITY(1,1), value NVARCHAR(50))"
        )

        # Source carries only the non-identity column.
        source = pa.table({"value": pa.array(["Row 1", "Row 2", "Row 3"])})
        result = cursor.bulkcopy_arrow(
            table_name,
            source,
            keep_identity=False,
            batch_size=1000,
            column_mappings=[(0, "value")],
        )
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT id, value FROM {table_name} ORDER BY id")
        rows = cursor.fetchall()
        assert [r[0] for r in rows] == [1, 2, 3]

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# ── table_lock (smoke: source-agnostic hint) ─────────────────────────────────


@pytest.mark.integration
def test_bulkcopy_arrow_table_lock_true(client_context):
    """table_lock=True is accepted and the load completes (smoke)."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    table_name = unique_table_name("BCArrowTableLock")
    try:
        cursor.execute(
            f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
        )
        cursor.execute(f"CREATE TABLE {table_name} (id INT, value NVARCHAR(50))")

        source = pa.table(
            {
                "id": pa.array([1, 2, 3], type=pa.int32()),
                "value": pa.array(["Row 1", "Row 2", "Row 3"]),
            }
        )
        result = cursor.bulkcopy_arrow(
            table_name, source, table_lock=True, batch_size=1000
        )
        assert result["rows_copied"] == 3

        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        assert cursor.fetchall()[0][0] == 3

    finally:
        cursor.execute(f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}")
        conn.close()


# ── combined ─────────────────────────────────────────────────────────────────


@pytest.mark.integration
def test_bulkcopy_arrow_multiple_options_combined(client_context):
    """Combine fire_triggers, check_constraints, keep_nulls, table_lock."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()

    main_table = unique_table_name("BCArrowCombinedMain")
    marker_table = unique_table_name("BCArrowCombinedMarker")
    trigger_name = unique_table_name("trg_ArrowCombined")

    try:
        cursor.execute(
            f"IF OBJECT_ID('{marker_table}', 'U') IS NOT NULL DROP TABLE {marker_table}"
        )
        cursor.execute(
            f"CREATE TABLE {marker_table} (id INT IDENTITY(1,1), triggered_at DATETIME DEFAULT GETDATE())"
        )
        cursor.execute(
            f"IF OBJECT_ID('{main_table}', 'U') IS NOT NULL DROP TABLE {main_table}"
        )
        cursor.execute(
            f"CREATE TABLE {main_table} (id INT, value INT CHECK (value >= 0) DEFAULT 999)"
        )
        cursor.execute(f"""
            CREATE TRIGGER {trigger_name} ON {main_table}
            AFTER INSERT
            AS
            BEGIN
                INSERT INTO {marker_table} (triggered_at) VALUES (GETDATE())
            END
        """)

        source = pa.table(
            {
                "id": pa.array([1, 2, 3, 4], type=pa.int32()),
                "value": pa.array([100, None, 200, 0], type=pa.int32()),
            }
        )
        result = cursor.bulkcopy_arrow(
            main_table,
            source,
            fire_triggers=True,
            check_constraints=True,
            keep_nulls=True,
            table_lock=True,
            batch_size=1000,
        )
        assert result["rows_copied"] == 4

        cursor.execute(f"SELECT value FROM {main_table} WHERE id = 2")
        assert cursor.fetchall()[0][0] is None  # keep_nulls preserved the NULL
        cursor.execute(f"SELECT COUNT(*) FROM {marker_table}")
        assert cursor.fetchall()[0][0] == 1  # trigger fired

    finally:
        cursor.execute(f"IF OBJECT_ID('{trigger_name}', 'TR') IS NOT NULL DROP TRIGGER {trigger_name}")
        cursor.execute(f"IF OBJECT_ID('{main_table}', 'U') IS NOT NULL DROP TABLE {main_table}")
        cursor.execute(f"IF OBJECT_ID('{marker_table}', 'U') IS NOT NULL DROP TABLE {marker_table}")
        conn.close()
