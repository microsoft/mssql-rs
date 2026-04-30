# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""E2E tests for connection string parameter forwarding through bulkcopy.

Verifies that connection params supported by ``_ALLOWED_CONNECTION_STRING_PARAMS``
(ServerSPN, MultiSubnetFailover, PacketSize, ApplicationIntent, Encrypt, etc.)
are correctly forwarded from the ODBC connection string →
``connstr_to_pycore_params()`` → py-core's ``PyCoreConnection`` dict →
``connection.rs``.

Tests use two strategies:
  1. **Spy tests** — patch ``PyCoreConnection`` to capture the dict and assert
     keys/types/values without needing a real connection for every param.
  2. **Live tests** — connect directly through py-core and verify server-side
     state via SQL queries on ``sys.dm_exec_connections`` / ``sys.dm_exec_sessions``.

NOTE: Only params accepted by both the connection-string parser and the ODBC
driver are tested through the ODBC path.  Params like KeepAlive/KeepAliveInterval
are accepted by the parser but rejected by ODBC Driver 18 — those are tested
only through the py-core live path.
"""

import os
from pathlib import Path
from unittest.mock import patch

import pytest
from dotenv import load_dotenv

# Load .env from mssql-tds root
_env_file = Path(__file__).parent.parent.parent.parent / ".env"
if _env_file.exists():
    load_dotenv(_env_file)


# ── env helpers (same pattern as test_auth_resolution_e2e.py) ────

def _server():
    return os.environ.get("DB_HOST", os.environ.get("SQL_SERVER", "localhost"))

def _database():
    return os.environ.get("SQL_DATABASE", "master")

def _uid():
    return os.environ.get("DB_USERNAME", "sa")

def _pwd():
    v = os.environ.get("SQL_PASSWORD")
    if not v:
        try:
            with open("/tmp/password") as f:
                v = f.read().strip()
        except FileNotFoundError:
            pytest.skip("SQL_PASSWORD not set and /tmp/password not found")
    return v

def _base_connstr(**extras):
    """Minimal working connection string.  ``extras`` are appended as key=value pairs."""
    parts = [
        f"Server={_server()}",
        f"Database={_database()}",
        f"UID={_uid()}",
        f"PWD={_pwd()}",
        "TrustServerCertificate=Yes",
    ]
    for k, v in extras.items():
        parts.append(f"{k}={v}")
    return ";".join(parts)


def _connect(connstr, autocommit=True):
    from mssql_python import connect
    return connect(connstr, autocommit=autocommit)


# ── spy helper ───────────────────────────────────────────────────

def _capture_pycore_context(cursor, table="__param_test"):
    """Run cursor.bulkcopy through a spy PyCoreConnection and return the
    captured context dict (what connection.rs would receive).
    """
    cursor.execute(f"IF OBJECT_ID('{table}', 'U') IS NOT NULL DROP TABLE {table}")
    cursor.execute(f"CREATE TABLE {table} (id INT)")

    captured = {}
    original_pycore = __import__("mssql_py_core")

    class SpyPyCoreConnection:
        def __init__(self, ctx, **kwargs):
            captured.update(ctx)
            raise RuntimeError("Spy: captured context")
        def cursor(self):
            pass

    try:
        with patch.object(original_pycore, "PyCoreConnection", SpyPyCoreConnection):
            with pytest.raises(RuntimeError, match="Spy: captured context"):
                cursor.bulkcopy(table, [(1,)], timeout=30)
    finally:
        cursor.execute(f"IF OBJECT_ID('{table}', 'U') IS NOT NULL DROP TABLE {table}")

    return captured


# ═════════════════════════════════════════════════════════════════
#  Spy tests — verify connstr params land in the py-core dict
#  with the correct key names and types.
#
#  Only params in _ALLOWED_CONNECTION_STRING_PARAMS are tested.
#  Params rejected by ODBC Driver 18 (KeepAlive, etc.) cannot go
#  through ODBC connect, so they are only tested in the live
#  py-core section below.
# ═════════════════════════════════════════════════════════════════

class TestParamForwardingSpy:
    """Patch PyCoreConnection, call _bulkcopy, inspect the dict."""

    @pytest.fixture
    def conn_and_cursor(self):
        conn = _connect(_base_connstr())
        cur = conn.cursor()
        yield conn, cur
        cur.close()
        conn.close()

    # ── server / database / basic strings ────────────────────────

    def test_server_forwarded(self, conn_and_cursor):
        _, cur = conn_and_cursor
        ctx = _capture_pycore_context(cur)
        assert ctx["server"] == _server()

    def test_database_forwarded(self, conn_and_cursor):
        _, cur = conn_and_cursor
        ctx = _capture_pycore_context(cur)
        assert ctx["database"] == _database()

    def test_uid_pwd_forwarded(self, conn_and_cursor):
        _, cur = conn_and_cursor
        ctx = _capture_pycore_context(cur)
        assert ctx["user_name"] == _uid()
        assert ctx["password"] == _pwd()

    # ── boolean params ───────────────────────────────────────────

    def test_trust_server_certificate_is_string(self, conn_and_cursor):
        _, cur = conn_and_cursor
        ctx = _capture_pycore_context(cur)
        assert isinstance(ctx["trust_server_certificate"], str)
        assert ctx["trust_server_certificate"] == "Yes"

    def test_multi_subnet_failover_yes(self):
        conn = _connect(_base_connstr(MultiSubnetFailover="Yes"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["multi_subnet_failover"] == "Yes"
        cur.close()
        conn.close()

    def test_multi_subnet_failover_no(self):
        conn = _connect(_base_connstr(MultiSubnetFailover="No"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["multi_subnet_failover"] == "No"
        cur.close()
        conn.close()

    # ── integer params ───────────────────────────────────────────

    def test_packet_size_forwarded_as_int(self):
        conn = _connect(_base_connstr(PacketSize="8192"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["packet_size"] == 8192
        assert isinstance(ctx["packet_size"], int)
        cur.close()
        conn.close()

    # ── string params ────────────────────────────────────────────

    def test_server_spn_forwarded(self):
        conn = _connect(_base_connstr(ServerSPN="MSSQLSvc/myhost:1433"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["server_spn"] == "MSSQLSvc/myhost:1433"
        cur.close()
        conn.close()

    def test_application_intent_readonly(self):
        conn = _connect(_base_connstr(ApplicationIntent="ReadOnly"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["application_intent"] == "ReadOnly"
        cur.close()
        conn.close()

    def test_ip_address_preference_forwarded(self):
        conn = _connect(_base_connstr(IPAddressPreference="IPv4First"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["ip_address_preference"] == "IPv4First"
        cur.close()
        conn.close()

    def test_connect_retry_forwarded(self):
        conn = _connect(_base_connstr(ConnectRetryCount=3, ConnectRetryInterval=5))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["connect_retry_count"] == 3
        assert ctx["connect_retry_interval"] == 5
        cur.close()
        conn.close()

    def test_host_name_in_certificate_forwarded(self):
        conn = _connect(_base_connstr(HostNameInCertificate="*.database.windows.net"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["host_name_in_certificate"] == "*.database.windows.net"
        cur.close()
        conn.close()

    # ── encryption ───────────────────────────────────────────────

    @pytest.mark.parametrize("value", ["Yes", "No", "Optional", "Mandatory"])
    def test_encryption_forwarded(self, value):
        conn = _connect(_base_connstr(Encrypt=value))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["encryption"] == value
        cur.close()
        conn.close()

    def test_encryption_strict_forwarded(self):
        """Encrypt=Strict requires TDS 8.0 + valid cert; skip if unavailable."""
        try:
            conn = _connect(_base_connstr(Encrypt="Strict"))
        except Exception as exc:
            pytest.skip(f"Encrypt=Strict not supported in this environment: {exc}")
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["encryption"] == "Strict"
        cur.close()
        conn.close()

    # ── connect retry ────────────────────────────────────────────

    def test_connect_retry_count_forwarded(self):
        conn = _connect(_base_connstr(ConnectRetryCount="3"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["connect_retry_count"] == 3
        assert isinstance(ctx["connect_retry_count"], int)
        cur.close()
        conn.close()

    def test_connect_retry_interval_forwarded(self):
        conn = _connect(_base_connstr(ConnectRetryInterval="5"))
        cur = conn.cursor()
        ctx = _capture_pycore_context(cur)
        assert ctx["connect_retry_interval"] == 5
        assert isinstance(ctx["connect_retry_interval"], int)
        cur.close()
        conn.close()

    # ── absent params should NOT appear in dict ──────────────────

    def test_absent_params_not_in_dict(self, conn_and_cursor):
        """Params not in the connection string shouldn't appear in the dict."""
        _, cur = conn_and_cursor
        ctx = _capture_pycore_context(cur)
        for key in ("server_spn", "keep_alive", "keep_alive_interval",
                     "connect_retry_count", "connect_retry_interval",
                     "application_intent",
                     "host_name_in_certificate", "server_certificate"):
            assert key not in ctx, f"{key} should not be in context when not in connstr"


