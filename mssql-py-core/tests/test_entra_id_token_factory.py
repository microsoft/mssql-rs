# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""
Integration tests for the Entra ID token factory callback registered from Python.

mssql-py-core accepts an optional `entra_id_token_factory` key in the client
context dict. When present, it is wired into `ClientContext::auth_method_map`
under the resolved auth method so that mssql-tds can invoke it during the
FedAuth handshake (workflow 0x02) for methods like ActiveDirectoryServicePrincipal.

These tests cover the dict/plumbing behavior plus wire-level coverage of the
FedAuth challenge handshake against the mock TDS server. The mock-server test
(TestServicePrincipalFedAuthOverMockServer) is the regression guard for the
connection-time GIL release: without it the spawn_blocking token callback
deadlocks against the GIL held by the connecting thread.

Usage:
    ./dev/test-python.sh
"""

import secrets

import pytest

# The mock TDS server Python bindings (mssql-mock-tds-py) are optional: they are
# built by dev/test-python.sh but may be absent in other environments.
try:
    import mssql_mock_tds_py

    MOCK_TDS_PY_AVAILABLE = True
except ImportError:
    MOCK_TDS_PY_AVAILABLE = False


class TestEntraIdTokenFactoryDictKey:
    """Verify the entra_id_token_factory dict key is accepted by PyCoreConnection
    construction without forcing a real Azure AD token acquisition."""

    def test_none_factory_is_ignored(self):
        """Passing entra_id_token_factory=None must not cause an error during
        client_context construction. The dict-to-context conversion happens
        before any network I/O, so we only need the construction to not
        raise on the factory key itself."""
        import mssql_py_core

        ctx = {
            "server": "127.0.0.1,1",  # bogus, we only care about pre-connect parsing
            "user_name": "alice",
            "password": "secret",
            "encryption": "Optional",
            "trust_server_certificate": True,
            "entra_id_token_factory": None,
        }
        # The connection will fail (no server), but the failure must be a
        # connection error, not a TypeError from the factory dict path.
        with pytest.raises(RuntimeError) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        assert "entra_id_token_factory" not in str(exc_info.value), (
            f"Expected connection failure, got token-factory error: {exc_info.value}"
        )

    def test_callable_factory_is_accepted(self):
        """A callable factory is accepted and registered. Connection still
        fails (no server), but again the failure must come from connect, not
        from the factory dict path."""
        import mssql_py_core

        def factory(spn, sts_url, auth_method):
            return b"unused_token_bytes"

        ctx = {
            "server": "127.0.0.1,1",
            "user_name": "11111111-2222-3333-4444-555555555555",
            "password": "client-secret",
            "authentication": "ActiveDirectoryServicePrincipal",
            "encryption": "Optional",
            "trust_server_certificate": True,
            "entra_id_token_factory": factory,
        }
        with pytest.raises(RuntimeError) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        # Must not be a factory-key parse error.
        assert "entra_id_token_factory" not in str(exc_info.value), (
            f"Factory key rejected: {exc_info.value}"
        )

    @pytest.mark.parametrize(
        "non_callable",
        [
            "not-a-callable",
            42,
            b"raw-bytes",
            object(),
            ["list"],
            {"key": "value"},
        ],
        ids=["str", "int", "bytes", "object", "list", "dict"],
    )
    def test_non_callable_factory_raises_type_error(self, non_callable):
        """A non-None, non-callable factory must be rejected up front with a
        TypeError, instead of being silently registered and failing later
        during the FedAuth handshake when the bridge tries to invoke it."""
        import mssql_py_core

        ctx = {
            "server": "127.0.0.1,1",
            "user_name": "11111111-2222-3333-4444-555555555555",
            "password": "client-secret",
            "authentication": "ActiveDirectoryServicePrincipal",
            "encryption": "Optional",
            "trust_server_certificate": True,
            "entra_id_token_factory": non_callable,
        }
        with pytest.raises(TypeError) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        assert "entra_id_token_factory" in str(exc_info.value), (
            f"Expected entra_id_token_factory TypeError, got: {exc_info.value}"
        )
        assert "callable" in str(exc_info.value), (
            f"Expected error to mention 'callable', got: {exc_info.value}"
        )

    def test_factory_without_auth_method_still_accepted(self):
        """Even when the resolved auth method does not normally need a factory
        (e.g. SQL Password), passing one is harmless — it is registered but
        never invoked by the wire path."""
        import mssql_py_core

        def factory(spn, sts_url, auth_method):
            return b"unused"

        ctx = {
            "server": "127.0.0.1,1",
            "user_name": "sa",
            "password": "pw",
            "encryption": "Optional",
            "trust_server_certificate": True,
            "entra_id_token_factory": factory,
        }
        with pytest.raises(RuntimeError) as exc_info:
            mssql_py_core.PyCoreConnection(ctx)
        assert "entra_id_token_factory" not in str(exc_info.value), (
            f"Factory key rejected on SQL auth path: {exc_info.value}"
        )


class TestInvokeEntraIdTokenFactory:
    """Direct exercises of `PythonEntraIdTokenFactory::create_token` via the
    `_invoke_entra_id_token_factory` test hook.

    The dict-key tests above only verify that `PyCoreConnection::new` accepts
