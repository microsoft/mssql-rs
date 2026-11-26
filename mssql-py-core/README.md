<!--
Published verbatim as the mssql-python-rs PyPI project description (see
`readme` in pyproject.toml); it is republished on the next release. Keep the
content user-facing and every link absolute: repo-relative links do not
resolve on PyPI.
-->

# mssql-python-rs

Native runtime for the
[`mssql-python`](https://github.com/microsoft/mssql-python) SQL Server and
Azure SQL driver. Install and use `mssql-python` for the Python database API;
this package supplies its native components:

- **`mssql_py_core`** — a PyO3 extension that implements the SQL Server TDS
  protocol in Rust. It backs high-throughput paths (such as bulk copy) for the
  [`mssql-python`](https://github.com/microsoft/mssql-python) driver.
- **`mssqlodbc`** (alpha) — an ODBC driver for SQL Server, implemented in
  Rust. It exposes the native ODBC C API. It is under active development and
  validation for use as an opt-in backend for the `mssql-python` driver. It is
  not yet a complete general-purpose replacement for Microsoft ODBC Driver 18
  for SQL Server.

## Platforms

Prebuilt wheels for CPython 3.10+:

- **Windows** — x64, and ARM64 (CPython 3.11+)
- **macOS 15+** — universal2 (x86_64 + arm64)
- **Linux** — glibc (`manylinux_2_28`, `manylinux_2_34`) and musl
  (`musllinux_1_2`), on x86_64 and aarch64

The Linux wheels require system OpenSSL: 1.1 for `manylinux_2_28`, or 3 for
`manylinux_2_34` and `musllinux_1_2` (`libssl3` on Alpine). Windows and macOS
do not require OpenSSL.

## Documentation

For SDK authors integrating the native runtime:

- Async cursors retain their logical connection when the connection wrapper is
  collected. Explicit `await connection.close()` revokes cursor access;
  `await cursor.close()` leaves the parent and sibling cursors usable. The last
  logical owner discards any remaining direct client. After parent revocation,
  cursor close releases local results even if an earlier close attempt was
  interrupted by connection shutdown.
- Native async execute, executemany, and streaming fetch/nextset awaitables
  expose an internal `_cancel_and_settle()` hook for SDK integration. Buffered
  and exhausted fetch/nextset paths and `fetchmany(size <= 0)` do not expose
  the hook; SDKs must check for it before invoking it. The hook signals
  only the originating operation and returns an awaitable that completes after
  protocol recovery. Shield that awaitable from caller cancellation before
  reusing the connection. Recovery is bounded to five seconds from the first
  cancellation; repeated waits do not restart that budget. Failed or timed-out
  recovery leaves the connection broken. Worker interruption, delivery-task
  join failures, and cancellation timeouts raise `OperationalError`.
  Cancelling the original Python future alone still returns immediately while
  recovery runs in the background. The hook adds per-operation lifetime
  bookkeeping, not per-row copies, and does not change synchronous or
  connection/authentication APIs.
  Streaming `fetchone()` repeats this bookkeeping for each row. Relative to
  the `pyo3-async-runtimes` delivery bridge, successful delivery adds one
  blocking task/GIL acquisition to release backup Python references after
  delivery. This preserves their lifetime through worker failure and recovery,
  but cleanup can contend with subsequent fetches. `fetchmany()` and
  `fetchall()` amortize the per-call cost. A
  [fetch-loop benchmark](https://github.com/microsoft/mssql-rs/blob/main/mssql-py-core/benchmarks/fetchone_delivery.py)
  uses the existing test connection settings to measure warmed native
  `fetchone()` and `fetchmany()` loops, excluding connection and execute time.
  Streaming fetches retain exclusive session ownership during Python row
  conversion while rows or later result sets remain. Cancellation can settle
  that batch without waiting for conversion. Fully exhausted batches release
  ownership before conversion, so blocked conversion does not delay reuse.
  Native completion does not end cancellation recovery: while an undelivered
  result retains a batch, its worker and operation generation remain active
  until the Python event loop accepts the result or cancellation settles it.
  A settlement hook cannot cancel later work after result delivery wins.
  After cleanup and cursor-state publication release ownership, logging does
  not delay settlement. Timeout retirement checks the originating operation
  and fetch generation atomically, so stale waits cannot break newer work.
  Settled workers may outlive their event loop without retiring a connection
  reused on another loop. Late results are not scheduled for already-cancelled
  Futures.
  Credential-provider cancellation during idle session recovery propagates
  `CancelledError` without permanently breaking the logical connection when no
  batch is open; a later query can retry recovery.
- Concurrent async connection closes await one teardown outcome. Cancelling a
  close waiter does not stop cleanup. Direct close uses one five-second budget
  to settle operations, drain results, finalize transactions and close the
  transport. Explicit close rolls back active transactions even in autocommit
  mode; clean context exit commits only when autocommit is off. Cleanup failures
  are reported, without masking an existing block exception. A broken connection
  is discarded, and a clean exit requiring commit reports failure. Cursor
  finalization racing close preserves the same transaction intent. This does not
  enable connection pooling.
  Cleanup rechecks its original deadline after waiting for transaction
  authorization; an expired budget cannot start transaction finalization.
- Cancelling `executemany` during parameter binding leaves the connection and
  existing transaction usable. After execution claims the session, await its
  cancellation settlement hook before reusing the connection. Query workers
  remain registered with the connection so concurrent close can join them within
  its cleanup budget. Close rechecks interruption after draining and publishes
  its outcome atomically with cancellation. Cancellation before commit authorization prevents
  the request; cancellation afterward reports failure with a potentially unknown
  transaction outcome, without retrying commit.
- A broken connection still needs `await connection.close()`. The `closed`
  property indicates teardown, not whether the connection is healthy.
  Cursor close reports a broken-session interruption as `InterfaceError`,
  including when the session breaks while that close waits for client access.
- Explicit `commit()` and `rollback()` also recheck interruption after draining
  and before reporting completion. Interruption before authorization prevents
  finalization; interruption during finalization reports an unknown outcome
  without retrying the transaction request.
- If creating a `commit()` or `rollback()` awaitable fails before native work
  starts, pending transactions and unread cursor results remain available.
- [Connection settings](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md#connection-policy-and-migration)
  for native connection options and defaults.
- [Bulk-copy behavior](https://github.com/microsoft/mssql-rs/blob/main/mssql-py-core/docs/bulk-copy.md)
  for datetime string precision, offsets, and validation.

For contributors, the
[async pool context guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-py-core/src/pool/README.md)
maps internal types and ownership; it does not describe an enabled pooling API.

## License

Licensed under the [MIT License](https://github.com/microsoft/mssql-rs/blob/main/LICENSE).
