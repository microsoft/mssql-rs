# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Custom async credential login through the native FedAuth handshake."""

import asyncio
import contextvars
import gc
import threading
import warnings
from types import SimpleNamespace

import mssql_py_core
import pytest


def connect(context, **kwargs):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return mssql_py_core.PyAsyncConnection.connect(context, **kwargs)


def provider_context(context):
    return {key: value for key, value in context.items() if key != "access_token"}


def test_custom_credential_fedauth_login_and_query(
    mock_client_context, mock_tds_server, monkeypatch
):
    async def run():
        context = provider_context(mock_client_context)
        original = dict(context)
        loop = asyncio.get_running_loop()
        thread = threading.get_ident()
        identity = contextvars.ContextVar("identity")
        identity.set("custom-user")
        calls = []

        class Credential:
            async def get_token(self, scope):
                assert asyncio.get_running_loop() is loop
                assert threading.get_ident() == thread
                assert identity.get() == "custom-user"
                calls.append(scope)
                await asyncio.sleep(0)
                return SimpleNamespace(token="custom-async-fedauth-token")

            async def close(self):
                pytest.fail("Caller-owned credential was closed")

        def unexpected_executor(*args, **kwargs):
            pytest.fail("Native async login used an executor")

        monkeypatch.setattr(loop, "run_in_executor", unexpected_executor)
        connection = await asyncio.wait_for(
            connect(context, token_provider=Credential(), autocommit=True), 5
        )
        try:
            assert context == original
            assert connection.is_connected()
            cursor = connection.cursor()
            try:
                await cursor.execute("SELECT 1", use_prepare=False)
                assert await cursor.fetchone() == (1,)
            finally:
                await cursor.close()
            assert calls == ["https://database.windows.net/.default"]
            assert mock_tds_server.get_last_access_token() == "custom-async-fedauth-token"
        finally:
            await connection.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    "conflict",
    [
        {"authentication": "ActiveDirectoryDefault"},
        {"authentication": "SqlPassword"},
        {"authentication": "Typo"},
        {"user_name": "user"},
        {"password": "secret"},
        {"trusted_connection": "Yes"},
        {"access_token": "raw-token"},
        {"access_token": ""},
        {"entra_id_token_factory": lambda *args: b"token"},
    ],
)
def test_conflicting_sources_rejected_before_connection(conflict):
    class Credential:
        async def get_token(self, scope):
            pytest.fail("Conflicting settings reached token acquisition")

    context = {"server": "127.0.0.1,1", **conflict}
    original = dict(context)
    with pytest.raises(mssql_py_core.InterfaceError, match="token_provider"):
        connect(context, token_provider=Credential())
    assert context == original


@pytest.mark.parametrize("provider", [object(), SimpleNamespace(get_token=42)])
def test_invalid_provider_rejected_before_connection(provider):
    async def run():
        with pytest.raises(TypeError, match="callable get_token"):
            connect({"server": "127.0.0.1,1"}, token_provider=provider)

    asyncio.run(run())


def test_token_provider_is_keyword_only():
    with pytest.raises(TypeError):
        mssql_py_core.PyAsyncConnection.connect({}, None, False, object())


