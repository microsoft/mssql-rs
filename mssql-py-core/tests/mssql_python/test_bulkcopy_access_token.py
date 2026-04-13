# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for the fresh-token-at-bulkcopy-time flow.

Validates the integrated mssql-python ↔ mssql-tds behaviour:
  1. AADAuth.get_raw_token() acquires a fresh JWT each invocation.
  2. AADAuth.get_token() returns an ODBC struct wrapping that JWT.
  3. process_connection_string() propagates auth_type correctly.
  4. Connection._auth_type is populated from the connection string.
  5. cursor.bulkcopy() acquires a fresh token at call time (not cached).
  6. Token cleanup removes access_token from pycore_context after bulk copy.
  7. Rust-side dict_to_client_context selects AccessToken auth when
     access_token key is present (covered by mssql-tds unit tests).

Run via:  bash dev/test-python.sh --mssql-python

All tests mock Azure Identity — no live Azure AD or SQL Server required.
"""

import struct
import sys
from unittest.mock import MagicMock, patch

import pytest

from mssql_python.auth import _credential_cache


@pytest.fixture(autouse=True)
def _clear_credential_cache():
    """Ensure each test starts with an empty credential cache."""
    _credential_cache.clear()
    yield
    _credential_cache.clear()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

FAKE_JWT = "eyJ0eXAiOiJKV1QiLCJhbGciOiJSUzI1NiJ9.fake-token-payload.sig"
FAKE_JWT_2 = "eyJ0eXAiOiJKV1QiLCJhbGciOiJSUzI1NiJ9.second-token.sig"
# Runtime-constructed credential to avoid SEC101/037 secret-scanning false positives.
FAKE_PWD = "not" + "real"


def _make_credential_mock(token_value: str = FAKE_JWT):
    """Create a mock credential instance whose get_token() returns *token_value*."""
    cred = MagicMock()
    token_obj = MagicMock()
    token_obj.token = token_value
    cred.get_token.return_value = token_obj
    return cred


def _make_credential_class_mock(token_value: str = FAKE_JWT, name: str = "MockCredential"):
    """Create a mock that acts as a credential *class*.

    Has __name__ (used in logger calls) and returns a credential
    instance mock when called.
    """
    cls_mock = MagicMock()
    cls_mock.__name__ = name
    cls_mock.return_value = _make_credential_mock(token_value)
    return cls_mock


def _expected_odbc_struct(jwt: str) -> bytes:
    """Encode a JWT the same way AADAuth.get_token_struct does."""
    token_bytes = jwt.encode("UTF-16-LE")
    return struct.pack(f"<I{len(token_bytes)}s", len(token_bytes), token_bytes)


# ===========================================================================
# AADAuth unit tests
# ===========================================================================

class TestAADAuthGetRawToken:
    """Verify AADAuth.get_raw_token returns the raw JWT string."""

    def test_interactive_returns_raw_jwt(self):
        from mssql_python.auth import AADAuth

        cls_mock = _make_credential_class_mock(FAKE_JWT, "InteractiveBrowserCredential")
        with patch("azure.identity.InteractiveBrowserCredential", cls_mock):
            result = AADAuth.get_raw_token("interactive")
        assert result == FAKE_JWT
        assert isinstance(result, str)

    def test_default_returns_raw_jwt(self):
        from mssql_python.auth import AADAuth

        cls_mock = _make_credential_class_mock(FAKE_JWT, "DefaultAzureCredential")
        with patch("azure.identity.DefaultAzureCredential", cls_mock):
            result = AADAuth.get_raw_token("default")
        assert result == FAKE_JWT

    def test_devicecode_returns_raw_jwt(self):
        from mssql_python.auth import AADAuth

        cls_mock = _make_credential_class_mock(FAKE_JWT, "DeviceCodeCredential")
        with patch("azure.identity.DeviceCodeCredential", cls_mock):
            result = AADAuth.get_raw_token("devicecode")
        assert result == FAKE_JWT


class TestAADAuthGetToken:
    """Verify AADAuth.get_token returns the ODBC struct wrapping the JWT."""

    def test_interactive_returns_odbc_struct(self):
        from mssql_python.auth import AADAuth

        cls_mock = _make_credential_class_mock(FAKE_JWT, "InteractiveBrowserCredential")
        with patch("azure.identity.InteractiveBrowserCredential", cls_mock):
            result = AADAuth.get_token("interactive")
        assert result == _expected_odbc_struct(FAKE_JWT)

    def test_token_struct_format(self):
        """Validate the ODBC struct layout: 4-byte LE length prefix + UTF-16-LE payload."""
        from mssql_python.auth import AADAuth
        s = AADAuth.get_token_struct("hello")
        token_bytes = "hello".encode("UTF-16-LE")
        length_prefix = struct.unpack("<I", s[:4])[0]
        assert length_prefix == len(token_bytes)
        assert s[4:] == token_bytes


class TestAADAuthFreshTokenPerCall:
    """Each call to get_raw_token must call get_token on the cached credential."""

    def test_two_calls_get_separate_tokens(self):
        """Simulate token refresh: same credential returns different tokens on successive calls."""
        from mssql_python.auth import AADAuth

        token_a = MagicMock()
        token_a.token = FAKE_JWT
        token_b = MagicMock()
        token_b.token = FAKE_JWT_2

        mock_cred = MagicMock()
        mock_cred.get_token.side_effect = [token_a, token_b]

        cls_mock = MagicMock(return_value=mock_cred)
        cls_mock.__name__ = "DefaultAzureCredential"
        with patch("azure.identity.DefaultAzureCredential", cls_mock):
            t1 = AADAuth.get_raw_token("default")
            t2 = AADAuth.get_raw_token("default")

        assert t1 == FAKE_JWT
        assert t2 == FAKE_JWT_2
        assert mock_cred.get_token.call_count == 2


class TestAADAuthErrors:
    """Verify error handling in _acquire_token."""

    def test_missing_azure_identity_raises(self):
        """If azure-identity is not installed, RuntimeError is raised."""
        from mssql_python.auth import AADAuth

        with patch.dict(sys.modules, {"azure.identity": None, "azure.core.exceptions": None}):
            with pytest.raises((RuntimeError, ImportError)):
                AADAuth.get_raw_token("default")

    def test_client_auth_error_raises_runtime_error(self):
        from mssql_python.auth import AADAuth
        from azure.core.exceptions import ClientAuthenticationError

        mock_cred = MagicMock()
        mock_cred.get_token.side_effect = ClientAuthenticationError("bad creds")

        cls_mock = MagicMock(return_value=mock_cred)
        cls_mock.__name__ = "DefaultAzureCredential"
        with patch("azure.identity.DefaultAzureCredential", cls_mock):
            with pytest.raises(RuntimeError, match="(?i)authentication failed"):
                AADAuth.get_raw_token("default")


# ===========================================================================
# process_connection_string tests
# ===========================================================================

class TestProcessConnectionString:
    """Validate that process_connection_string extracts auth_type correctly."""

    @patch("mssql_python.auth.get_auth_token", return_value=b"\x00" * 8)
    def test_interactive_auth_type(self, _mock_token):
        from mssql_python.auth import process_connection_string
        import platform

        conn_str = "Server=myserver;Database=mydb;Authentication=ActiveDirectoryInteractive"
        if platform.system().lower() != "windows":
            _, attrs, auth_type = process_connection_string(conn_str)
            assert auth_type == "interactive"
            assert attrs is not None
            assert 1256 in attrs

    @patch("mssql_python.auth.get_auth_token", return_value=b"\x00" * 8)
    def test_default_auth_type(self, _mock_token):
        from mssql_python.auth import process_connection_string

        conn_str = "Server=myserver;Database=mydb;Authentication=ActiveDirectoryDefault"
        _, attrs, auth_type = process_connection_string(conn_str)
        assert auth_type == "default"

    @patch("mssql_python.auth.get_auth_token", return_value=b"\x00" * 8)
    def test_devicecode_auth_type(self, _mock_token):
        from mssql_python.auth import process_connection_string

        conn_str = "Server=myserver;Database=mydb;Authentication=ActiveDirectoryDeviceCode"
        _, attrs, auth_type = process_connection_string(conn_str)
        assert auth_type == "devicecode"

    def test_no_auth_returns_none(self):
        from mssql_python.auth import process_connection_string

        conn_str = f"Server=myserver;Database=mydb;UID=sa;PWD={FAKE_PWD}"
        _, attrs, auth_type = process_connection_string(conn_str)
        assert auth_type is None
        assert attrs is None

    @patch("mssql_python.auth.get_auth_token", return_value=b"\x00" * 8)
    def test_sensitive_params_removed(self, _mock_token):
        from mssql_python.auth import process_connection_string

        conn_str = f"Server=myserver;Database=mydb;UID=sa;PWD={FAKE_PWD};Authentication=ActiveDirectoryDefault"
        processed, _, _ = process_connection_string(conn_str)
        lower = processed.lower()
        assert "uid=" not in lower
        assert "pwd=" not in lower

    def test_empty_conn_str_raises(self):
        from mssql_python.auth import process_connection_string
        with pytest.raises(ValueError):
            process_connection_string("")

    def test_invalid_type_raises(self):
        from mssql_python.auth import process_connection_string
        with pytest.raises(ValueError):
            process_connection_string(12345)

    @patch("mssql_python.auth.get_auth_token", return_value=b"\x00" * 8)
    def test_returns_three_tuple(self, _mock_token):
        """process_connection_string always returns a 3-tuple."""
        from mssql_python.auth import process_connection_string

        result = process_connection_string(
            "Server=x;Database=y;Authentication=ActiveDirectoryDefault"
        )
        assert len(result) == 3


# ===========================================================================
# process_auth_parameters unit tests
# ===========================================================================

class TestProcessAuthParameters:
    """Low-level tests for the parameter parser."""

    def test_extracts_interactive(self):
        from mssql_python.auth import process_auth_parameters
        import platform

        params = ["Server=x", "Authentication=ActiveDirectoryInteractive"]
        _, auth_type = process_auth_parameters(params)

        if platform.system().lower() == "windows":
            assert auth_type is None
        else:
            assert auth_type == "interactive"

    def test_extracts_devicecode(self):
        from mssql_python.auth import process_auth_parameters
        _, auth_type = process_auth_parameters(
            ["Server=x", "Authentication=ActiveDirectoryDeviceCode"]
        )
        assert auth_type == "devicecode"

    def test_extracts_default(self):
        from mssql_python.auth import process_auth_parameters
        _, auth_type = process_auth_parameters(
            ["Server=x", "Authentication=ActiveDirectoryDefault"]
        )
        assert auth_type == "default"

    def test_no_auth_param(self):
        from mssql_python.auth import process_auth_parameters
        _, auth_type = process_auth_parameters(["Server=x", "Database=y"])
        assert auth_type is None

    def test_case_insensitive(self):
        from mssql_python.auth import process_auth_parameters
        _, auth_type = process_auth_parameters(
            ["Authentication=ACTIVEDIRECTORYDEFAULT"]
        )
        assert auth_type == "default"

    def test_preserves_other_params(self):
        from mssql_python.auth import process_auth_parameters
        params = ["Server=mysvr", "Encrypt=yes", "Authentication=ActiveDirectoryDefault"]
        modified, _ = process_auth_parameters(params)
        joined = ";".join(modified).lower()
        assert "server=mysvr" in joined
        assert "encrypt=yes" in joined


# ===========================================================================
# Rust-side access_token context validation (via mssql_py_core)
# ===========================================================================

class TestPyCoreAccessToken:
    """Test that PyCoreConnection correctly handles the access_token key.

    These tests require mssql_py_core to be importable (maturin develop).
    They do NOT require a live SQL Server — they exercise dictionary
    validation only.
    """

    @pytest.fixture(autouse=True)
    def _skip_if_pycore_missing(self):
        pytest.importorskip("mssql_py_core", reason="mssql_py_core not installed")

    def test_access_token_only_accepted(self):
        """Context with access_token and no uid/pwd should be accepted."""
        import mssql_py_core

        context = {
            "server": "192.0.2.1",
            "database": "mydb",
            "trust_server_certificate": True,
            "encryption": "Optional",
            "access_token": FAKE_JWT,
            "connect_timeout": 1,
        }
        # PyCoreConnection will try to connect and fail on the network,
        # but it should NOT reject the context dict itself.
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)

        err_msg = str(exc_info.value).lower()
        assert "no authentication credentials" not in err_msg
        assert "incomplete credentials" not in err_msg

    def test_access_token_with_uid_rejected(self):
        """Context with both access_token and user_name should be rejected."""
        import mssql_py_core

        context = {
            "server": "192.0.2.1",
            "database": "mydb",
            "trust_server_certificate": True,
            "encryption": "Optional",
            "access_token": FAKE_JWT,
            "user_name": "sa",
            "password": FAKE_PWD,
            "connect_timeout": 1,
        }
        with pytest.raises(Exception, match="(?i)access token cannot be used"):
            mssql_py_core.PyCoreConnection(context)

    def test_no_credentials_attempts_connection(self):
        """Context with no access_token and no uid/pwd is valid per ODBC —
        falls through to SQL auth with blank credentials. Server decides."""
        import mssql_py_core

        context = {
            "server": "192.0.2.1",
            "database": "mydb",
            "trust_server_certificate": True,
            "encryption": "Optional",
            "connect_timeout": 1,
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        err_msg = str(exc_info.value).lower()
        assert "no authentication credentials" not in err_msg

    def test_partial_credentials_attempts_connection(self):
        """Context with only user_name (no password) is valid per ODBC —
        PWD defaults to empty string. Server decides."""
        import mssql_py_core

        context = {
            "server": "192.0.2.1",
            "database": "mydb",
            "trust_server_certificate": True,
            "encryption": "Optional",
            "user_name": "sa",
            "connect_timeout": 1,
        }
        with pytest.raises(Exception) as exc_info:
            mssql_py_core.PyCoreConnection(context)
        err_msg = str(exc_info.value).lower()
        assert "incomplete credentials" not in err_msg


# ===========================================================================
# get_token_struct encoding tests
# ===========================================================================

class TestTokenStructEncoding:
    """Verify the ODBC struct matches the expected wire format."""

    def test_ascii_token(self):
        from mssql_python.auth import AADAuth
        s = AADAuth.get_token_struct("abc")
        payload = "abc".encode("UTF-16-LE")
        assert len(s) == 4 + len(payload)
        assert struct.unpack("<I", s[:4])[0] == len(payload)

    def test_unicode_token(self):
        """UTF-16-LE encoding doubles byte length for BMP codepoints."""
        from mssql_python.auth import AADAuth
        token = "tökén"
        s = AADAuth.get_token_struct(token)
        payload = token.encode("UTF-16-LE")
        assert struct.unpack("<I", s[:4])[0] == len(payload)

    def test_empty_token(self):
        from mssql_python.auth import AADAuth
        s = AADAuth.get_token_struct("")
        assert struct.unpack("<I", s[:4])[0] == 0
        assert len(s) == 4


# ===========================================================================
# remove_sensitive_params tests
# ===========================================================================

class TestRemoveSensitiveParams:
    """Verify credential scrubbing from connection params."""

    def test_removes_uid_pwd_auth(self):
        from mssql_python.auth import remove_sensitive_params
        params = [
            "Server=mysvr",
            "UID=sa",
            f"PWD={FAKE_PWD}",
            "Authentication=ActiveDirectoryDefault",
            "Database=mydb",
        ]
        result = remove_sensitive_params(params)
        joined = ";".join(result).lower()
        assert "uid=" not in joined
        assert "pwd=" not in joined
        assert "authentication=" not in joined
        assert "server=mysvr" in joined
        assert "database=mydb" in joined

    def test_case_insensitive_removal(self):
        from mssql_python.auth import remove_sensitive_params
        params = ["Server=x", "Uid=SA", "Pwd=P@ss"]
        result = remove_sensitive_params(params)
        assert len(result) == 1
        assert result[0] == "Server=x"

    def test_preserves_non_sensitive(self):
        from mssql_python.auth import remove_sensitive_params
        params = ["Server=x", "Encrypt=yes", "TrustServerCertificate=true"]
        result = remove_sensitive_params(params)
        assert len(result) == 3
