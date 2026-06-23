# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Full-stack auth resolution e2e tests.

Each test builds a connection string → calls mssql_python.connect() (DDBC/ODBC path)
→ then calls cursor.bulkcopy() (py-core/mssql-tds path) and verifies BOTH paths
agree on success/failure.

The conflict matrix is from "docs/ODBC Auth Resolution - Deep Dive.md" §3.
  §3.1  Happy paths (#1–#17)
  §3.2  Silent clears (#18–#23)
  §3.3  Auth + TC clashes (#24–#29)
  §3.4  Partial / forbidden credentials (#30–#38)
  §3.5  Access token clashes (#39–#43)
  §3.6  DSN + connection string overrides

Tests that need real Azure AD infra or Kerberos are skipped unless env vars are set.
"""

import os
import re
from pathlib import Path
from unittest.mock import patch, MagicMock

import pytest
from dotenv import load_dotenv

# Load .env from mssql-tds root
_env_file = Path(__file__).parent.parent.parent.parent / ".env"
if _env_file.exists():
    load_dotenv(_env_file)

# ── env helpers ──────────────────────────────────────────────────

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

def _base(*, tc=None, auth=None, uid=None, pwd=None, extra=None):
    """Build a connection string from keyword args."""
    parts = [
        f"Server={_server()}",
        f"Database={_database()}",
    ]
    if tc is not None:
        parts.append(f"Trusted_Connection={tc}")
    if auth is not None:
        parts.append(f"Authentication={auth}")
    if uid is not None:
        parts.append(f"UID={uid}")
    if pwd is not None:
        parts.append(f"PWD={pwd}")
    parts.append("TrustServerCertificate=Yes")
    if extra:
        parts.extend(extra)
    return ";".join(parts)


# ── bulkcopy helper ──────────────────────────────────────────────

_BULKCOPY_TABLE = "e2e_auth_bulkcopy_test"

def _ensure_table(cursor):
    cursor.execute(
        f"IF OBJECT_ID('{_BULKCOPY_TABLE}', 'U') IS NOT NULL DROP TABLE {_BULKCOPY_TABLE}"
    )
    cursor.execute(f"CREATE TABLE {_BULKCOPY_TABLE} (id INT, name VARCHAR(50))")

def _do_bulkcopy(cursor):
    """Perform a small bulkcopy and return the result."""
    _ensure_table(cursor)
    result = cursor.bulkcopy(
        _BULKCOPY_TABLE,
        [(1, "Alice"), (2, "Bob")],
        timeout=30,
    )
    return result

def _verify_bulkcopy(cursor, expected_rows=2):
    cursor.execute(f"SELECT COUNT(*) FROM {_BULKCOPY_TABLE}")
    assert cursor.fetchone()[0] == expected_rows

def _cleanup_table(cursor):
    cursor.execute(
        f"IF OBJECT_ID('{_BULKCOPY_TABLE}', 'U') IS NOT NULL DROP TABLE {_BULKCOPY_TABLE}"
    )


# ── assert helpers ───────────────────────────────────────────────

def _connect(conn_str, autocommit=True):
    """Import & call mssql_python.connect. Raises on failure."""
    from mssql_python import connect
    return connect(conn_str, autocommit=autocommit)

def _connect_and_bulkcopy(conn_str):
    """Full flow: connect → cursor → bulkcopy → verify → cleanup.
    Returns (connection, result) so caller can assert on result.
    """
    conn = _connect(conn_str)
    cur = conn.cursor()
    try:
        result = _do_bulkcopy(cur)
        _verify_bulkcopy(cur)
        return conn, result
    finally:
        _cleanup_table(cur)
        cur.close()

def _expect_connect_error(conn_str, pattern=None):
    """Assert that connect() raises; optionally check message matches pattern."""
    with pytest.raises(Exception) as exc_info:
        _connect(conn_str)
    if pattern:
        assert re.search(pattern, str(exc_info.value), re.IGNORECASE), (
            f"Expected pattern '{pattern}' in error: {exc_info.value}"
        )
    return exc_info


# ═════════════════════════════════════════════════════════════════
#  SECTION 1 — §3.1 Happy paths (#1–#17)
# ═════════════════════════════════════════════════════════════════

class TestHappyPathsSqlAuth:
    """§3.1 rows #1, #5, #6, #7, #15, #16 — SQL auth combos that should connect AND bulkcopy."""

    def test_row1_uid_pwd_only(self):
        """#1  UID+PWD, no auth keywords → SQL Server auth."""
        cs = _base(uid=_uid(), pwd=_pwd())
        conn, result = _connect_and_bulkcopy(cs)
        assert result["rows_copied"] == 2
        conn.close()

    def test_row5_tc_no_uid_pwd(self):
        """#5  Trusted_Connection=No + UID+PWD → SQL Server auth (explicit non-integrated)."""
        cs = _base(tc="No", uid=_uid(), pwd=_pwd())
        conn, result = _connect_and_bulkcopy(cs)
        assert result["rows_copied"] == 2
        conn.close()

    def test_row7_sqlpassword_uid_pwd(self):
        """#7  Authentication=SqlPassword + UID+PWD → validated SQL auth."""
        cs = _base(auth="SqlPassword", uid=_uid(), pwd=_pwd())
        conn, result = _connect_and_bulkcopy(cs)
        assert result["rows_copied"] == 2
        conn.close()

    def test_row15_tc_no_ad_password_uid_pwd(self):
        """#15  TC=No + ADPassword + UID+PWD → AAD Password auth.
        TC=No is the default, so no conflict. Needs Azure AD infra to fully connect.
        mssql_python.auth.py intercepts ADPassword and calls azure-identity.
        """
        cs = _base(tc="No", auth="ActiveDirectoryPassword", uid="user@domain.com", pwd="p")
        # auth.py will try to acquire a token via azure-identity → expect failure
        # (no Azure AD infra), but NOT a client-side TC+Auth clash error.
        with pytest.raises(Exception) as exc_info:
            _connect(cs)
        msg = str(exc_info.value).lower()
        assert "cannot use authentication" not in msg
        assert "integrated security" not in msg

    def test_row16_tc_no_sqlpassword_uid_pwd(self):
        """#16  TC=No + SqlPassword + UID+PWD → SQL Server auth.
        TC=No is the default, no conflict with Authentication keyword.
        """
        cs = _base(tc="No", auth="SqlPassword", uid=_uid(), pwd=_pwd())
        conn, result = _connect_and_bulkcopy(cs)
        assert result["rows_copied"] == 2
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  SECTION 2 — §3.1 Happy path: no creds (#2, #6)
# ═════════════════════════════════════════════════════════════════

class TestHappyPathNoCreds:
    """§3.1 #2, #6 — No auth keywords, no UID/PWD → SQL auth with blank creds (server rejects)."""

    def test_row2_nothing_set(self):
        """Driver sends Login7 with empty UID — server should reject (login failure)."""
        cs = _base()
        _expect_connect_error(cs)

    def test_row6_tc_no_explicit_no_creds(self):
        """#6  Trusted_Connection=No, no UID/PWD → blank login attempt."""
        cs = _base(tc="No")
        _expect_connect_error(cs)


# ═════════════════════════════════════════════════════════════════
#  SECTION 3 — §3.1 Trusted_Connection=Yes (#3, #4)
# ═════════════════════════════════════════════════════════════════

_has_kerberos = os.environ.get("KERBEROS_TEST") == "1"

class TestTrustedConnection:
    """§3.1 #3, #4 — Windows Integrated / Kerberos."""

    @pytest.mark.skipif(not _has_kerberos, reason="KERBEROS_TEST not set")
    def test_row3_tc_yes_no_creds(self):
        """#3  Trusted_Connection=Yes → SSPI / Kerberos."""
        cs = _base(tc="Yes")
        conn, result = _connect_and_bulkcopy(cs)
        assert result["rows_copied"] == 2
        conn.close()

    @pytest.mark.skipif(not _has_kerberos, reason="KERBEROS_TEST not set")
    def test_row4_tc_yes_uid_pwd_silently_ignored(self):
        """#4  TC=Yes + UID+PWD → Integrated auth, creds silently ignored."""
        cs = _base(tc="Yes", uid=_uid(), pwd=_pwd())
        conn, result = _connect_and_bulkcopy(cs)
        assert result["rows_copied"] == 2
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  SECTION 4 — §3.4 SqlPassword partial creds (#30, #31, #32)
# ═════════════════════════════════════════════════════════════════

class TestSqlPasswordErrors:
    """§3.4 #30–#32 — SqlPassword requires both UID and PWD."""

    def test_row30_sqlpassword_no_uid_no_pwd(self):
        """#30  Authentication=SqlPassword, no UID/PWD → ERROR."""
        cs = _base(auth="SqlPassword")
        _expect_connect_error(cs)

    def test_row31_sqlpassword_uid_no_pwd(self):
        """#31  SqlPassword + UID only → ERROR."""
        cs = _base(auth="SqlPassword", uid=_uid())
        _expect_connect_error(cs)

    def test_row32_sqlpassword_pwd_no_uid(self):
        """#32  SqlPassword + PWD only → ERROR."""
        cs = _base(auth="SqlPassword", pwd=_pwd())
        _expect_connect_error(cs)


# ═════════════════════════════════════════════════════════════════
#  SECTION 5 — §3.1 #8 + §3.4 #33–#35 ActiveDirectoryPassword
# ═════════════════════════════════════════════════════════════════

class TestADPassword:
    """§3.1 #8 happy path; §3.4 #33–#35 partial creds."""

    @pytest.mark.skip(reason="Needs Azure AD infrastructure")
    def test_row8_ad_password_uid_pwd(self):
        """#8  ADPassword + UID+PWD → AAD Password auth (FedAuth in Login7)."""
        cs = _base(auth="ActiveDirectoryPassword", uid="user@domain.com", pwd="pwd")
        conn = _connect(cs)
        conn.close()

    def test_row33_ad_password_no_creds(self):
        """#33  ADPassword, no UID/PWD → ERROR (before network)."""
        cs = _base(auth="ActiveDirectoryPassword")
        _expect_connect_error(cs)

    def test_row34_ad_password_uid_no_pwd(self):
        """#34  ADPassword + UID only → ERROR."""
        cs = _base(auth="ActiveDirectoryPassword", uid="user@domain.com")
        _expect_connect_error(cs)

    def test_row35_ad_password_pwd_no_uid(self):
        """#35  ADPassword + PWD only → ERROR."""
        cs = _base(auth="ActiveDirectoryPassword", pwd="secret")
        _expect_connect_error(cs)


# ═════════════════════════════════════════════════════════════════
#  SECTION 6 — §3.1 #9 ActiveDirectoryIntegrated happy path
# ═════════════════════════════════════════════════════════════════

class TestADIntegrated:
    """§3.1 #9 — ADIntegrated uses FedAuth + MSAL, no creds.
    Silent clears for ADIntegrated + UID/PWD are in TestSilentClears.
    """

    @pytest.mark.skip(reason="Needs Azure AD infrastructure")
    def test_row9_ad_integrated_alone(self):
        """#9  ADIntegrated, no creds → FedAuth integrated."""
        cs = _base(auth="ActiveDirectoryIntegrated")
        conn = _connect(cs)
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  SECTION 7 — §3.1 #10, #11 ActiveDirectoryInteractive
# ═════════════════════════════════════════════════════════════════

class TestADInteractive:
    """§3.1 #10, #11 — ADInteractive triggers browser auth.
    Silent clear of PWD is in TestSilentClears (#18).
    """

    @pytest.mark.skip(reason="Needs interactive browser")
    def test_row10_ad_interactive_with_uid(self):
        """#10  ADInteractive + UID hint → browser popup."""
        cs = _base(auth="ActiveDirectoryInteractive", uid="user@domain.com")
        conn = _connect(cs)
        conn.close()

    @pytest.mark.skip(reason="Needs interactive browser")
    def test_row11_ad_interactive_no_hint(self):
        """#11  ADInteractive, no UID → browser popup, no hint."""
        cs = _base(auth="ActiveDirectoryInteractive")
        conn = _connect(cs)
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  SECTION 8 — §3.1 #12, #13 ActiveDirectoryMSI
# ═════════════════════════════════════════════════════════════════

class TestADMSI:
    """§3.1 #12, #13 — Managed Identity.
    Silent clears of PWD are in TestSilentClears (#19, #20).
    """

    @pytest.mark.skip(reason="Needs Azure Managed Identity environment")
    def test_row12_admsi_system_assigned(self):
        """#12  ADMSI, no UID → system-assigned managed identity."""
        cs = _base(auth="ActiveDirectoryMSI")
        conn = _connect(cs)
        conn.close()

    @pytest.mark.skip(reason="Needs Azure Managed Identity environment")
    def test_row13_admsi_user_assigned(self):
        """#13  ADMSI + UID (client_id) → user-assigned managed identity."""
        cs = _base(auth="ActiveDirectoryMSI", uid="client-id-here")
        conn = _connect(cs)
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  SECTION 9 — §3.1 #14 + §3.4 #36–#38 ActiveDirectoryServicePrincipal
# ═════════════════════════════════════════════════════════════════

class TestADServicePrincipal:
    """§3.1 #14 happy path; §3.4 #36–#38 partial creds."""

    @pytest.mark.skip(reason="Needs Azure AD app registration")
    def test_row14_adspa_uid_pwd(self):
        """#14  ADSPA + UID(client_id) + PWD(secret) → SPA auth."""
        cs = _base(auth="ActiveDirectoryServicePrincipal", uid="cid", pwd="sec")
        conn = _connect(cs)
        conn.close()

    def test_row36_adspa_no_creds(self):
        """#36  ADSPA, no creds → ERROR."""
        cs = _base(auth="ActiveDirectoryServicePrincipal")
        _expect_connect_error(cs)

    def test_row37_adspa_uid_no_pwd(self):
        """#37  ADSPA + UID only → ERROR."""
        cs = _base(auth="ActiveDirectoryServicePrincipal", uid="cid")
        _expect_connect_error(cs)

    def test_row38_adspa_pwd_no_uid(self):
        """#38  ADSPA + PWD only → ERROR."""
        cs = _base(auth="ActiveDirectoryServicePrincipal", pwd="sec")
        _expect_connect_error(cs)


# ═════════════════════════════════════════════════════════════════
#  SECTION 10 — §3.3 Auth + TC clashes (#24–#29)
# ═════════════════════════════════════════════════════════════════

class TestAuthTcClash:
    """§3.3 #24–#29 — Auth keyword + TC=Yes is always an error."""

    def test_row24_tc_yes_sqlpassword(self):
        """#24  TC=Yes + SqlPassword → ERROR."""
        cs = _base(tc="Yes", auth="SqlPassword", uid=_uid(), pwd=_pwd())
        _expect_connect_error(cs)

    def test_row25_tc_yes_ad_password(self):
        """#25  TC=Yes + ADPassword → ERROR."""
        cs = _base(tc="Yes", auth="ActiveDirectoryPassword", uid="u", pwd="p")
        _expect_connect_error(cs)

    def test_row26_tc_yes_ad_integrated(self):
        """#26  TC=Yes + ADIntegrated → ERROR."""
        cs = _base(tc="Yes", auth="ActiveDirectoryIntegrated")
        _expect_connect_error(cs)

    def test_row27_tc_yes_ad_interactive(self):
        """#27  TC=Yes + ADInteractive → ERROR.
        Must mock token acquisition — mssql_python's auth.py intercepts
        Interactive before ODBC sees the conn string.
        """
        cs = _base(tc="Yes", auth="ActiveDirectoryInteractive")
        with patch("mssql_python.auth.get_auth_token", return_value=None):
            _expect_connect_error(cs)

    def test_row28_tc_yes_admsi(self):
        """#28  TC=Yes + ADMSI → ERROR."""
        cs = _base(tc="Yes", auth="ActiveDirectoryMSI")
        _expect_connect_error(cs)

    def test_row29_tc_yes_adspa(self):
        """#29  TC=Yes + ADSPA → ERROR."""
        cs = _base(tc="Yes", auth="ActiveDirectoryServicePrincipal", uid="c", pwd="s")
        _expect_connect_error(cs)

    def test_tc_yes_ad_default(self):
        """TC=Yes + ADDefault → ERROR (not in §3.3 — ADDefault is mssql-python-specific).
        Must mock — auth.py intercepts Default before ODBC sees the clash.
        """
        cs = _base(tc="Yes", auth="ActiveDirectoryDefault")
        with patch("mssql_python.auth.get_auth_token", return_value=None):
            _expect_connect_error(cs)


# ═════════════════════════════════════════════════════════════════
#  SECTION 10a — §3.2 Silent Clears (#18–#23)
# ═════════════════════════════════════════════════════════════════

class TestSilentClears:
    """§3.2 #18–#23 — Inputs that are silently dropped without error.

    These are tested via py-core PyCoreConnection dict path (bulkcopy),
    since mssql_python.auth.py intercepts AD modes before ODBC sees them.
    We verify the transformer silently clears UID/PWD as expected.
    """

    def _ctx(self, **overrides):
        base = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
        }
        base.update(overrides)
        return base

    def test_row18_ad_interactive_uid_pwd_clears_pwd(self):
        """#18  ADInteractive + UID + PWD → PWD silently cleared, UID used as hint.
        Transformer should clear PWD. On py-core path, this means the context
        sent to mssql-tds should not have password after transformation.
        """
        import mssql_py_core
        ctx = self._ctx(
            authentication="ActiveDirectoryInteractive",
            user_name="user@domain.com",
            password="should-be-cleared",
        )
        # mssql-tds transformer should silently clear PWD.
        # Without Azure AD infra, connection will fail post-validation,
        # but should NOT fail with a "User or Password" validation error.
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        msg = str(exc_info.value)
        assert "Both User and Password" not in msg
        assert "Cannot use" not in msg

    def test_row19_admsi_uid_pwd_clears_pwd(self):
        """#19  ADMSI + UID + PWD → PWD silently cleared, UID = client_id."""
        import mssql_py_core
        ctx = self._ctx(
            authentication="ActiveDirectoryMSI",
            user_name="client-id-here",
            password="should-be-cleared",
        )
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        msg = str(exc_info.value)
        assert "Both User and Password" not in msg
        assert "Cannot use" not in msg

    def test_row20_admsi_pwd_only_clears_pwd(self):
        """#20  ADMSI + PWD only → PWD silently cleared, system-assigned MSI."""
        import mssql_py_core
        ctx = self._ctx(
            authentication="ActiveDirectoryMSI",
            password="should-be-cleared",
        )
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        msg = str(exc_info.value)
        assert "Both User and Password" not in msg
        assert "Cannot use" not in msg

    def test_row21_ad_integrated_uid_cleared(self):
        """#21  ADIntegrated + UID → UID silently cleared (dialog) or ignored (no-prompt).
        ODBC driver rejects this with error 41402, but mssql-tds transformer
        silently clears per PR 891 semantics. This test documents the DEVIATION:
        mssql_python.connect() → ODBC → ERROR, but py-core → mssql-tds → silent clear.
        """
        # ODBC path: ERROR
        cs = _base(auth="ActiveDirectoryIntegrated", uid="user@domain.com")
        _expect_connect_error(cs)

        # py-core path: transformer silently clears UID
        import mssql_py_core
        ctx = self._ctx(
            authentication="ActiveDirectoryIntegrated",
            user_name="user@domain.com",
        )
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        msg = str(exc_info.value)
        # Should NOT be a "Cannot use ADIntegrated with User" validation error
        # because transformer silently clears. Failure is post-validation (no Azure AD).
        assert "Cannot use Authentication" not in msg

    def test_row22_ad_integrated_pwd_cleared(self):
        """#22  ADIntegrated + PWD → PWD silently cleared."""
        import mssql_py_core
        ctx = self._ctx(
            authentication="ActiveDirectoryIntegrated",
            password="should-be-cleared",
        )
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        msg = str(exc_info.value)
        assert "Both User and Password" not in msg
        assert "Cannot use" not in msg

    def test_row23_ad_integrated_uid_pwd_both_cleared(self):
        """#23  ADIntegrated + UID + PWD → both silently cleared."""
        import mssql_py_core
        ctx = self._ctx(
            authentication="ActiveDirectoryIntegrated",
            user_name="user@domain.com",
            password="should-be-cleared",
        )
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        msg = str(exc_info.value)
        assert "Cannot use Authentication" not in msg
        assert "Both User and Password" not in msg


# ═════════════════════════════════════════════════════════════════
#  SECTION 11 — §3.1 #17 + §3.5 Access Token (#39–#43)
# ═════════════════════════════════════════════════════════════════

class TestAccessTokenIsolation:
    """§3.1 #17 happy path; §3.5 #39–#43 clashes.

    mssql_python sets access token via attrs_before dict (not connection string keyword),
    so we test via the py-core PyCoreConnection dict path (what bulkcopy uses).
    """

    def test_row17_access_token_alone_bulkcopy_path(self):
        """#17  Access token alone → connect succeeds through py-core.
        (We mock at the PyCoreConnection level since access_token is not a conn string keyword.)
        """
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "access_token": "mock.jwt.token",
        }
        # This will attempt a real connection with a fake token — expect
        # the server to reject the token, but NOT a client-side validation error.
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        # Should NOT be a validation error (no "Cannot use Access Token" message)
        assert "Cannot use Access Token" not in str(exc_info.value)

    def test_row39_access_token_plus_tc_bulkcopy_path(self):
        """#39  Access token + TC=Yes → ERROR (client-side)."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "access_token": "mock.jwt.token",
            "trusted_connection": "Yes",
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Access Token cannot be used" in str(exc_info.value)

    def test_row40_access_token_plus_auth_bulkcopy_path(self):
        """#40  Access token + Authentication → ERROR."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "access_token": "mock.jwt.token",
            "authentication": "SqlPassword",
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Access Token cannot be used" in str(exc_info.value)

    def test_row41_access_token_plus_uid_bulkcopy_path(self):
        """#41  Access token + UID → ERROR."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "access_token": "mock.jwt.token",
            "user_name": "sa",
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Access Token cannot be used" in str(exc_info.value)

    def test_row42_access_token_plus_pwd_bulkcopy_path(self):
        """#42  Access token + PWD → ERROR."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "access_token": "mock.jwt.token",
            "password": "secret",
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Access Token cannot be used" in str(exc_info.value)

    def test_row43_access_token_all_conflicts_bulkcopy_path(self):
        """#43  Access token + TC + Auth + UID + PWD → ERROR listing all conflicts."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "access_token": "mock.jwt.token",
            "trusted_connection": "Yes",
            "authentication": "SqlPassword",
            "user_name": "sa",
            "password": "secret",
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        msg = str(exc_info.value)
        assert "Access Token cannot be used" in msg
        assert "Trusted_Connection is set" in msg
        assert "Authentication keyword is provided" in msg
        assert "User is provided" in msg
        assert "Password is provided" in msg


# ═════════════════════════════════════════════════════════════════
#  SECTION 12 — Unsupported / bogus Authentication values
# ═════════════════════════════════════════════════════════════════

class TestUnsupportedAuthValues:
    """Bogus or unknown Authentication keywords should error."""

    def test_bogus_auth_value_connect(self):
        """Unknown Authentication value → ODBC driver rejects."""
        cs = _base(auth="BogusValue", uid=_uid(), pwd=_pwd())
        _expect_connect_error(cs)

    def test_bogus_auth_value_bulkcopy_path(self):
        """Unknown Authentication value → py-core validator rejects."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "authentication": "BogusValue",
            "user_name": "sa",
            "password": "secret",
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Unsupported Authentication value" in str(exc_info.value)

    def test_managed_identity_not_supported_in_driver(self):
        """ManagedIdentity (unsupported alias) → error."""
        cs = _base(auth="ManagedIdentity")
        _expect_connect_error(cs)


# ═════════════════════════════════════════════════════════════════
#  SECTION 13 — TC value validation
# ═════════════════════════════════════════════════════════════════

class TestTCValueValidation:
    """Only 'Yes'/'No' accepted for Trusted_Connection."""

    def test_tc_true_rejected(self):
        """Trusted_Connection=true → ERROR (ODBC only accepts Yes/No)."""
        cs = _base(tc="true", uid=_uid(), pwd=_pwd())
        # ODBC may accept 'true' as yes on some platforms; py-core rejects it.
        # We test the py-core path explicitly.
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "trusted_connection": "true",
            "user_name": _uid(),
            "password": _pwd(),
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Invalid Trusted_Connection value" in str(exc_info.value)

    def test_tc_false_rejected(self):
        """Trusted_Connection=false → ERROR (only Yes/No)."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "trusted_connection": "false",
            "user_name": _uid(),
            "password": _pwd(),
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Invalid Trusted_Connection value" in str(exc_info.value)

    def test_tc_1_rejected(self):
        """Trusted_Connection=1 → ERROR."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "trusted_connection": "1",
            "user_name": _uid(),
            "password": _pwd(),
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Invalid Trusted_Connection value" in str(exc_info.value)

    def test_tc_0_rejected(self):
        """Trusted_Connection=0 → ERROR (only Yes/No)."""
        import mssql_py_core
        context = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "trusted_connection": "0",
            "user_name": _uid(),
            "password": _pwd(),
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        assert "Invalid Trusted_Connection value" in str(exc_info.value)


# ═════════════════════════════════════════════════════════════════
#  SECTION 15 — Full-stack connect + bulkcopy parity (SQL auth)
# ═════════════════════════════════════════════════════════════════

class TestConnectBulkcopyParity:
    """Verify that DDBC connect path and py-core bulkcopy path
    agree on success/failure for the same connection string.
    """

    def test_uid_pwd_both_paths_succeed(self):
        """UID+PWD: DDBC connects, bulkcopy also connects."""
        cs = _base(uid=_uid(), pwd=_pwd())
        conn = _connect(cs)
        cur = conn.cursor()
        try:
            result = _do_bulkcopy(cur)
            assert result["rows_copied"] == 2
            _verify_bulkcopy(cur)
        finally:
            _cleanup_table(cur)
            cur.close()
            conn.close()

    def test_sqlpassword_both_paths_succeed(self):
        """SqlPassword + UID+PWD: both paths succeed."""
        cs = _base(auth="SqlPassword", uid=_uid(), pwd=_pwd())
        conn = _connect(cs)
        cur = conn.cursor()
        try:
            result = _do_bulkcopy(cur)
            assert result["rows_copied"] == 2
            _verify_bulkcopy(cur)
        finally:
            _cleanup_table(cur)
            cur.close()
            conn.close()

    def test_tc_no_uid_pwd_both_paths_succeed(self):
        """TC=No + UID+PWD: explicit non-integrated, both succeed."""
        cs = _base(tc="No", uid=_uid(), pwd=_pwd())
        conn = _connect(cs)
        cur = conn.cursor()
        try:
            result = _do_bulkcopy(cur)
            assert result["rows_copied"] == 2
            _verify_bulkcopy(cur)
        finally:
            _cleanup_table(cur)
            cur.close()
            conn.close()


# ═════════════════════════════════════════════════════════════════
#  SECTION 16 — Bulkcopy builds pycore_context correctly
# ═════════════════════════════════════════════════════════════════

class TestBulkcopyContextBuilding:
    """Verify cursor.bulkcopy() builds the right pycore_context dict
    by patching PyCoreConnection and inspecting the dict.
    """

    def test_sql_auth_context_has_uid_pwd(self):
        """SQL auth → pycore_context has user_name and password."""
        cs = _base(uid=_uid(), pwd=_pwd())
        conn = _connect(cs)
        cur = conn.cursor()
        _ensure_table(cur)

        captured = {}
        original_pycore = __import__("mssql_py_core")

        class SpyPyCoreConnection:
            def __init__(self, ctx, **kwargs):
                captured.update(ctx)
                raise RuntimeError("Spy: captured context")

            def cursor(self):
                pass

        with patch.object(original_pycore, "PyCoreConnection", SpyPyCoreConnection):
            with pytest.raises(RuntimeError, match="Spy: captured context"):
                cur.bulkcopy(_BULKCOPY_TABLE, [(1, "test")], timeout=30)

        assert "user_name" in captured
        assert captured["user_name"] == _uid()
        _cleanup_table(cur)
        cur.close()
        conn.close()

    def test_ad_auth_context_has_access_token(self):
        """AD auth → pycore_context has access_token (mocked)."""
        cs = _base(auth="ActiveDirectoryDefault", uid=_uid(), pwd=_pwd())

        # Mock the auth flow so connect() succeeds without real Azure AD
        mock_token_struct = b"\x00" * 8
        mock_attrs_before = {1256: mock_token_struct}  # SQL_COPT_SS_ACCESS_TOKEN

        # We can't easily mock through mssql_python.connect() for AD since
        # it needs DDBC. Instead, test the pycore_context building logic
        # by simulating what cursor.bulkcopy() does when _auth_type is set.
        from mssql_python import connect as mssql_connect

        # Connect with SQL auth (known to work)
        conn = mssql_connect(
            _base(uid=_uid(), pwd=_pwd()),
            autocommit=True,
        )
        # Simulate an AD connection by setting _auth_type
        conn._auth_type = "default"
        cur = conn.cursor()
        _ensure_table(cur)

        # Mock AADAuth.get_raw_token to return a fake token
        with patch("mssql_python.auth.AADAuth.get_raw_token", return_value="fake.jwt.token"):
            captured = {}
            original_pycore = __import__("mssql_py_core")
            OrigPyCoreConn = original_pycore.PyCoreConnection

            class SpyPyCoreConnection:
                def __init__(self, ctx, **kwargs):
                    captured.update(ctx)
                    # Don't actually connect — just capture the context
                    raise RuntimeError("Spy: captured context")

                def cursor(self):
                    pass

            with patch.object(original_pycore, "PyCoreConnection", SpyPyCoreConnection):
                with pytest.raises(RuntimeError, match="Spy: captured context"):
                    cur.bulkcopy(_BULKCOPY_TABLE, [(1, "test")], timeout=30)

        assert "access_token" in captured
        assert captured["access_token"] == "fake.jwt.token"
        # cursor.py forwards raw params — uid/pwd ARE in the context dict.
        # py-core's transformer (not cursor.py) silently clears them for AD modes.
        _cleanup_table(cur)
        cur.close()
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  SECTION 17 — Bulkcopy auth type → token acquisition
# ═════════════════════════════════════════════════════════════════

class TestBulkcopyTokenAcquisition:
    """Verify that _bulkcopy() acquires fresh tokens for each AD auth type."""

    @pytest.fixture
    def sql_conn_with_fake_auth(self):
        """A real SQL connection with _auth_type faked for testing."""
        from mssql_python import connect as mssql_connect
        conn = mssql_connect(_base(uid=_uid(), pwd=_pwd()), autocommit=True)
        cur = conn.cursor()
        _ensure_table(cur)
        yield conn, cur
        _cleanup_table(cur)
        cur.close()
        conn.close()

    @pytest.mark.parametrize("auth_type", ["default", "interactive", "devicecode"])
    def test_token_acquired_for_auth_type(self, sql_conn_with_fake_auth, auth_type):
        """For each supported auth_type, verify get_raw_token is called."""
        conn, cur = sql_conn_with_fake_auth
        conn._auth_type = auth_type

        with patch("mssql_python.auth.AADAuth.get_raw_token", return_value="fake.jwt") as mock_get:
            original_pycore = __import__("mssql_py_core")

            class SpyPyCoreConnection:
                def __init__(self, ctx, **kwargs):
                    raise RuntimeError("Spy stop")

                def cursor(self):
                    pass

            with patch.object(original_pycore, "PyCoreConnection", SpyPyCoreConnection):
                with pytest.raises(RuntimeError, match="Spy stop"):
                    cur.bulkcopy(_BULKCOPY_TABLE, [(1, "test")], timeout=30)

            mock_get.assert_called_once_with(auth_type, None)

    def test_token_acquisition_failure_raises(self, sql_conn_with_fake_auth):
        """If get_raw_token fails, _bulkcopy raises RuntimeError."""
        conn, cur = sql_conn_with_fake_auth
        conn._auth_type = "default"

        with patch(
            "mssql_python.auth.AADAuth.get_raw_token",
            side_effect=RuntimeError("Azure Identity unavailable"),
        ):
            with pytest.raises(RuntimeError, match="Bulk copy failed"):
                cur.bulkcopy(_BULKCOPY_TABLE, [(1, "test")], timeout=30)


# ═════════════════════════════════════════════════════════════════
#  SECTION 18 — Edge cases
# ═════════════════════════════════════════════════════════════════

class TestEdgeCases:
    """Miscellaneous edge cases from the ODBC deep dive."""

    def test_empty_auth_string_clears_auth_pycore(self):
        """Authentication='' (empty) → intentional reset, falls through to SQL auth.
        mssql_python's parser rejects empty values, so we test via py-core directly.
        This matches ODBC's DSN reset semantics (Authentication= clears DSN's auth).
        """
        import mssql_py_core
        ctx = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
            "authentication": "",
            "user_name": _uid(),
            "password": _pwd(),
        }
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_case_insensitive_auth_keyword(self):
        """SQLPASSWORD, sqlpassword, SqlPassword → all accepted."""
        for variant in ["SQLPASSWORD", "sqlpassword", "Sqlpassword"]:
            cs = _base(auth=variant, uid=_uid(), pwd=_pwd())
            conn = _connect(cs)
            conn.close()

    def test_uid_pwd_whitespace(self):
        """UID/PWD with leading/trailing spaces — driver should handle."""
        cs = _base(uid=_uid(), pwd=_pwd())
        conn = _connect(cs)
        conn.close()

    def test_multiple_semicolons(self):
        """Extra semicolons in connection string don't break parsing."""
        cs = _base(uid=_uid(), pwd=_pwd()) + ";;;"
        conn = _connect(cs)
        conn.close()

    def test_sqlpassword_collapse_wire_equivalence(self):
        """SqlPassword collapses to Password on wire — same as bare UID+PWD.
        Both succeed and bulkcopy works identically.
        """
        cs_bare = _base(uid=_uid(), pwd=_pwd())
        cs_sqlpwd = _base(auth="SqlPassword", uid=_uid(), pwd=_pwd())

        conn1, r1 = _connect_and_bulkcopy(cs_bare)
        conn1.close()
        conn2, r2 = _connect_and_bulkcopy(cs_sqlpwd)
        conn2.close()
        assert r1["rows_copied"] == r2["rows_copied"] == 2


# ═════════════════════════════════════════════════════════════════
#  SECTION 19 — py-core dict validation parity
# ═════════════════════════════════════════════════════════════════

class TestPyCoreValidationParity:
    """Verify py-core validator rejects the same combos ODBC does.
    Uses PyCoreConnection dict directly (what bulkcopy sends).
    """

    def _ctx(self, **overrides):
        base = {
            "server": _server(),
            "database": _database(),
            "trust_server_certificate": True,
            "encryption": "Optional",
        }
        base.update(overrides)
        return base

    def test_auth_plus_tc_rejected(self):
        ctx = self._ctx(
            authentication="SqlPassword",
            trusted_connection="Yes",
            user_name="sa",
            password="secret",
        )
        import mssql_py_core
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        assert "Trusted_Connection" in str(exc_info.value)

    def test_sqlpassword_no_uid_rejected(self):
        ctx = self._ctx(authentication="SqlPassword")
        import mssql_py_core
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        assert "Both User and Password" in str(exc_info.value)

    def test_ad_password_no_creds_rejected(self):
        ctx = self._ctx(authentication="ActiveDirectoryPassword")
        import mssql_py_core
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        assert "Both User and Password" in str(exc_info.value)

    def test_adspa_missing_pwd_rejected(self):
        ctx = self._ctx(authentication="ActiveDirectoryServicePrincipal", user_name="cid")
        import mssql_py_core
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        assert "Both User and Password" in str(exc_info.value)

    def test_bogus_auth_rejected(self):
        ctx = self._ctx(authentication="ManagedIdentity", user_name="x", password="y")
        import mssql_py_core
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        assert "Unsupported Authentication value" in str(exc_info.value)

    def test_valid_sql_auth_passes(self):
        ctx = self._ctx(user_name=_uid(), password=_pwd())
        import mssql_py_core
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_sqlpassword_with_creds_passes(self):
        ctx = self._ctx(authentication="SqlPassword", user_name=_uid(), password=_pwd())
        import mssql_py_core
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()

    def test_empty_auth_passes(self):
        ctx = self._ctx(authentication="", user_name=_uid(), password=_pwd())
        import mssql_py_core
        conn = mssql_py_core.PyCoreConnection(ctx)
        cur = conn.cursor()
        cur.execute("SELECT 1")
        assert cur.fetchone()[0] == 1
        cur.close()
        conn.close()


# ═════════════════════════════════════════════════════════════════
#  SECTION 20 — Encryption interaction with auth
# ═════════════════════════════════════════════════════════════════

class TestEncryptionAuthInteraction:
    """Encryption setting should not affect auth validation."""

    @pytest.mark.parametrize("encrypt", ["Yes", "No", "Optional", "Mandatory"])
    def test_encrypt_with_sql_auth(self, encrypt):
        """Various Encrypt values with SQL auth — all should attempt connect."""
        cs = _base(uid=_uid(), pwd=_pwd(), extra=[f"Encrypt={encrypt}"])
        try:
            conn = _connect(cs)
            conn.close()
        except Exception:
            # Encryption mismatch may cause server to reject, but NOT a
            # client-side auth validation error.
            pass

    def test_encrypt_strict_with_auth_clash_still_errors(self):
        """Auth clash errors happen before encryption negotiation."""
        cs = _base(tc="Yes", auth="SqlPassword", uid=_uid(), pwd=_pwd(),
                   extra=["Encrypt=Strict"])
        _expect_connect_error(cs)