Python callback. These
    tests cover the bridge itself: success path, Python-exception mapping,
    and non-bytes-return mapping.
    """

    def test_success_returns_bytes_unchanged(self):
        import mssql_py_core

        def factory(spn, sts_url, auth_method):
            return b"hello-token-bytes"

        result = mssql_py_core._invoke_entra_id_token_factory(
            factory,
            "https://database.windows.net/",
            "https://login.microsoftonline.com/tenant-guid/",
        )
        assert result == b"hello-token-bytes"

    def test_args_pass_through_to_python(self):
        import mssql_py_core

        captured = {}

        def factory(spn, sts_url, auth_method):
            captured["spn"] = spn
            captured["sts_url"] = sts_url
            captured["auth_method"] = auth_method
            return b"ok"

        mssql_py_core._invoke_entra_id_token_factory(
            factory,
            "https://database.windows.net/",
            "https://login.microsoftonline.com/tenant-guid/",
        )
        assert captured == {
            "spn": "https://database.windows.net/",
            "sts_url": "https://login.microsoftonline.com/tenant-guid/",
            "auth_method": "activedirectoryserviceprincipal",
        }

    def test_python_exception_maps_to_runtime_error(self):
        import mssql_py_core

        def factory(spn, sts_url, auth_method):
            raise RuntimeError("boom from python")

        with pytest.raises(RuntimeError) as exc_info:
            mssql_py_core._invoke_entra_id_token_factory(
                factory,
                "https://database.windows.net/",
                "https://login.microsoftonline.com/tenant-guid/",
            )
        msg = str(exc_info.value)
        assert "token callback raised" in msg, f"got: {msg}"
        assert "boom from python" in msg, f"got: {msg}"

    def test_non_bytes_return_maps_to_runtime_error(self):
        import mssql_py_core

        def factory(spn, sts_url, auth_method):
            return "not bytes, a string"

        with pytest.raises(RuntimeError) as exc_info:
            mssql_py_core._invoke_entra_id_token_factory(
                factory,
                "https://database.windows.net/",
                "https://login.microsoftonline.com/tenant-guid/",
            )
        assert "non-bytes value" in str(exc_info.value), (
            f"got: {exc_info.value}"
        )

    def test_none_return_maps_to_runtime_error(self):
        import mssql_py_core

        def factory(spn, sts_url, auth_method):
            return None

        with pytest.raises(RuntimeError) as exc_info:
            mssql_py_core._invoke_entra_id_token_factory(
                factory,
                "https://database.windows.net/",
                "https://login.microsoftonline.com/tenant-guid/",
            )
        assert "non-bytes value" in str(exc_info.value), (
            f"got: {exc_info.value}"
        )


@pytest.mark.skipif(
    not MOCK_TDS_PY_AVAILABLE,
    reason="mssql_mock_tds_py not available. Build it with: cd mssql-mock-tds-py && maturin develop",
)
class TestServicePrincipalFedAuthOverMockServer:
    """Wire-level coverage of the FedAuth challenge handshake driving the Python
    entra_id_token_factory. The connect path must release the GIL so the
    spawn_blocking token callback can acquire it; otherwise this deadlocks."""

    def test_service_principal_uses_entra_id_token_factory(self):
        import time

        import mssql_py_core

        raw_token = f"service_principal_token_{secrets.token_hex(8)}"
        callback_calls = []

        def factory(spn, sts_url, auth_method):
            callback_calls.append((spn, sts_url, auth_method))
            return raw_token.encode("utf-16-le")

        server = mssql_mock_tds_py.PyMockTdsServer(port=0, tls=True)

        with server:
            client_context = {
                "server": server.sql_address,
                "database": "master",
                "user_name": "11111111-2222-3333-4444-555555555555",
                "password": "not" + "real",
                "authentication": "ActiveDirectoryServicePrincipal",
                "entra_id_token_factory": factory,
                "encryption": "Optional",
                "trust_server_certificate": True,
            }

            conn = mssql_py_core.PyCoreConnection(client_context)
            assert conn.is_connected()

            cursor = conn.cursor()
            cursor.execute("SELECT 1")
            row = cursor.fetchone()
            assert row is not None
            assert row[0] == 1

            cursor.close()
            conn.close()

            del cursor
            del conn

            time.sleep(0.3)

            assert callback_calls == [
                (
                    "https://database.windows.net/",
                    "https://login.microsoftonline.com/test-tenant/",
                    "activedirectoryserviceprincipal",
                )
            ]
            assert server.has_received_token(raw_token), (
                "Server should have received the token returned by entra_id_token_factory"
            )
            assert server.get_last_access_token() == raw_token
