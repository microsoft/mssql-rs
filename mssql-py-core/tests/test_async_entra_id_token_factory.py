# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Direct async credential bridge tests, without a SQL or Entra endpoint."""

import asyncio
import contextvars
import gc
import threading
import weakref
from types import SimpleNamespace

import mssql_py_core
import pytest


invoke = mssql_py_core._invoke_async_entra_id_token_factory
SPN = "https://database.windows.net/"


@pytest.mark.parametrize(
    "spn",
    [SPN, SPN.rstrip("/"), SPN + ".default", SPN + ".default/"],
)
def test_provider_runs_on_originating_loop_and_context(spn, monkeypatch):
    async def run():
        loop = asyncio.get_running_loop()
        thread = threading.get_ident()
        identity = contextvars.ContextVar("identity")
        identity.set("caller")

        class Credential:
            def get_token(self, scope):
                assert asyncio.get_running_loop() is loop
                assert threading.get_ident() == thread
                assert identity.get() == "caller"
                assert scope == SPN + ".default"

                async def acquire():
                    assert identity.get() == "caller"
                    await asyncio.sleep(0)
                    return SimpleNamespace(token="header.\U0001f600.signature")

                return acquire()

        def unexpected_executor(*args, **kwargs):
            pytest.fail("Async token acquisition must not use an executor")

        monkeypatch.setattr(loop, "run_in_executor", unexpected_executor)
        assert await invoke(Credential(), spn) == "header.\U0001f600.signature".encode(
            "utf-16-le"
        )

    asyncio.run(run())


@pytest.mark.parametrize("spn", ["", "/", " ", SPN + "\n", "https://bad host/"])
def test_invalid_spn_never_invokes_provider(spn):
    async def run():
        class Credential:
            async def get_token(self, scope):
                pytest.fail("Invalid SPN reached provider")

        with pytest.raises(ValueError, match="SPN"):
            await invoke(Credential(), spn)

    asyncio.run(run())


@pytest.mark.parametrize(
    "result,error",
    [
        (None, TypeError),
        ("raw-token", TypeError),
        (SimpleNamespace(token=None), TypeError),
        (SimpleNamespace(token=b"bytes"), TypeError),
        (SimpleNamespace(token=123), TypeError),
        (SimpleNamespace(token=""), ValueError),
        (SimpleNamespace(token=" \t"), ValueError),
    ],
)
def test_invalid_token_results(result, error):
    async def run():
        class Credential:
            async def get_token(self, scope):
                return result

        with pytest.raises(error, match="token"):
            await invoke(Credential(), SPN)

    asyncio.run(run())


def test_nonawaitable_provider_result_is_rejected():
    async def run():
        class Credential:
            def get_token(self, scope):
                return SimpleNamespace(token="token")

        with pytest.raises(TypeError):
            await invoke(Credential(), SPN)

    asyncio.run(run())


@pytest.mark.parametrize("provider", [None, object(), SimpleNamespace(get_token=42)])
def test_invalid_provider_is_rejected(provider):
    async def run():
        with pytest.raises(mssql_py_core.InterfaceError, match="callable get_token"):
            await invoke(provider, SPN)

    asyncio.run(run())


def test_awaitable_from_another_loop_is_rejected():
    other_loop = asyncio.new_event_loop()
    try:
        async def run():
            class Credential:
                def get_token(self, scope):
                    return other_loop.create_future()

            with pytest.raises(ValueError, match="different loop"):
                await invoke(Credential(), SPN)

        asyncio.run(run())
    finally:
        other_loop.close()


def test_token_property_error_is_preserved():
    async def run():
        failure = RuntimeError("token property failed")

        class Result:
            @property
            def token(self):
                raise failure

        class Credential:
            async def get_token(self, scope):
                return Result()

        with pytest.raises(RuntimeError) as caught:
            await invoke(Credential(), SPN)
        assert caught.value is failure

    asyncio.run(run())


@pytest.mark.parametrize("during_call", [False, True])
def test_provider_exception_is_preserved(during_call):
    async def run():
        failure = RuntimeError("credential failed")

        class Credential:
            def get_token(self, scope):
                if during_call:
                    raise failure

                async def acquire():
                    raise failure

                return acquire()

        with pytest.raises(RuntimeError) as caught:
            await invoke(Credential(), SPN)
        assert caught.value is failure

    asyncio.run(run())


def test_provider_cancellation_remains_cancellation():
    async def run():
        class Credential:
            async def get_token(self, scope):
                raise asyncio.CancelledError

        with pytest.raises(asyncio.CancelledError):
            await invoke(Credential(), SPN)

    asyncio.run(run())