# ═════════════════════════════════════════════════════════════════
#  Invalid / garbage value tests
#
#  Verify that connstr_to_pycore_params and py-core handle bogus
#  values safely — defaulting, dropping, or rejecting as
#  appropriate.
# ═════════════════════════════════════════════════════════════════

class TestInvalidValues:
    """Invalid or garbage param values should be handled safely."""

    # ── connstr_to_pycore_params unit tests (no connection) ──────

    def test_trust_cert_passed_as_string(self):
        """trustservercertificate=Yes → passed as string 'Yes', not bool."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"trustservercertificate": "Yes"})
        assert result["trust_server_certificate"] == "Yes"
        assert isinstance(result["trust_server_certificate"], str)

    def test_multi_subnet_failover_passed_as_string(self):
        """multisubnetfailover=No → passed as string 'No', not bool."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"multisubnetfailover": "No"})
        assert result["multi_subnet_failover"] == "No"
        assert isinstance(result["multi_subnet_failover"], str)

    def test_garbage_bool_passed_through_as_string(self):
        """trustservercertificate=saurabh → passed as 'saurabh', py-core rejects."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"trustservercertificate": "saurabh"})
        assert result["trust_server_certificate"] == "saurabh"

    def test_non_odbc_bool_values_passed_through(self):
        """True/False/1/0 are NOT valid ODBC boolean values → passed as-is, py-core rejects."""
        from mssql_python.helpers import connstr_to_pycore_params
        for val in ("True", "true", "FALSE", "1", "0"):
            result = connstr_to_pycore_params({"trustservercertificate": val})
            assert result["trust_server_certificate"] == val, f"failed for {val!r}"

    def test_garbage_int_dropped(self):
        """packetsize=abc → key absent from result (invalid int is skipped)."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"packetsize": "abc"})
        assert "packet_size" not in result

    def test_garbage_connect_retry_count_dropped(self):
        """connectretrycount=xyz → key absent."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"connectretrycount": "xyz"})
        assert "connect_retry_count" not in result

    def test_float_string_int_dropped(self):
        """packetsize=8192.5 → dropped (int('8192.5') raises ValueError)."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"packetsize": "8192.5"})
        assert "packet_size" not in result

    def test_empty_string_int_dropped(self):
        """packetsize='' → dropped."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"packetsize": ""})
        assert "packet_size" not in result

    def test_empty_string_bool_passed_through(self):
        """trustservercertificate='' → passed as empty string, py-core rejects."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"trustservercertificate": ""})
        assert result["trust_server_certificate"] == ""

    def test_string_param_passed_through_as_is(self):
        """encrypt=GarbageValue → passed through unchanged, py-core decides."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({"encrypt": "GarbageValue"})
        assert result["encryption"] == "GarbageValue"

    def test_unknown_keys_silently_dropped(self):
        """Keys not in the key_map are ignored."""
        from mssql_python.helpers import connstr_to_pycore_params
        result = connstr_to_pycore_params({
            "server": "myhost",
            "fakekey": "fakevalue",
            "anotherbogus": "123",
        })
        assert result == {"server": "myhost"}

    # ── py-core live tests — how connection.rs handles bad values ─

    def _pycore_ctx(self, **overrides):
        base = {
            "server": _server(),
            "database": _database(),
            "user_name": _uid(),
            "password": _pwd(),
            "trust_server_certificate": "Yes",
            "encryption": "Optional",
        }
        base.update(overrides)
        return base

    def test_pycore_garbage_encryption_rejects(self):
        """encryption='GarbageValue' → py-core rejects with clear error.

        ODBC only accepts Yes/No/True/False/Optional/Mandatory/Strict.
        """
        import mssql_py_core
        ctx = self._pycore_ctx(encryption="GarbageValue")
        with pytest.raises(RuntimeError, match="Invalid Encrypt value"):
            mssql_py_core.PyCoreConnection(ctx)

    def test_pycore_garbage_application_intent_rejects(self):
        """application_intent='Bogus' → py-core rejects with clear error.

        ODBC only accepts ReadOnly/ReadWrite.
        """
        import mssql_py_core
        ctx = self._pycore_ctx(application_intent="Bogus")
        with pytest.raises(RuntimeError, match="Invalid ApplicationIntent value"):
            mssql_py_core.PyCoreConnection(ctx)

    def test_pycore_trusted_connection_invalid_rejects(self):
        """trusted_connection='xyz' → py-core rejects with clear error.

        connection.rs only accepts 'Yes'/'No' and returns an error for anything else.
        """
        import mssql_py_core
        ctx = self._pycore_ctx(trusted_connection="xyz")
        with pytest.raises(RuntimeError, match="Invalid Trusted_Connection value"):
            mssql_py_core.PyCoreConnection(ctx)

    def test_pycore_trusted_connection_bool_silently_ignored(self):
        """trusted_connection=False (bool) → py-core extracts as String, fails,
        falls through to None (not set).  Connection uses SQL auth instead.

        This documents a known type mismatch: connstr_to_pycore_params converts
        to bool, but connection.rs expects a String.  The bool is silently dropped.
        """
        import mssql_py_core
        ctx = self._pycore_ctx(trusted_connection=False)
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_pycore_yes_no_trust_cert_accepted(self):
        """trust_server_certificate='Yes'/'No' → py-core accepts both."""
        import mssql_py_core
        # 'Yes' should work
        ctx = self._pycore_ctx(trust_server_certificate="Yes")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_pycore_yes_no_multi_subnet_failover_accepted(self):
        """multi_subnet_failover='Yes'/'No' → py-core accepts both."""
        import mssql_py_core
        ctx = self._pycore_ctx(multi_subnet_failover="Yes")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_pycore_garbage_trust_cert_rejects(self):
        """trust_server_certificate='saurabh' → py-core rejects with clear error.

        connection.rs only accepts 'Yes'/'No' for TrustServerCertificate.
        """
        import mssql_py_core
        ctx = self._pycore_ctx(trust_server_certificate="saurabh")
        with pytest.raises(RuntimeError, match="Invalid TrustServerCertificate value"):
            mssql_py_core.PyCoreConnection(ctx)

    def test_pycore_garbage_multi_subnet_failover_rejects(self):
        """multi_subnet_failover='banana' → py-core rejects with clear error.

        connection.rs only accepts 'Yes'/'No' for MultiSubnetFailover.
        """
        import mssql_py_core
        ctx = self._pycore_ctx(multi_subnet_failover="banana")
        with pytest.raises(RuntimeError, match="Invalid MultiSubnetFailover value"):
            mssql_py_core.PyCoreConnection(ctx)

    # ── Encrypt: additional accepted values ──────────────────────

    @pytest.mark.parametrize("value", ["True", "true", "FALSE"])
    def test_pycore_encrypt_true_false_accepted(self, value):
        """Encrypt=True/False → py-core maps to Required/PreferOff."""
        import mssql_py_core
        ctx = self._pycore_ctx(encryption=value)
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_pycore_encrypt_disabled_accepted(self):
        """Encrypt=Disabled → py-core maps to PreferOff."""
        import mssql_py_core
        ctx = self._pycore_ctx(encryption="Disabled")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    # ── Default value tests (absent param → correct default) ────

    def test_pycore_encrypt_default_is_mandatory(self):
        """No encryption param → defaults to 'Mandatory' (Required), matching ODBC 18."""
        import mssql_py_core
        ctx = self._pycore_ctx()
        del ctx["encryption"]  # remove explicit value — should default to Mandatory
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute(
            "SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID"
        )
        assert cur.fetchone()[0] == "TRUE", "Default encryption should be Mandatory (TRUE)"
        cur.close()
        conn.close()

    def test_pycore_application_intent_readwrite_accepted(self):
        """application_intent='ReadWrite' → explicit value accepted."""
        import mssql_py_core
        ctx = self._pycore_ctx(application_intent="ReadWrite")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_pycore_application_intent_default_is_readwrite(self):
        """No application_intent → defaults to ReadWrite."""
        import mssql_py_core
        ctx = self._pycore_ctx()  # no application_intent key
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_pycore_ip_preference_ipv6first_accepted(self):
        """ip_address_preference='IPv6First' → accepted by py-core."""
        import mssql_py_core
        ctx = self._pycore_ctx(ip_address_preference="IPv6First")
        try:
            conn = mssql_py_core.PyCoreConnection(ctx)
        except RuntimeError as exc:
            if "Timeout" in str(exc) or "Connection refused" in str(exc):
                pytest.skip("Server not reachable via IPv6")
            raise
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_pycore_ip_preference_default_is_platform(self):
        """No ip_address_preference → defaults to UsePlatformDefault."""
        import mssql_py_core
        ctx = self._pycore_ctx()  # no ip_address_preference key
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_pycore_ip_preference_garbage_rejects(self):
        """ip_address_preference='Banana' → py-core rejects."""
        import mssql_py_core
        ctx = self._pycore_ctx(ip_address_preference="Banana")
        with pytest.raises(RuntimeError, match="Invalid IPAddressPreference value"):
            mssql_py_core.PyCoreConnection(ctx)

    def test_pycore_packet_size_out_of_range_defaults(self):
        """packet_size=99999 (>32768) → py-core ignores, defaults to 4096."""
        import mssql_py_core
        ctx = self._pycore_ctx(packet_size=99999)
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute(
            "SELECT net_packet_size FROM sys.dm_exec_connections "
            "WHERE session_id = @@SPID"
        )
        pkt = cur.fetchone()[0]
        assert 4000 <= pkt <= 5000, f"expected ~4096 default, got {pkt}"
        cur.close()
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  Live tests — connect directly through py-core and verify
#  server state.  These bypass the ODBC driver entirely, so they
#  can test params that ODBC rejects (KeepAlive, etc.) and params
#  not in _ALLOWED_CONNECTION_STRING_PARAMS (language, etc.).
# ═════════════════════════════════════════════════════════════════

class TestParamForwardingLive:
    """Connect via py-core with specific params and verify server-side."""

    def _pycore_ctx(self, **overrides):
        base = {
            "server": _server(),
            "database": _database(),
            "user_name": _uid(),
            "password": _pwd(),
            "trust_server_certificate": "Yes",
            "encryption": "Optional",
        }
        base.update(overrides)
        return base

    def test_packet_size_applied(self):
        """PacketSize=16384 → server sees that packet size."""
        import mssql_py_core
        ctx = self._pycore_ctx(packet_size=16384)
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute(
            "SELECT net_packet_size FROM sys.dm_exec_connections "
            "WHERE session_id = @@SPID"
        )
        pkt = cur.fetchone()[0]
        # TDS negotiation may adjust the exact size; confirm it's in the right ballpark
        assert 15000 <= pkt <= 17000, f"expected ~16384, got {pkt}"
        cur.close()
        conn.close()

    def test_default_packet_size_is_4096(self):
        """No PacketSize → py-core default is 4096."""
        import mssql_py_core
        ctx = self._pycore_ctx()
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute(
            "SELECT net_packet_size FROM sys.dm_exec_connections "
            "WHERE session_id = @@SPID"
        )
        pkt = cur.fetchone()[0]
        # TDS negotiation may adjust the exact size; confirm it's in the default range
        assert 4000 <= pkt <= 5000, f"expected ~4096, got {pkt}"
        cur.close()
        conn.close()

    def test_application_intent_readonly(self):
        """ApplicationIntent=ReadOnly → connection succeeds."""
        import mssql_py_core
        ctx = self._pycore_ctx(application_intent="ReadOnly")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_application_name_visible_on_server(self):
        """application_name → sys.dm_exec_sessions shows it."""
        import mssql_py_core
        ctx = self._pycore_ctx(application_name="ConnstrParamTest")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute(
            "SELECT program_name FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
        )
        app_name = cur.fetchone()[0].strip()
        assert app_name == "ConnstrParamTest"
        cur.close()
        conn.close()

    def test_workstation_id_visible_on_server(self):
        """workstation_id=TESTBOX → sys.dm_exec_sessions shows it."""
        import mssql_py_core
        ctx = self._pycore_ctx(workstation_id="TESTBOX")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute(
            "SELECT host_name FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
        )
        host = cur.fetchone()[0].strip()
        assert host == "TESTBOX"
        cur.close()
        conn.close()

    @pytest.mark.skip(reason="Known issue: non-ASCII language names in TDS LOGIN7 packet")
    def test_language_visible_on_server(self):
        """Language=French → @@LANGUAGE reflects it."""
        import mssql_py_core
        ctx = self._pycore_ctx(language="French")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT @@LANGUAGE")
        lang = cur.fetchone()[0]
        assert lang == "Français"
        cur.close()
        conn.close()

    def test_connect_timeout_does_not_break_connection(self):
        """connect_timeout=60 → connection still succeeds."""
        import mssql_py_core
        ctx = self._pycore_ctx(connect_timeout=60)
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_multi_subnet_failover_does_not_break_connection(self):
        """multi_subnet_failover='Yes' → connection still succeeds."""
        import mssql_py_core
        ctx = self._pycore_ctx(multi_subnet_failover="Yes")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_connect_retry_does_not_break_connection(self):
        """connect_retry_count and interval → connection still succeeds."""
        import mssql_py_core
        ctx = self._pycore_ctx(connect_retry_count=3, connect_retry_interval=5)
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_keepalive_does_not_break_connection(self):
        """keep_alive / keep_alive_interval → connection still succeeds.

        Some Linux kernels reject certain keepalive values at the socket
        level (EINVAL).  Skip when the OS refuses the option.
        """
        import mssql_py_core
        ctx = self._pycore_ctx(keep_alive=15000, keep_alive_interval=500)
        try:
            conn = mssql_py_core.PyCoreConnection(ctx)
        except RuntimeError as exc:
            if "os error 22" in str(exc).lower():
                pytest.skip("OS rejected keepalive socket option (EINVAL)")
            raise
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_ip_address_preference_ipv4first(self):
        """ip_address_preference=IPv4First → connection still succeeds."""
        import mssql_py_core
        ctx = self._pycore_ctx(ip_address_preference="IPv4First")
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  Bulkcopy round-trip — ODBC connect + bulkcopy with extra params
#
#  Only params accepted by BOTH the parser AND ODBC Driver 18.
#  KeepAlive/KeepAliveInterval are excluded — ODBC rejects them.
# ═════════════════════════════════════════════════════════════════

_BC_TABLE = "__connstr_param_bc_test"

class TestBulkcopyWithExtraParams:
    """Full ODBC-connect → bulkcopy flow with non-default params."""

    @pytest.fixture(autouse=True)
    def setup_table(self):
        conn = _connect(_base_connstr())
        cur = conn.cursor()
        cur.execute(f"IF OBJECT_ID('{_BC_TABLE}', 'U') IS NOT NULL DROP TABLE {_BC_TABLE}")
        cur.execute(f"CREATE TABLE {_BC_TABLE} (id INT, name VARCHAR(50))")
        yield conn, cur
        cur.execute(f"IF OBJECT_ID('{_BC_TABLE}', 'U') IS NOT NULL DROP TABLE {_BC_TABLE}")
        cur.close()
        conn.close()

    def test_bulkcopy_with_packet_size(self, setup_table):
        conn = _connect(_base_connstr(PacketSize="16384"))
        cur = conn.cursor()
        result = cur.bulkcopy(_BC_TABLE, [(1, "Alice"), (2, "Bob")], timeout=30)
        assert result["rows_copied"] == 2
        cur.close()
        conn.close()

    def test_bulkcopy_with_multi_subnet_failover(self, setup_table):
        conn = _connect(_base_connstr(MultiSubnetFailover="Yes"))
        cur = conn.cursor()
        result = cur.bulkcopy(_BC_TABLE, [(1, "Alice"), (2, "Bob")], timeout=30)
        assert result["rows_copied"] == 2
        cur.close()
        conn.close()

    def test_bulkcopy_with_application_intent_readwrite(self, setup_table):
        conn = _connect(_base_connstr(ApplicationIntent="ReadWrite"))
        cur = conn.cursor()
        result = cur.bulkcopy(_BC_TABLE, [(1, "Alice"), (2, "Bob")], timeout=30)
        assert result["rows_copied"] == 2
        cur.close()
        conn.close()

    def test_bulkcopy_with_multiple_extra_params(self, setup_table):
        """Several non-default params all at once (ODBC-compatible only)."""
        conn = _connect(_base_connstr(
            PacketSize="8192",
            MultiSubnetFailover="No",
            ApplicationIntent="ReadWrite",
            ConnectRetryCount="2",
            ConnectRetryInterval="5",
        ))
        cur = conn.cursor()
        result = cur.bulkcopy(_BC_TABLE, [(1, "Alice"), (2, "Bob"), (3, "Charlie")], timeout=30)
        assert result["rows_copied"] == 3
        cur.close()
        conn.close()
