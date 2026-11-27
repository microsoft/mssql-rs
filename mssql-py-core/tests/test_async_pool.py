# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Internal SQL-password pool boundary and live-session contracts."""

import asyncio
import gc
import inspect

import pytest
import mssql_py_core as core


def pool(**overrides):
    settings = dict(
        max_size=1, acquire_timeout=5, cleanup_timeout=5, shutdown_timeout=5
    )
    settings.update(overrides)
    return core._AsyncPool(**settings)


def test_native_connect_positional_slots_are_unchanged():
    parameters = inspect.signature(core.PyAsyncConnection.connect).parameters
    assert list(parameters)[:3] == [
        "client_context_dict", "python_logger", "autocommit"
    ]
    for name in ("pool", "acquire_timeout", "isolation_level"):
        assert parameters[name].kind is inspect.Parameter.KEYWORD_ONLY


def test_pool_requires_a_running_loop():
    with pytest.raises(core.InterfaceError, match="running event loop"):
        pool()


@pytest.mark.parametrize("field", [
    "acquire_timeout", "cleanup_timeout", "shutdown_timeout",
    "max_size", "max_waiting",
])
def test_pool_rejects_zero_budgets_and_limits(field):
    async def run():
        with pytest.raises(core.InterfaceError):
            pool(**{field: 0})
    asyncio.run(run())


@pytest.mark.parametrize("field", [
    "acquire_timeout", "cleanup_timeout", "shutdown_timeout",
    "max_size", "max_waiting", "idle_timeout",
])
@pytest.mark.parametrize("value", [True, -1, float("inf"), float("nan"), "1"])
def test_invalid_pool_configuration_fails_before_io(field, value):
    async def run():
        with pytest.raises(core.InterfaceError):
            pool(**{field: value})
    asyncio.run(run())


def test_pool_requires_owner_loop_and_terminal_close_cannot_reopen():
    async def create():
        return pool()

    manager = asyncio.run(create())

    async def wrong_loop():
        for operation in (manager.enable, manager.disable, manager.aclose):
            with pytest.raises(core.InterfaceError, match="owning event loop"):
                operation()
    asyncio.run(wrong_loop())

    async def lifecycle():
        manager = pool(idle_timeout=0, max_waiting=None)
        manager.disable()
        manager.enable()
        await manager.aclose()
        await manager.aclose()
        with pytest.raises(core.InterfaceError) as error:
            manager.enable()
        assert error.value.pool_reason == "Closed"
    asyncio.run(lifecycle())


@pytest.mark.parametrize("disabled", [False, True])
@pytest.mark.parametrize("option", [
    {"authentication": "ActiveDirectoryMSI"},
    {"trusted_connection": "Yes"},
    {"access_token": "not-a-token"},
    {"column_encryption": "Enabled"},
    {"column_encryption_setting": "Enabled"},
    {"mars_enabled": True},
    {"connect_retry_count": 1},
    {"packet_size": 1},
    {"packet_size": 32769},
    {"unknown": "secret-must-not-appear"},
])
def test_pool_rejects_unsupported_options_without_io(option, disabled):
    async def run():
        async with pool() as manager:
            if disabled:
                manager.disable()
            with pytest.raises(core.InterfaceError) as error:
                await core.PyAsyncConnection.connect(
                    {"server": "127.0.0.1,1", **option}, pool=manager
                )
            assert "secret-must-not-appear" not in str(error.value)
    asyncio.run(run())


@pytest.mark.parametrize("disabled", [False, True])
def test_pool_rejects_token_provider_before_invoking_it(disabled):
    class Provider:
        async def get_token(self, scope):
            pytest.fail("Unsupported pooled authentication invoked the provider")

    async def run():
        async with pool() as manager:
            if disabled:
                manager.disable()
            with pytest.raises(core.InterfaceError, match="SQL password"):
                await core.PyAsyncConnection.connect(
                    {"server": "127.0.0.1,1"}, pool=manager,
                    token_provider=Provider(),
                )
    asyncio.run(run())


