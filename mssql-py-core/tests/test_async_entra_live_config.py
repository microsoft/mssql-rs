# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Configuration guards for opt-in live authentication validation."""

import pytest

from . import conftest


@pytest.fixture
def live_environment(monkeypatch):
    monkeypatch.setattr(conftest, "_load_env", lambda: None)
    for key in [
        "AQE_ENTRA_TEST",
        "SQL_SERVER",
        "SQL_DATABASE",
        "AZURE_CLIENT_ID",
        "AZURE_CLIENT_SECRET",
        "MI_CLIENT_ID",
    ]:
        monkeypatch.delenv(key, raising=False)
    return monkeypatch


def test_live_auth_is_disabled_without_explicit_opt_in(live_environment):
    with pytest.raises(pytest.skip.Exception, match="AQE_ENTRA_TEST"):
        conftest._build_context_async_entra("ActiveDirectoryServicePrincipal")


@pytest.mark.parametrize(
    "missing", ["SQL_SERVER", "SQL_DATABASE", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET"]
)
def test_enabled_live_auth_fails_for_missing_inputs(live_environment, missing):
    for key in ["SQL_SERVER", "SQL_DATABASE", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET"]:
        live_environment.setenv(key, "test-value")
    live_environment.setenv("AQE_ENTRA_TEST", "1")
    live_environment.delenv(missing)
    with pytest.raises(pytest.fail.Exception, match=missing):
        conftest._build_context_async_entra("ActiveDirectoryServicePrincipal")


def test_live_service_principal_uses_native_auth_and_validates_tls(live_environment):
    live_environment.setenv("AQE_ENTRA_TEST", "1")
    live_environment.setenv("SQL_SERVER", "test.database.windows.net")
    live_environment.setenv("SQL_DATABASE", "test-db")
    live_environment.setenv("AZURE_CLIENT_ID", "test-client")
    live_environment.setenv("AZURE_CLIENT_SECRET", "test-secret")
    live_environment.setenv("TRUST_SERVER_CERTIFICATE", "true")
    context = conftest._build_context_async_entra("ActiveDirectoryServicePrincipal")
    assert context == {
        "server": "test.database.windows.net",
        "database": "test-db",
        "authentication": "ActiveDirectoryServicePrincipal",
        "encryption": "Mandatory",
        "trust_server_certificate": False,
        "connect_timeout": 30,
        "connect_retry_count": 0,
        "user_name": "test-client",
        "password": "test-secret",
    }


@pytest.mark.parametrize("client_id", [None, "managed-identity-client"])
def test_live_managed_identity_does_not_inherit_service_principal_credentials(
    live_environment, client_id
):
    live_environment.setenv("AQE_ENTRA_TEST", "1")
    live_environment.setenv("SQL_SERVER", "test.database.windows.net")
    live_environment.setenv("SQL_DATABASE", "test-db")
    live_environment.setenv("AZURE_CLIENT_ID", "service-principal-client")
    live_environment.setenv("AZURE_CLIENT_SECRET", "service-principal-secret")
    if client_id is not None:
        live_environment.setenv("MI_CLIENT_ID", client_id)
    context = conftest._build_context_async_entra("ActiveDirectoryMSI")
    assert context.get("user_name") == client_id
    assert "password" not in context
    assert "access_token" not in context
