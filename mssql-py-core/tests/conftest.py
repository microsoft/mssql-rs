# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Shared test fixtures and utilities.

Auth mode (SMOKE_AUTH_MODE env var):
  sql_auth          — user_name / password (default, used by all existing tests)
  access_token      — pre-fetched token via ACCESS_TOKEN env var
  managed_identity  — azure-identity ManagedIdentityCredential (ACI with user-assigned MI)
"""

import os

import pytest
from pathlib import Path
from dotenv import load_dotenv

try:
    import mssql_mock_tds
except ImportError:
    mssql_mock_tds = None


# ---------------------------------------------------------------------------
# Env helpers
# ---------------------------------------------------------------------------

def _load_env():
    env_path = Path(__file__).parent.parent.parent / ".env"
    if env_path.exists():
        load_dotenv(env_path)


def get_db_credentials():
    """Get database credentials from environment variables or /tmp/password."""
    _load_env()

    username = os.environ.get("DB_USERNAME", "sa")

    # Try SQL_PASSWORD env var first, then /tmp/password file
    password = os.environ.get("SQL_PASSWORD")
    if not password:
        try:
            with open("/tmp/password", "r") as f:
                password = f.read().strip()
        except FileNotFoundError:
            pytest.skip("SQL_PASSWORD not set and /tmp/password not found")

    return username, password


def get_server_address():
    """Get server address from environment variable."""
    return os.environ.get("SQL_SERVER", "localhost")


def get_database_name():
    """Get database name from environment variable."""
    return os.environ.get("SQL_DATABASE", "master")


def trust_server_certificate():
    """Get trust server certificate setting from environment."""
    return os.environ.get("TRUST_SERVER_CERTIFICATE", "false").lower() == "true"


# ---------------------------------------------------------------------------
# Auth-mode-aware context builders
# ---------------------------------------------------------------------------

def _get_auth_mode():
    return os.environ.get("SMOKE_AUTH_MODE", "sql_auth")


def _build_context_sql_auth():
    username, password = get_db_credentials()
    return {
        "server": get_server_address(),
        "database": get_database_name(),
        "user_name": username,
        "password": password,
        "trust_server_certificate": trust_server_certificate(),
        "encryption": os.environ.get("ENCRYPTION", "Optional"),
    }


def _build_context_access_token():
    token = os.environ.get("ACCESS_TOKEN")
    if not token:
        pytest.skip("ACCESS_TOKEN not set")
    return {
        "server": get_server_address(),
        "database": get_database_name(),
        "access_token": token,
        "trust_server_certificate": True,
        "encryption": os.environ.get("ENCRYPTION", "Mandatory"),
    }


def _build_context_managed_identity():
    from azure.identity import ManagedIdentityCredential

    client_id = os.environ.get(
        "MI_CLIENT_ID", "d5c2ef7c-cb1b-4be6-b2bd-19ca351aed80"
    )
    credential = ManagedIdentityCredential(client_id=client_id)
    token = credential.get_token("https://database.windows.net/.default").token
    return {
        "server": os.environ.get(
            "SQL_SERVER", "mssqlrustlibtest.database.windows.net"
        ),
        "database": os.environ.get("SQL_DATABASE", "librarytest"),
        "access_token": token,
        "trust_server_certificate": False,
        "encryption": os.environ.get("ENCRYPTION", "Mandatory"),
    }


_AUTH_BUILDERS = {
    "sql_auth": _build_context_sql_auth,
    "access_token": _build_context_access_token,
    "managed_identity": _build_context_managed_identity,
}


def get_client_context():
    """Build client context dict based on SMOKE_AUTH_MODE (default: sql_auth)."""
    mode = _get_auth_mode()
    builder = _AUTH_BUILDERS.get(mode)
    if not builder:
        pytest.fail(f"Unknown SMOKE_AUTH_MODE={mode!r}. Use: {list(_AUTH_BUILDERS)}")
    return builder()


@pytest.fixture
def client_context():
    """Pytest fixture that provides client context for database connections."""
    return get_client_context()


@pytest.fixture
def mock_client_context():
    """Client context backed by an ephemeral mock TDS server."""
    if mssql_mock_tds is None:
        pytest.skip("mssql_mock_tds is not installed")

    server = mssql_mock_tds.PyMockTdsServer(port=0, tls=True)
    with server:
        yield {
            "server": server.sql_address,
            "database": "master",
            "access_token": "async-cursor-test-token",
            "encryption": "Optional",
            "trust_server_certificate": True,
        }


@pytest.fixture
def connection():
    """Pytest fixture that provides a connected PyCoreConnection instance."""
    import mssql_py_core

    context = get_client_context()
    conn = mssql_py_core.PyCoreConnection(context)
    yield conn
    if conn.is_connected():
        conn.close()
