# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""L6 sync + default cursor tests against the mock TDS server.

These exercise the shared reactor-free sync core wiring without a live SQL
Server:

- The **sync** cursor (`conn.sync_cursor()`) flips the shared client cell to the
  reactor-free `TdsSyncClient` edge for its row-pull hot loop on a **plaintext**
  connection, then reverts to the async edge for control-plane work.
- The **default** cursor (`conn.cursor()`) drives the async core via `block_on`
  and never flips (regression guard — its public API is unchanged). It is a
  blocking cursor; the genuine coroutine cursor is `conn.async_cursor()`
  (`PyCoreAsyncCursor`), covered by the async-focused tests.
- `rowcount` is an additive read-only property sourced identically on both
  cursors, so sync == default by construction.
- On a **TLS** connection the sync edge is `NotEligible`, so the sync cursor
  transparently falls back to the async `block_on` path (byte-identical rows).

The mock's built-in query registry answers `SELECT 1` and
`SELECT CAST(1 AS BIGINT), 2, 3`; unknown SELECTs return an empty result set.
"""

import pytest

try:
    import mssql_mock_tds

    MOCK_TDS_PY_AVAILABLE = True
except ImportError:
    MOCK_TDS_PY_AVAILABLE = False

pytestmark = pytest.mark.skipif(
    not MOCK_TDS_PY_AVAILABLE,
    reason="mssql_mock_tds not available. Build it with: cd mssql-mock-tds-py && maturin develop",
)


def _ctx(server, encryption="Optional"):
    """Build a sql_auth client context pointed at the mock server."""
    return {
        "server": server.sql_address,
        "database": "master",
        "user_name": "sa",
        "password": "unused-by-mock",
        "encryption": encryption,
        "trust_server_certificate": True,
    }


@pytest.fixture
def plaintext_server():
    """A plaintext (non-TLS) mock TDS server; the sync edge is eligible here."""
    server = mssql_mock_tds.PyMockTdsServer(port=0, tls=False)
    with server:
        yield server


@pytest.fixture
def tls_server():
    """A TLS mock TDS server; the sync edge reports NotEligible here.

    TLS against the mock is environment-sensitive (native-tls handshake); when
    the local host cannot complete it, the dependent test skips rather than
    hanging the suite — mirroring the existing FedAuth TLS tests.
    """
    try:
        server = mssql_mock_tds.PyMockTdsServer(port=0, tls=True)
    except Exception as exc:  # noqa: BLE001 - surfaced as a skip below
        pytest.skip(f"TLS mock server unavailable in this environment: {exc}")
    with server:
        yield server


def _connect(ctx):
    import mssql_py_core

    return mssql_py_core.PyCoreConnection(ctx)


# ---------------------------------------------------------------------------
# Sync cursor — reactor-free edge on a plaintext connection
# ---------------------------------------------------------------------------


def test_sync_cursor_fetchone_plaintext(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        cur = conn.sync_cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone() == (1,)
        assert cur.fetchone() is None
        cur.close()
    finally:
        conn.close()


def test_sync_cursor_fetchall_plaintext(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        cur = conn.sync_cursor()
        cur.execute("SELECT CAST(1 AS BIGINT), 2, 3")
        assert cur.fetchall() == [(1, 2, 3)]
        cur.close()
    finally:
        conn.close()


def test_sync_cursor_fetchmany_plaintext(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        cur = conn.sync_cursor()
        cur.execute("SELECT 1")
        assert cur.fetchmany(1) == [(1,)]
        assert cur.fetchmany(1) == []
        cur.close()
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Default cursor — unchanged public API (regression guard)
# ---------------------------------------------------------------------------


def test_async_cursor_fetchone_plaintext(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone() == (1,)
        assert cur.fetchone() is None
        cur.close()
    finally:
        conn.close()


def test_sync_matches_async_rows_plaintext(plaintext_server):
    """Sync fetch is byte-identical to the async path for the same statement."""
    conn = _connect(_ctx(plaintext_server))
    try:
        query = "SELECT CAST(1 AS BIGINT), 2, 3"

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


# ---------------------------------------------------------------------------
# rowcount parity (additive property; same oracle on both cursors)
# ---------------------------------------------------------------------------


def test_rowcount_sync_equals_async_select(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        ac = conn.cursor()
        ac.execute("SELECT 1")
        async_rowcount = ac.rowcount
        ac.fetchall()
        ac.close()

        sc = conn.sync_cursor()
        sc.execute("SELECT 1")
        sync_rowcount = sc.rowcount
        sc.fetchall()
        sc.close()

        assert sync_rowcount == async_rowcount
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# Flip / revert discipline — the shared cell recovers to the async edge
# ---------------------------------------------------------------------------


def test_sync_then_async_reuse(plaintext_server):
    """After a sync fetch, a control-plane op on the default cursor reverts the
    shared cell and succeeds (revert-before-control-plane)."""
    conn = _connect(_ctx(plaintext_server))
    try:
        sc = conn.sync_cursor()
        sc.execute("SELECT 1")
        assert sc.fetchone() == (1,)
        sc.close()

        ac = conn.cursor()
        ac.execute("SELECT 1")
        assert ac.fetchone() == (1,)
        ac.close()
    finally:
        conn.close()


def test_async_then_sync_reuse(plaintext_server):
    conn = _connect(_ctx(plaintext_server))
    try:
        ac = conn.cursor()
        ac.execute("SELECT 1")
        assert ac.fetchone() == (1,)
        ac.close()

        sc = conn.sync_cursor()
        sc.execute("SELECT 1")
        assert sc.fetchone() == (1,)
        sc.close()
    finally:
        conn.close()


def test_sync_cursor_close_before_exhaust_reverts(plaintext_server):
    """Closing a sync cursor mid-result-set reverts the cell so the connection
    stays usable for the next statement."""
    conn = _connect(_ctx(plaintext_server))
    try:
        sc = conn.sync_cursor()
        sc.execute("SELECT CAST(1 AS BIGINT), 2, 3")
        sc.close()  # close without draining rows -> revert_to_async

        again = conn.sync_cursor()
        again.execute("SELECT 1")
        assert again.fetchone() == (1,)
        again.close()
    finally:
        conn.close()


def test_sync_cursor_reexecute_reverts(plaintext_server):
    """Re-executing on the same sync cursor reverts the previous sync edge
    before driving the new control-plane execute."""
    conn = _connect(_ctx(plaintext_server))
    try:
        sc = conn.sync_cursor()
        sc.execute("SELECT 1")
        assert sc.fetchone() == (1,)

        sc.execute("SELECT CAST(1 AS BIGINT), 2, 3")
        assert sc.fetchall() == [(1, 2, 3)]
        sc.close()
    finally:
        conn.close()


# ---------------------------------------------------------------------------
# TLS connection — sync edge NotEligible, transparent async fallback
# ---------------------------------------------------------------------------


def test_sync_cursor_tls_fallback(tls_server):
    """On a TLS connection the sync edge is NotEligible; the sync cursor falls
    back to the async block_on path and returns byte-identical rows."""
    ctx = _ctx(tls_server, encryption="Optional")
    try:
        conn = _connect(ctx)
    except Exception as exc:  # noqa: BLE001 - env-sensitive TLS handshake
        pytest.skip(f"TLS connect to mock unavailable in this environment: {exc}")
    try:
        sc = conn.sync_cursor()
        sc.execute("SELECT 1")
        assert sc.fetchone() == (1,)
        sc.close()
    finally:
        conn.close()
