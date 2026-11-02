# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Built-in aio credential selection and ownership over mock FedAuth login."""

import asyncio
import contextvars
import gc
import sys
import threading
import warnings
from types import SimpleNamespace

import mssql_py_core
import pytest


MODES = [
    ("ActiveDirectoryDefault", "DefaultAzureCredential", {}),
    ("ActiveDirectoryMsi", "ManagedIdentityCredential", {}),
    (
        "ActiveDirectoryMSI",
        "ManagedIdentityCredential",
        {"user_name": "user-assigned-client-id"},
    ),
    (
        "ActiveDirectoryServicePrincipal",
        "ClientSecretCredential",
        {"user_name": "client-id", "password": "client-secret"},
    ),
]


def connect(context, **kwargs):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        return mssql_py_core.PyAsyncConnection.connect(context, **kwargs)


@pytest.fixture
def builtin_context(mock_client_context):
    return {
        key: value for key, value in mock_client_context.items() if key != "access_token"
    }


@pytest.fixture
def builtin_sdk(monkeypatch):
    aio = pytest.importorskip("azure.identity.aio")
    pytest.importorskip("aiohttp")
    state = SimpleNamespace(instances=[], get_hook=None, close_hook=None)

    def constructor(name):
        def create(**kwargs):
            instance = SimpleNamespace(
                name=name,
                kwargs=kwargs,
                loop=asyncio.get_running_loop(),
                thread=threading.get_ident(),
                scopes=[],
                close_calls=0,
                close_finished=False,
                token=f"{name}-token",
            )

            async def get_token(scope):
                assert asyncio.get_running_loop() is instance.loop
                assert threading.get_ident() == instance.thread
                instance.scopes.append(scope)
                if state.get_hook is not None:
                    return await state.get_hook(instance)
                await asyncio.sleep(0)
                return SimpleNamespace(token=instance.token)

            async def close():
                assert asyncio.get_running_loop() is instance.loop
                assert threading.get_ident() == instance.thread
                instance.close_calls += 1
                try:
                    if state.close_hook is not None:
                        await state.close_hook(instance)
                finally:
                    instance.close_finished = True

            instance.get_token = get_token
            instance.close = close
            state.instances.append(instance)
            return instance

        return create

    for name in {name for _, name, _ in MODES}:
        monkeypatch.setattr(aio, name, constructor(name))
    return state


@pytest.mark.parametrize("mode,class_name,options", MODES)
def test_builtin_mode_constructs_aio_credential_and_sends_token(
    builtin_context, builtin_sdk, mock_tds_server, monkeypatch, mode, class_name, options
):
    async def run():
        loop = asyncio.get_running_loop()
        identity = contextvars.ContextVar("identity")
        identity.set("builtin-caller")

        async def get_token(instance):
            assert identity.get() == "builtin-caller"
            return SimpleNamespace(token=instance.token)

        def unexpected_executor(*args, **kwargs):
            pytest.fail("Built-in acquisition must not use a driver executor")

        builtin_sdk.get_hook = get_token
        monkeypatch.setattr(loop, "run_in_executor", unexpected_executor)
        context = dict(builtin_context, authentication=mode, **options)
        original = dict(context)
        connection = await connect(context, autocommit=True)
        try:
            assert context == original
            assert len(builtin_sdk.instances) == 1
            instance = builtin_sdk.instances[0]
            assert instance.name == class_name
            assert instance.scopes == ["https://database.windows.net/.default"]
            assert instance.close_calls == 1
            assert instance.close_finished
            assert mock_tds_server.get_last_access_token() == instance.token
            if class_name == "ClientSecretCredential":
                assert instance.kwargs == {
                    "tenant_id": "test-tenant",
                    "client_id": "client-id",
                    "client_secret": "client-secret",
                    "authority": "login.microsoftonline.com",
                }
            elif class_name == "ManagedIdentityCredential":
                expected = {"client_id": options["user_name"]} if options else {}
                assert instance.kwargs == expected
            else:
                assert instance.kwargs == {"authority": "login.microsoftonline.com"}
            cursor = connection.cursor()
            try:
                await cursor.execute("SELECT 1", use_prepare=False)
                assert await cursor.fetchone() == (1,)
            finally:
                await cursor.close()
        finally:
            await connection.close()

    asyncio.run(run())