def test_empty_authentication_options_are_not_competing_sources(mock_client_context):
    async def run():
        class Credential:
            async def get_token(self, scope):
                return SimpleNamespace(token="custom-token")

        context = provider_context(mock_client_context)
        context.update(
            authentication="",
            user_name="",
            password="",
            trusted_connection="No",
            access_token=None,
            entra_id_token_factory=None,
        )
        connection = await connect(context, token_provider=Credential())
        await connection.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    "outcome",
    ["provider-error", "nonawaitable", "invalid-token", "provider-cancelled"],
)
def test_failed_acquisition_is_terminal_and_native_error_is_safe(
    mock_client_context, mock_tds_server, outcome
):
    async def run():
        calls = []
        secret = "private-provider-diagnostic"

        class Credential:
            def get_token(self, scope):
                calls.append(scope)
                if outcome == "nonawaitable":
                    return SimpleNamespace(token="not-awaited")

                async def acquire():
                    if outcome == "provider-error":
                        raise RuntimeError(secret)
                    if outcome == "provider-cancelled":
                        raise asyncio.CancelledError
                    return SimpleNamespace(token=None)

                return acquire()

            async def close(self):
                pytest.fail("Caller-owned credential was closed")

        expected = (
            asyncio.CancelledError
            if outcome == "provider-cancelled"
            else mssql_py_core.OperationalError
        )
        with pytest.raises(expected) as caught:
            await asyncio.wait_for(
                connect(
                    provider_context(mock_client_context),
                    token_provider=Credential(),
                ),
                5,
            )
        assert secret not in str(caught.value)
        assert len(calls) == 1
        assert not mock_tds_server.has_received_token("not-awaited")

    asyncio.run(run())


@pytest.mark.parametrize("late_success", [False, True])
def test_cancel_connect_cancels_and_observes_acquisition(
    mock_client_context, mock_tds_server, late_success
):
    async def run():
        started = asyncio.Event()
        settled = asyncio.Event()
        loop = asyncio.get_running_loop()
        unhandled = []
        loop.set_exception_handler(lambda _, context: unhandled.append(context))

        class Credential:
            async def get_token(self, scope):
                started.set()
                try:
                    await asyncio.Future()
                except asyncio.CancelledError:
                    settled.set()
                    if late_success:
                        return SimpleNamespace(token="late-token")
                    raise RuntimeError("credential stopped after caller cancellation")

            async def close(self):
                pytest.fail("Caller-owned credential was closed")

        future = connect(
            provider_context(mock_client_context), token_provider=Credential()
        )
        await asyncio.wait_for(started.wait(), 5)
        future.cancel()
        with pytest.raises(asyncio.CancelledError):
            await future
        await asyncio.wait_for(settled.wait(), 2)
        for _ in range(5):
            await asyncio.sleep(0)
        gc.collect()
        assert not unhandled
        assert not mock_tds_server.has_received_token("late-token")

    asyncio.run(run())


def test_login_timeout_cancels_pending_acquisition(mock_client_context):
    async def run():
        started = asyncio.Event()
        settled = asyncio.Event()

        class Credential:
            async def get_token(self, scope):
                started.set()
                try:
                    await asyncio.Future()
                finally:
                    settled.set()

        context = provider_context(mock_client_context)
        context.update(connect_timeout=1, connect_retry_count=0)
        with pytest.raises(mssql_py_core.OperationalError, match="(?i)timeout"):
            await asyncio.wait_for(connect(context, token_provider=Credential()), 3)
        assert started.is_set()
        await asyncio.wait_for(settled.wait(), 2)

    asyncio.run(run())


def test_concurrent_logins_keep_credentials_separate(
    mock_client_context, mock_tds_server
):
    async def run():
        class Credential:
            def __init__(self, token):
                self.token = token

            async def get_token(self, scope):
                await asyncio.sleep(0)
                return SimpleNamespace(token=self.token)

        context = provider_context(mock_client_context)
        tokens = ["first-user-token", "second-user-token"]
        connections = await asyncio.gather(
            *(connect(context, token_provider=Credential(token)) for token in tokens)
        )
        try:
            assert all(connection.is_connected() for connection in connections)
            assert all(mock_tds_server.has_received_token(token) for token in tokens)
        finally:
            await asyncio.gather(*(connection.close() for connection in connections))

    asyncio.run(run())


def test_explicit_none_preserves_raw_token_login(mock_client_context, mock_tds_server):
    async def run():
        connection = await connect(mock_client_context, token_provider=None)
        await connection.close()
        assert mock_tds_server.get_last_access_token() == mock_client_context["access_token"]

    asyncio.run(run())