@pytest.mark.parametrize("fail_during_cleanup", [False, True])
def test_caller_cancellation_settles_and_observes_provider(fail_during_cleanup):
    async def run():
        loop = asyncio.get_running_loop()
        started = asyncio.Event()
        settled = asyncio.Event()
        unhandled = []
        loop.set_exception_handler(lambda _, context: unhandled.append(context))

        class Credential:
            async def get_token(self, scope):
                started.set()
                try:
                    await asyncio.Future()
                finally:
                    settled.set()
                    if fail_during_cleanup:
                        raise RuntimeError("provider cleanup failed")

            async def close(self):
                pytest.fail("Caller-owned credentials must not be closed")

        future = invoke(Credential(), SPN)
        await asyncio.wait_for(started.wait(), 2)
        assert future.cancel()
        with pytest.raises(asyncio.CancelledError):
            await future
        await asyncio.wait_for(settled.wait(), 2)
        # Completion callbacks run after the credential task has settled.
        for _ in range(5):
            await asyncio.sleep(0)
        gc.collect()
        assert not unhandled
        assert not [
            task
            for task in asyncio.all_tasks()
            if task is not asyncio.current_task()
        ]

    asyncio.run(run())


@pytest.mark.parametrize("cancel_callback_first", [False, True])
def test_cancellation_before_dispatch_does_not_invoke_provider(
    monkeypatch, cancel_callback_first
):
    async def run():
        loop = asyncio.get_running_loop()
        queued = asyncio.Event()
        cancelled = asyncio.Event()
        deferred = []
        deferred_cancellation = []
        calls = []
        schedule = loop.call_soon_threadsafe

        def intercept(callback, *args, **kwargs):
            name = getattr(callback, "__name__", "")
            owner = getattr(callback, "__self__", None)
            if type(owner).__name__ == "TokenRequest":
                if name == "start":
                    deferred.append((callback, args, kwargs))
                    return schedule(queued.set)
                if name == "cancel":
                    if not cancel_callback_first:
                        deferred_cancellation.append((callback, args))
                        return schedule(cancelled.set)

                    def cancel():
                        callback(*args)
                        cancelled.set()

                    return schedule(cancel)
            return schedule(callback, *args, **kwargs)

        monkeypatch.setattr(loop, "call_soon_threadsafe", intercept)

        class Credential:
            def get_token(self, scope):
                calls.append(scope)
                pytest.fail("Cancelled request invoked the provider")

        future = invoke(Credential(), SPN)
        await asyncio.wait_for(queued.wait(), 2)
        future.cancel()
        with pytest.raises(asyncio.CancelledError):
            await future
        await asyncio.wait_for(cancelled.wait(), 2)
        for callback, args, kwargs in deferred:
            schedule(callback, *args, **kwargs)
        await asyncio.sleep(0)
        for callback, args in deferred_cancellation:
            schedule(callback, *args)
        await asyncio.sleep(0)
        assert not calls
        assert not [
            task
            for task in asyncio.all_tasks()
            if task is not asyncio.current_task()
        ]

    asyncio.run(run())


def test_cancelled_acquisition_is_retained_until_a_late_result_is_observed():
    async def run():
        started = asyncio.Event()
        cancelling = asyncio.Event()
        finished = asyncio.Event()
        pending = []
        tasks = []
        unhandled = []
        asyncio.get_running_loop().set_exception_handler(
            lambda _, context: unhandled.append(context)
        )

        class Credential:
            async def get_token(self, scope):
                tasks.append(weakref.ref(asyncio.current_task()))
                started.set()
                try:
                    await asyncio.Future()
                except asyncio.CancelledError:
                    future = asyncio.get_running_loop().create_future()
                    pending.append(weakref.ref(future))
                    cancelling.set()
                    await future
                    raise RuntimeError("late-provider-diagnostic")
                finally:
                    finished.set()

            async def close(self):
                pytest.fail("Caller-owned credential was closed")

        future = invoke(Credential(), SPN)
        await asyncio.wait_for(started.wait(), 2)
        future.cancel()
        with pytest.raises(asyncio.CancelledError):
            await future
        await asyncio.wait_for(cancelling.wait(), 2)
        gc.collect()
        try:
            assert tasks[0]() is not None
            assert pending[0]() is not None
        finally:
            if pending[0]() is not None:
                pending[0]().set_result(None)
        await asyncio.wait_for(finished.wait(), 2)
        for _ in range(5):
            await asyncio.sleep(0)
        gc.collect()
        assert tasks[0]() is None
        assert not unhandled

    asyncio.run(run())


def test_separate_event_loops_do_not_share_dispatch_state():
    async def run():
        loop = asyncio.get_running_loop()

        class Credential:
            async def get_token(self, scope):
                assert asyncio.get_running_loop() is loop
                return SimpleNamespace(token="token")

        assert await invoke(Credential(), SPN) == b"t\x00o\x00k\x00e\x00n\x00"

    asyncio.run(run())
    asyncio.run(run())