def test_constructor_failure_is_not_retried(
    builtin_context, builtin_sdk, monkeypatch
):
    import azure.identity.aio as aio

    calls = []

    def construct(**kwargs):
        calls.append(kwargs)
        raise RuntimeError("private-constructor-diagnostic")

    monkeypatch.setattr(aio, "DefaultAzureCredential", construct)

    async def run():
        with pytest.raises(mssql_py_core.OperationalError) as caught:
            await connect(dict(builtin_context, authentication="ActiveDirectoryDefault"))
        assert "private-constructor" not in str(caught.value)
        assert len(calls) == 1
        assert not builtin_sdk.instances

    asyncio.run(run())


@pytest.mark.parametrize("failure", ["synchronous-error", "nonawaitable", "cancelled"])
def test_get_token_startup_failure_still_closes_owned_credential(
    builtin_context, builtin_sdk, monkeypatch, failure
):
    import azure.identity.aio as aio

    construct = aio.DefaultAzureCredential

    def create(**kwargs):
        instance = construct(**kwargs)

        def get_token(scope):
            if failure == "synchronous-error":
                raise RuntimeError("private-startup-diagnostic")
            return None

        async def cancelled(scope):
            raise asyncio.CancelledError

        instance.get_token = cancelled if failure == "cancelled" else get_token
        return instance

    monkeypatch.setattr(aio, "DefaultAzureCredential", create)

    async def run():
        expected = (
            asyncio.CancelledError
            if failure == "cancelled"
            else mssql_py_core.OperationalError
        )
        with pytest.raises(expected):
            await connect(dict(builtin_context, authentication="ActiveDirectoryDefault"))
        assert len(builtin_sdk.instances) == 1
        assert builtin_sdk.instances[0].close_calls == 1
        assert builtin_sdk.instances[0].close_finished

    asyncio.run(run())


def test_owned_close_timeout_does_not_send_token(
    builtin_context, builtin_sdk, mock_tds_server
):
    async def run():
        cancelled = asyncio.Event()

        async def close(instance):
            try:
                await asyncio.Future()
            finally:
                cancelled.set()

        builtin_sdk.close_hook = close
        context = dict(
            builtin_context,
            authentication="ActiveDirectoryDefault",
            connect_timeout=15,
        )
        with pytest.raises(mssql_py_core.OperationalError):
            await asyncio.wait_for(connect(context), 8)
        assert cancelled.is_set()
        assert builtin_sdk.instances[0].close_calls == 1
        assert not mock_tds_server.has_received_token(builtin_sdk.instances[0].token)

    asyncio.run(run())


def test_token_is_not_sent_until_owned_credential_is_closed(
    builtin_context, builtin_sdk, mock_tds_server
):
    async def run():
        closing = asyncio.Event()
        release = asyncio.Event()

        async def close(instance):
            closing.set()
            await release.wait()

        builtin_sdk.close_hook = close
        future = connect(dict(builtin_context, authentication="ActiveDirectoryDefault"))
        await asyncio.wait_for(closing.wait(), 2)
        try:
            assert not future.done()
            assert not mock_tds_server.has_received_token(builtin_sdk.instances[0].token)
        finally:
            release.set()
        connection = await future
        await connection.close()
        assert builtin_sdk.instances[0].close_finished

    asyncio.run(run())


@pytest.mark.parametrize("failure", ["get-token", "invalid-token", "close", "both"])
def test_owned_credential_cleanup_on_failure(
    builtin_context, builtin_sdk, mock_tds_server, failure
):
    async def run():
        async def get_token(instance):
            if failure in ("get-token", "both"):
                raise RuntimeError("private-sdk-diagnostic")
            return SimpleNamespace(
                token=None if failure == "invalid-token" else instance.token
            )

        async def close(instance):
            if failure in ("close", "both"):
                raise RuntimeError("private-sdk-close-diagnostic")

        builtin_sdk.get_hook = get_token
        builtin_sdk.close_hook = close
        with pytest.raises(mssql_py_core.OperationalError) as caught:
            await connect(dict(builtin_context, authentication="ActiveDirectoryDefault"))
        assert "private-sdk" not in str(caught.value)
        assert len(builtin_sdk.instances) == 1
        instance = builtin_sdk.instances[0]
        assert instance.close_calls == 1
        assert instance.close_finished
        assert not mock_tds_server.has_received_token(instance.token)

    asyncio.run(run())