@pytest.mark.parametrize("value", [True, 0, -1, float("inf"), float("nan"), "1"])
def test_invalid_acquire_override_is_rejected_on_direct_and_pooled_paths(value):
    async def run():
        async with pool() as manager:
            for selected in (None, manager):
                with pytest.raises(core.InterfaceError):
                    await core.PyAsyncConnection.connect(
                        {"server": "127.0.0.1,1"}, pool=selected,
                        acquire_timeout=value,
                    )
    asyncio.run(run())


@pytest.mark.parametrize("level", ["", "NO CHANGE", "READ COMMITTED; SELECT 1"])
def test_isolation_is_a_closed_set(level):
    async def run():
        with pytest.raises(core.InterfaceError, match="isolation_level"):
            await core.PyAsyncConnection.connect(
                {"server": "127.0.0.1,1"}, isolation_level=level
            )
    asyncio.run(run())


@pytest.mark.parametrize("cancel", [False, True])
@pytest.mark.parametrize("pooled", [False, True])
def test_connect_timeout_and_cancellation_destroy_partial_io(cancel, pooled):
    async def run():
        accepted = asyncio.Event()
        disconnected = asyncio.Event()

        async def hold_prelogin(reader, writer):
            accepted.set()
            try:
                await reader.read()
            finally:
                writer.close()
                await writer.wait_closed()
                disconnected.set()

        server = await asyncio.start_server(hold_prelogin, "127.0.0.1", 0)
        manager = pool() if pooled else None
        try:
            port = server.sockets[0].getsockname()[1]
            future = core.PyAsyncConnection.connect(
                {
                    "server": f"127.0.0.1,{port}", "connect_retry_count": 0,
                    "user_name": "test-user", "password": "test-password",
                },
                pool=manager,
                acquire_timeout=5 if cancel else 0.1,
            )
            await asyncio.wait_for(accepted.wait(), 2)
            if cancel:
                future.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await future
            else:
                with pytest.raises(core.OperationalError) as error:
                    await future
                assert error.value.pool_reason == "AcquireTimeout"
            await asyncio.wait_for(disconnected.wait(), 2)
        finally:
            server.close()
            await server.wait_closed()
            if manager is not None:
                await manager.aclose()
    asyncio.run(run())


@pytest.fixture
def sql_context(client_context):
    if "access_token" in client_context:
        pytest.skip("This milestone supports SQL password pooling only")
    context = dict(client_context)
    context["trust_server_certificate"] = (
        "Yes" if context["trust_server_certificate"] else "No"
    )
    return context


async def scalar(connection, sql):
    cursor = connection.cursor()
    try:
        await cursor.execute(sql, use_prepare=False)
        row = await cursor.fetchone()
        return row[0]
    finally:
        await cursor.close()


@pytest.mark.integration
def test_live_reuse_reset_isolation_and_stale_cursor(sql_context):
    async def run():
        async with pool() as manager:
            first = await core.PyAsyncConnection.connect(
                sql_context, None, True, pool=manager, isolation_level="SERIALIZABLE"
            )
            spid = await scalar(first, "SELECT @@SPID")
            stale = first.cursor()
            await stale.execute(
                "SET LOCK_TIMEOUT 1234; CREATE TABLE #pool_boundary(n int); SELECT 1",
                use_prepare=False,
            )
            await first.close()
            second = await core.PyAsyncConnection.connect(
                sql_context, pool=manager, autocommit=True
            )
            try:
                assert await scalar(second, "SELECT @@SPID") == spid
                assert await scalar(second, "SELECT @@LOCK_TIMEOUT") == -1
                assert await scalar(
                    second, "SELECT OBJECT_ID('tempdb..#pool_boundary')"
                ) is None
                assert await scalar(
                    second,
                    "SELECT CAST(transaction_isolation_level AS int) "
                    "FROM sys.dm_exec_sessions WHERE session_id=@@SPID",
                ) == 2
                with pytest.raises(core.InterfaceError):
                    await stale.execute("SELECT 999")
                await stale.close()
                del stale
                gc.collect()
                assert await scalar(second, "SELECT @@SPID") == spid
            finally:
                await second.close()
    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("exit_mode, expected", [
    ("close", 0), ("clean", 1), ("exception", 0), ("raw-autocommit", 0),
])
def test_live_close_preserves_transaction_intent(sql_context, exit_mode, expected):
    from uuid import uuid4

    async def run():
        table = "pool_boundary_" + uuid4().hex
        setup = await core.PyAsyncConnection.connect(sql_context, autocommit=True)
        try:
            cursor = setup.cursor()
            await cursor.execute(f"CREATE TABLE {table}(n int)", use_prepare=False)
            await cursor.close()
            async with pool() as manager:
                connection = await core.PyAsyncConnection.connect(
                    sql_context, pool=manager, autocommit=exit_mode == "raw-autocommit"
                )
                spid = await scalar(connection, "SELECT @@SPID")
                cursor = connection.cursor()
                prefix = "BEGIN TRANSACTION; " if exit_mode == "raw-autocommit" else ""
                await cursor.execute(
                    prefix + f"INSERT INTO {table} VALUES (1); SELECT 7",
                    use_prepare=False,
                )
                if exit_mode == "clean":
                    await connection.__aexit__(None, None, None)
                elif exit_mode == "exception":
                    error = ValueError("primary")
                    await connection.__aexit__(ValueError, error, None)
                else:
                    await connection.close()
                next_connection = await core.PyAsyncConnection.connect(
                    sql_context, pool=manager, autocommit=True
                )
                try:
                    assert await scalar(next_connection, "SELECT @@SPID") == spid
                    assert await scalar(next_connection, "SELECT @@TRANCOUNT") == 0
                    assert await scalar(
                        next_connection, f"SELECT COUNT(*) FROM {table}"
                    ) == expected
                finally:
                    await next_connection.close()
        finally:
            cursor = setup.cursor()
            await cursor.execute(f"DROP TABLE IF EXISTS {table}", use_prepare=False)
            await cursor.close()
            await setup.close()
    asyncio.run(run())


