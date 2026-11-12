# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Provider cancellation across an actual native session-recovery handshake."""

import asyncio
import warnings
from types import SimpleNamespace

import mssql_py_core
import pytest


@pytest.mark.parametrize("operation", ["execute", "executemany"])
@pytest.mark.parametrize("retry_count", [1, 3])
@pytest.mark.parametrize(
    "outcome", ["cancel-once", "cancel-always", "cancel-on-final-attempt", "success"]
)
@pytest.mark.parametrize("owned", [False, True])
@pytest.mark.parametrize("use_prepare", [False, True])
def test_provider_cancellation_stops_recovery_before_query_submission(
    mock_client_context,
    mock_tds_server,
    monkeypatch,
    operation,
    retry_count,
    outcome,
    owned,
    use_prepare,
):
    async def run():
        mock_tds_server.set_session_recovery(True)
        closing_query = "SELECT 1 AS close_session"
        mock_tds_server.close_connection_after(closing_query)
        calls = []
        closed = []
        expected_calls = retry_count + 2 if outcome == "cancel-on-final-attempt" else 2

        async def wait_for_request(recovery_login):
            while not any(
                info.recovery_login == recovery_login and info.request_count > 0
                for info in mock_tds_server.get_connections()
            ):
                await asyncio.sleep(0.01)

        async def wait_for_recovery_logins():
            while sum(
                info.recovery_login for info in mock_tds_server.get_connections()
            ) < expected_calls - 1:
                await asyncio.sleep(0.01)

        class Credential:
            async def get_token(self, scope, **kwargs):
                assert kwargs == ({"tenant_id": "test-tenant"} if owned else {})
                calls.append(scope)
                if outcome == "cancel-on-final-attempt" and len(calls) > 1:
                    if len(calls) < expected_calls:
                        raise RuntimeError("Controlled recovery acquisition failure")
                    if len(calls) == expected_calls:
                        raise asyncio.CancelledError()
                elif outcome != "success" and (
                    len(calls) == 2 or (outcome == "cancel-always" and len(calls) > 2)
                ):
                    raise asyncio.CancelledError()
                return SimpleNamespace(token=f"recovery-token-{len(calls)}")

            async def close(self):
                assert owned, "The driver must not close caller-owned credentials"
                closed.append(self)

        context = {
            key: value
            for key, value in mock_client_context.items()
            if key != "access_token"
        }
        context.update(
            connect_retry_count=retry_count,
            connect_retry_interval=0,
            connect_timeout=5,
        )
        options = {}
        if owned:
            aio = pytest.importorskip("azure.identity.aio")
            pytest.importorskip("aiohttp")
            monkeypatch.setattr(aio, "DefaultAzureCredential", lambda **kwargs: Credential())
            context["authentication"] = "ActiveDirectoryDefault"
        else:
            options["token_provider"] = Credential()
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", FutureWarning)
            connection = await mssql_py_core.PyAsyncConnection.connect(
                context, autocommit=True, **options
            )
        try:
            cursor = connection.cursor()
            try:
                await cursor.execute(closing_query, use_prepare=False)
                assert await cursor.fetchone() == (1,)
                assert await cursor.fetchone() is None
                # Wait until teardown has recorded the server-closed transport.
                await asyncio.wait_for(wait_for_request(False), 3)

                if operation == "execute":
                    pending = cursor.execute("SELECT 2", use_prepare=use_prepare)
                else:
                    pending = cursor.executemany(
                        "SELECT ?", [(2,), (3,)], use_prepare=use_prepare
                    )
                if outcome == "success":
                    await asyncio.wait_for(pending, 5)
                else:
                    with pytest.raises(asyncio.CancelledError):
                        await asyncio.wait_for(pending, 5)
                assert len(calls) == expected_calls
                assert len(closed) == (expected_calls if owned else 0)
                if owned:
                    assert len({id(credential) for credential in closed}) == expected_calls
            finally:
                await asyncio.wait_for(cursor.close(), 5)
        finally:
            await asyncio.wait_for(connection.close(), 5)

        await asyncio.wait_for(wait_for_recovery_logins(), 3)
        connections = mock_tds_server.get_connections()
        recovery = [info for info in connections if info.recovery_login]
        assert len(recovery) == expected_calls - 1
        if outcome != "success":
            assert sum(info.request_count for info in connections) == 1
            assert all(info.request_count == 0 and not info.authenticated for info in recovery)
            assert not mock_tds_server.has_received_token(
                f"recovery-token-{expected_calls + 1}"
            )
        else:
            await asyncio.wait_for(wait_for_request(True), 3)

    asyncio.run(run())