@pytest.mark.parametrize("cancel_during_close", [False, True])
def test_cancel_connect_does_not_abandon_owned_cleanup(
    builtin_context, builtin_sdk, mock_tds_server, cancel_during_close
):
    async def run():
        entered = asyncio.Event()
        closed = asyncio.Event()
        release_close = asyncio.Event()
        loop = asyncio.get_running_loop()
        unhandled = []
        loop.set_exception_handler(lambda _, context: unhandled.append(context))

        async def get_token(instance):
            if cancel_during_close:
                return SimpleNamespace(token=instance.token)
            entered.set()
            await asyncio.Future()

        async def close(instance):
            if cancel_during_close:
                entered.set()
                await release_close.wait()
            closed.set()

        builtin_sdk.get_hook = get_token
        builtin_sdk.close_hook = close
        future = connect(dict(builtin_context, authentication="ActiveDirectoryDefault"))
        await asyncio.wait_for(entered.wait(), 2)
        future.cancel()
        future.cancel()
        with pytest.raises(asyncio.CancelledError):
            await future
        release_close.set()
        await asyncio.wait_for(closed.wait(), 2)
        for _ in range(5):
            await asyncio.sleep(0)
        gc.collect()
        assert builtin_sdk.instances[0].close_calls == 1
        assert not mock_tds_server.has_received_token(builtin_sdk.instances[0].token)
        assert not unhandled

    asyncio.run(run())


def test_login_timeout_still_closes_owned_credential(builtin_context, builtin_sdk):
    async def run():
        closed = asyncio.Event()

        async def get_token(instance):
            await asyncio.Future()

        async def close(instance):
            closed.set()

        builtin_sdk.get_hook = get_token
        builtin_sdk.close_hook = close
        context = dict(
            builtin_context,
            authentication="ActiveDirectoryDefault",
            connect_timeout=1,
            connect_retry_count=0,
        )
        with pytest.raises(mssql_py_core.OperationalError, match="(?i)timeout"):
            await asyncio.wait_for(connect(context), 3)
        await asyncio.wait_for(closed.wait(), 2)
        assert builtin_sdk.instances[0].close_calls == 1

    asyncio.run(run())


@pytest.mark.parametrize("module", ["azure.identity.aio", "aiohttp"])
def test_missing_optional_dependency_is_explicit(monkeypatch, module):
    monkeypatch.setitem(sys.modules, module, None)
    with pytest.raises(mssql_py_core.OperationalError, match="azure-async"):
        connect(
            {
                "server": "127.0.0.1,1",
                "authentication": "ActiveDirectoryDefault",
            }
        )


def test_non_entra_and_custom_provider_do_not_require_sdk(
    mock_client_context, monkeypatch
):
    async def run():
        monkeypatch.setitem(sys.modules, "azure.identity.aio", None)
        monkeypatch.setitem(sys.modules, "aiohttp", None)
        raw = await connect(mock_client_context)
        await raw.close()

        class Credential:
            async def get_token(self, scope):
                return SimpleNamespace(token="custom-without-sdk")

        context = {
            key: value
            for key, value in mock_client_context.items()
            if key != "access_token"
        }
        custom = await connect(context, token_provider=Credential())
        await custom.close()

    asyncio.run(run())


@pytest.mark.parametrize(
    "mode",
    [
        "ActiveDirectoryInteractive",
        "ActiveDirectoryDeviceCodeFlow",
        "ActiveDirectoryWorkloadIdentity",
    ],
)
def test_unimplemented_builtin_modes_fail_before_network(mode):
    with pytest.raises(mssql_py_core.NotSupportedError, match="built-in AQE"):
        connect({"server": "127.0.0.1,1", "authentication": mode})


def test_sync_factory_is_not_used_for_builtin_async_mode():
    with pytest.raises(mssql_py_core.InterfaceError, match="entra_id_token_factory"):
        connect(
            {
                "server": "127.0.0.1,1",
                "authentication": "ActiveDirectoryServicePrincipal",
                "user_name": "client-id",
                "password": "secret",
                "entra_id_token_factory": lambda *args: b"sync-token",
            }
        )


@pytest.mark.parametrize("mode,class_name,options", MODES)
def test_actual_sdk_construction_and_close_without_token_network(
    builtin_context, monkeypatch, mode, class_name, options
):
    aio = pytest.importorskip("azure.identity.aio")
    pytest.importorskip("aiohttp")
    actual_class = getattr(aio, class_name)
    closed = []

    def construct(**kwargs):
        credential = actual_class(**kwargs)
        actual_close = credential.close

        async def get_token(scope):
            return SimpleNamespace(token="sdk-construction-test")

        async def close():
            await actual_close()
            closed.append(credential)

        monkeypatch.setattr(credential, "get_token", get_token)
        monkeypatch.setattr(credential, "close", close)
        return credential

    monkeypatch.setattr(aio, class_name, construct)

    async def run():
        connection = await connect(dict(builtin_context, authentication=mode, **options))
        await connection.close()
        assert len(closed) == 1

    asyncio.run(run())