@pytest.mark.integration
def test_live_queue_timeout_cancellation_and_no_bypass(sql_context):
    async def run():
        async with pool(max_waiting=1) as manager:
            first = await core.PyAsyncConnection.connect(sql_context, pool=manager)
            try:
                with pytest.raises(core.OperationalError) as error:
                    await core.PyAsyncConnection.connect(
                        sql_context, pool=manager, acquire_timeout=0.01
                    )
                assert error.value.pool_reason == "AcquireTimeout"
                attempts = [
                    core.PyAsyncConnection.connect(sql_context, pool=manager)
                    for _ in range(2)
                ]
                done, waiting = await asyncio.wait(
                    attempts, timeout=2, return_when=asyncio.FIRST_COMPLETED
                )
                assert len(done) == len(waiting) == 1
                with pytest.raises(core.OperationalError) as full:
                    await done.pop()
                assert full.value.pool_reason == "WaitQueueFull"
                pending = waiting.pop()
                pending.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await pending
            finally:
                await first.close()
            replacement = await core.PyAsyncConnection.connect(sql_context, pool=manager)
            await replacement.close()
    asyncio.run(run())


@pytest.mark.integration
@pytest.mark.parametrize("level, expected", [
    ("READ UNCOMMITTED", 1), ("READ COMMITTED", 2), ("REPEATABLE READ", 3),
    ("SERIALIZABLE", 4), ("SNAPSHOT", 5),
])
def test_live_fresh_and_reused_requested_isolation(sql_context, level, expected):
    async def run():
        async with pool() as manager:
            previous_spid = None
            for _ in range(2):
                connection = await core.PyAsyncConnection.connect(
                    sql_context, pool=manager, autocommit=True, isolation_level=level
                )
                try:
                    assert connection.timeout == 0
                    connection.timeout = 9
                    spid = await scalar(connection, "SELECT @@SPID")
                    if previous_spid is not None:
                        assert spid == previous_spid
                    previous_spid = spid
                    assert await scalar(
                        connection,
                        "SELECT CAST(transaction_isolation_level AS int) "
                        "FROM sys.dm_exec_sessions WHERE session_id=@@SPID",
                    ) == expected
                finally:
                    await connection.close()
    asyncio.run(run())


@pytest.mark.integration
def test_live_disabled_admission_is_direct_and_survives_pool_shutdown(sql_context):
    async def run():
        manager = pool()
        manager.disable()
        connection = await core.PyAsyncConnection.connect(
            sql_context, pool=manager, autocommit=True
        )
        try:
            await manager.aclose()
            assert await scalar(connection, "SELECT 42") == 42
        finally:
            await connection.close()
    asyncio.run(run())


