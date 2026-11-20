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
- Concurrent async connection closes await one teardown outcome. Cancelling a
  close waiter does not stop cleanup. Direct close uses one five-second budget
  to settle operations, drain results, finalize transactions and close the
  transport. Explicit close rolls back active transactions even in autocommit
  mode; clean context exit commits only when autocommit is off. Cleanup failures
  are reported, without masking an existing block exception. A broken connection
  is discarded, and a clean exit requiring commit reports failure. Cursor
  finalization racing close preserves the same transaction intent. This does not
  enable connection pooling.
- Cancelling `executemany` during parameter binding leaves the connection and
  existing transaction usable. After execution claims the session, cancellation
  before result delivery breaks the connection, even if native execution has
  finished. Close rechecks this after draining and publishes its outcome
  atomically with cancellation. Cancellation before commit authorization prevents
  the request; cancellation afterward reports failure with a potentially unknown
  transaction outcome, without retrying commit.
- A broken connection still needs `await connection.close()`. The `closed`
  property indicates teardown, not whether the connection is healthy.
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
