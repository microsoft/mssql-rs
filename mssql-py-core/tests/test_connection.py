# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for PyCoreConnection functionality."""
import time
from uuid import uuid4

import pytest
import mssql_py_core


def _session_count(cursor, application_name):
    cursor.execute(
        "SELECT COUNT(*) FROM sys.dm_exec_sessions "
        f"WHERE program_name = '{application_name}'"
    )
    return cursor.fetchone()[0]


def _wait_for_session_close(cursor, application_name):
    deadline = time.monotonic() + 10
    while _session_count(cursor, application_name) != 0:
        assert time.monotonic() < deadline, (
            f"Session for {application_name!r} still exists 10 seconds after close"
        )
        time.sleep(0.1)


@pytest.fixture
def monitored_connection(client_context, connection):
    application_name = f"mssql-py-close-{uuid4().hex}"
    context = {**client_context, "application_name": application_name}
    conn = mssql_py_core.PyCoreConnection(context)
    try:
        mon_cursor = connection.cursor()
        assert _session_count(mon_cursor, application_name) == 1, (
            "Session with the unique application name should exist before close"
        )
        yield conn, mon_cursor, application_name
    finally:
        conn.close()


def test_module_import():
    """Test that the mssql_py_core module can be imported."""
    assert mssql_py_core is not None


@pytest.mark.integration
def test_connection_close_terminates_server_session(monitored_connection):
    """Verify close() removes the session identified by its unique application name."""
    conn, mon_cursor, application_name = monitored_connection
    cursor = conn.cursor()
    cursor.execute("SELECT 1")
    assert cursor.fetchone()[0] == 1

    conn.close()
    _wait_for_session_close(mon_cursor, application_name)


@pytest.mark.integration
def test_connection_close_rejects_new_cursor(client_context):
    """Verify cursor() raises after close()."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    conn.close()

    with pytest.raises(RuntimeError, match="closed"):
        conn.cursor()


@pytest.mark.integration
def test_connection_close_rejects_query_on_existing_cursor(client_context):
    """Verify execute() on an existing cursor fails after connection close."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    cursor = conn.cursor()
    cursor.execute("SELECT 1")
    cursor.fetchone()

    conn.close()

    with pytest.raises(RuntimeError):
        cursor.execute("SELECT 1")


@pytest.mark.integration
def test_connection_close_after_bulkcopy(monitored_connection):
    """Verify close() works correctly after a bulk copy operation."""
    conn, mon_cursor, application_name = monitored_connection
    cursor = conn.cursor()

    table_name = "TestConnectionCloseBCP"
    cursor.execute(
        f"IF OBJECT_ID('{table_name}', 'U') IS NOT NULL DROP TABLE {table_name}"
    )
    cursor.execute(f"CREATE TABLE {table_name} (id INT, val NVARCHAR(5))")
    try:
        rows = [(i, f"r{i}") for i in range(10)]
        cursor.bulkcopy(table_name, iter(rows), batch_size=100, timeout=30, table_lock=True)

        cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        assert cursor.fetchone()[0] == 10

        conn.close()
        _wait_for_session_close(mon_cursor, application_name)

        # Data should persist (committed before close)
        mon_cursor.execute(f"SELECT COUNT(*) FROM {table_name}")
        assert mon_cursor.fetchone()[0] == 10
    finally:
        conn.close()
        mon_cursor.execute(f"DROP TABLE {table_name}")


@pytest.mark.integration
def test_connection_double_close(client_context):
    """Verify calling close() twice does not error."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    conn.close()
    conn.close()  # should be a no-op