@pytest.mark.integration
def test_live_retained_cursor_and_final_owner_discard_restore_capacity(sql_context):
    async def run():
        async with pool() as manager:
            connection = await core.PyAsyncConnection.connect(
                sql_context, pool=manager, autocommit=True
            )
            cursor = connection.cursor()
            sibling = connection.cursor()
            del connection
            gc.collect()
            await cursor.execute("SELECT 17", use_prepare=False)
            assert (await cursor.fetchone())[0] == 17
            await cursor.close()
            await sibling.execute("SELECT 19", use_prepare=False)
            assert (await sibling.fetchone())[0] == 19
            await sibling.close()
            del cursor, sibling
            gc.collect()
            replacement = await core.PyAsyncConnection.connect(sql_context, pool=manager)
            await replacement.close()
    asyncio.run(run())


@pytest.mark.integration
def test_live_cancelled_close_waiter_still_returns_one_reusable_lease(sql_context):
    async def run():
        async with pool() as manager:
            connection = await core.PyAsyncConnection.connect(
                sql_context, pool=manager, autocommit=True
            )
            spid = await scalar(connection, "SELECT @@SPID")
            cursor = connection.cursor()
            await cursor.execute("SELECT 1; SELECT 2", use_prepare=False)
            first = connection.close()
            second = connection.close()
            assert first.cancel()
            with pytest.raises(asyncio.CancelledError):
                await first
            await second
            await connection.close()
            replacement = await core.PyAsyncConnection.connect(
                sql_context, pool=manager, autocommit=True
            )
            try:
                assert await scalar(replacement, "SELECT @@SPID") == spid
                await cursor.close()
                assert await scalar(replacement, "SELECT 29") == 29
            finally:
                await replacement.close()
    asyncio.run(run())


@pytest.mark.integration
def test_live_shutdown_timeout_keeps_borrower_usable_and_can_be_awaited_again(sql_context):
    async def run():
        manager = pool(shutdown_timeout=0.05)
        connection = await core.PyAsyncConnection.connect(
            sql_context, pool=manager, autocommit=True
        )
        try:
            with pytest.raises(core.OperationalError) as error:
                await manager.aclose()
            assert error.value.pool_reason == "ShutdownTimeout"
            assert await scalar(connection, "SELECT 31") == 31
            with pytest.raises(core.InterfaceError) as closed:
                await core.PyAsyncConnection.connect(sql_context, pool=manager)
            assert closed.value.pool_reason == "Closed"
        finally:
            await connection.close()
            await manager.aclose()
    asyncio.run(run())


@pytest.mark.integration
def test_live_shutdown_rejects_pending_connect_without_cancelling_the_caller(sql_context):
    async def run():
        manager = pool(max_waiting=1)
        first = await core.PyAsyncConnection.connect(sql_context, pool=manager)
        attempts = [
            core.PyAsyncConnection.connect(sql_context, pool=manager)
            for _ in range(2)
        ]
        try:
            done, waiting = await asyncio.wait(
                attempts, timeout=2, return_when=asyncio.FIRST_COMPLETED
            )
            assert len(done) == len(waiting) == 1
            with pytest.raises(core.OperationalError) as full:
                await done.pop()
            assert full.value.pool_reason == "WaitQueueFull"
            closing = manager.aclose()
            with pytest.raises(core.InterfaceError) as closed:
                await waiting.pop()
            assert closed.value.pool_reason == "Closed"
            await first.close()
            await closing
        finally:
            await first.close()
            await manager.aclose()
    asyncio.run(run())


@pytest.mark.integration
def test_live_pool_exit_preserves_block_exception_when_shutdown_times_out(sql_context):
    async def run():
        manager = pool(shutdown_timeout=0.05)
        connection = None
        try:
            with pytest.raises(ValueError, match="primary") as error:
                async with manager:
                    connection = await core.PyAsyncConnection.connect(sql_context, pool=manager)
                    raise ValueError("primary")
            if hasattr(error.value, "__notes__"):
                assert "Pool shutdown failed" in error.value.__notes__[0]
        finally:
            if connection is not None:
                await connection.close()
            await manager.aclose()
    asyncio.run(run())
