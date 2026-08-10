# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Integration tests for the sync cursor and rowcount against a live server.

These require a reachable SQL Server (via the standard `client_context`
fixture / `.env`) and are marked `integration`, so they skip when no server is
configured. The mock-server-backed behavioral tests for the sync/async cursors
live in `tests/rs-only-tests/test_sync_async_cursor_mock.py`.
"""

import pytest

import mssql_py_core


@pytest.mark.integration
def test_sync_cursor_execute(client_context):
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cursor = conn.sync_cursor()
        cursor.execute("SELECT 1 AS value")
        result = cursor.fetchone()
        assert result is not None
        assert result[0] == 1
        cursor.close()
    finally:
        conn.close()


@pytest.mark.integration
def test_sync_cursor_fetchall(client_context):
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cursor = conn.sync_cursor()
        cursor.execute("SELECT 1 AS value UNION ALL SELECT 2 UNION ALL SELECT 3")
        results = cursor.fetchall()
        assert [row[0] for row in results] == [1, 2, 3]
        cursor.close()
    finally:
        conn.close()


@pytest.mark.integration
def test_sync_cursor_matches_async(client_context):
    """The sync fetch path returns byte-identical rows to the async path."""
    query = "SELECT 1 AS a, CAST('x' AS NVARCHAR(10)) AS b UNION ALL SELECT 2, 'y'"

    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        ac = conn.cursor()
        ac.execute(query)
        async_rows = ac.fetchall()
        ac.close()

        sc = conn.sync_cursor()
        sc.execute(query)
        sync_rows = sc.fetchall()
        sc.close()

        assert sync_rows == async_rows
    finally:
        conn.close()


@pytest.mark.integration
def test_rowcount_sync_equals_async_select(client_context):
    query = "SELECT 1 AS value UNION ALL SELECT 2 UNION ALL SELECT 3"

    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        ac = conn.cursor()
        ac.execute(query)
        async_rowcount = ac.rowcount
        ac.fetchall()
        ac.close()

        sc = conn.sync_cursor()
        sc.execute(query)
        sync_rowcount = sc.rowcount
        sc.fetchall()
        sc.close()

        assert sync_rowcount == async_rowcount
    finally:
        conn.close()


@pytest.mark.integration
def test_dml_rowcount_async(client_context):
    """A count-bearing DML statement captures rowcount on the async path."""
    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        cursor = conn.cursor()
        cursor.execute("CREATE TABLE #l6_rowcount (id INT)")
        cursor.execute("INSERT INTO #l6_rowcount (id) VALUES (1), (2), (3)")
        assert cursor.rowcount == 3
        cursor.close()
    finally:
        conn.close()


@pytest.mark.integration
def test_sync_cursor_error_mid_fetch_recovers(client_context):
    """A fetch error on the sync edge surfaces, then the cell reverts to async
    so the connection stays usable (ruling 4: recover via into_async drain)."""
    # The conversion trips on the third row, after rows have started streaming.
    bad_query = (
        "SELECT CAST(value AS INT) AS n "
        "FROM (VALUES ('1'), ('2'), ('notanumber')) AS t(value)"
    )

    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        sc = conn.sync_cursor()
        sc.execute(bad_query)
        with pytest.raises(RuntimeError):
            sc.fetchall()
        sc.close()

        # The connection reverted to the async edge and remains usable.
        ac = conn.cursor()
        ac.execute("SELECT 1")
        assert ac.fetchone() == (1,)
        ac.close()
    finally:
        conn.close()


@pytest.mark.integration
def test_dml_rowcount_sync_equals_async(client_context):
    """DML never flips to the sync edge, so both cursors capture the same count."""
    dml = "INSERT INTO #l6_rowcount_parity (id) VALUES (1), (2)"

    conn = mssql_py_core.PyCoreConnection(client_context)
    try:
        ac = conn.cursor()
        ac.execute("CREATE TABLE #l6_rowcount_parity (id INT)")
        ac.execute(dml)
        async_rowcount = ac.rowcount
        ac.close()

        sc = conn.sync_cursor()
        sc.execute(dml)
        sync_rowcount = sc.rowcount
        sc.close()

        assert sync_rowcount == async_rowcount
    finally:
        conn.close()
