# GitHub issue [microsoft/mssql-rs#38](https://github.com/microsoft/mssql-rs/issues/38): sp_prepare fails when statement uses a non-int named parameter

## Summary

`TdsClient::execute_sp_prepare` was forwarding the user's named parameter
values as RPC parameters on the `sp_prepare` call. `sp_prepare`'s signature
is fixed (`@handle int OUTPUT, @params ntext, @stmt ntext, @options int`)
and accepts no caller-supplied values; those belong on `sp_execute`,
`sp_prepexec`, or `sp_executesql`. When the extra named arguments did not
coincidentally fit the types `sp_prepare` expects, the server returned

  Msg 214: Procedure expects parameter '@options' of type 'int'.

`drain_stream()` collected the ERROR token but the call site dropped the
returned `Vec<SqlErrorInfo>`. The post-drain shape check then saw
`ColumnValues::Null` for the `@handle` output and raised the generic
`ProtocolError("Expected an integer value")` reported in the issue.

## Root cause

In `mssql-tds/src/connection/tds_client.rs`, `execute_sp_prepare` built the
RPC as:

```rust
let rpc = SqlRpc::new(
    RpcType::ProcId(RpcProcs::Prepare),
    positional_parameters,
    Some(named_params),    // <-- bug: user values smuggled onto sp_prepare
    &database_collation,
    &self.execution_context,
);
```

`SqlRpc::write_named_parameters` (`mssql-tds/src/message/rpc.rs`) iterates
the named slice and serializes every entry on the wire. Because the wire
contract for `sp_prepare` does not include user values, the server tried to
bind those as `@options` and rejected anything that wasn't an int.

The same call site also discarded `drain_stream`'s returned errors:

```rust
self.drain_stream().await?;     // returned Vec<SqlErrorInfo> dropped
// ... shape check then raised a generic ProtocolError
```

This collapsed the real diagnostic into an unrelated message and is the
reason the issue was originally misread as a parameter-declaration bug.

## Fix

`mssql-tds/src/connection/tds_client.rs::execute_sp_prepare`:

- Pass `None` (not `Some(named_params)`) as the RPC named-parameter list.
  `named_params` is still consumed locally to build the `@params` text
  string serialized as the second positional argument; only the wire-level
  RPC named arguments are dropped.
- Capture the `Vec<SqlErrorInfo>` returned by `drain_stream()` and, when
  non-empty, return `Error::SqlServerError { errors }` so callers see the
  actual server diagnostic.

The other RPC paths (`execute_sp_executesql`, `execute_sp_prepexec`) are
correct as-is: those stored procedures' contracts include the user's
parameter values after the fixed positional arguments, and the server
expects them on the wire.

## Tests

Added in `mssql-tds/tests/test_rpc_results.rs` (the file already contains
the other `sp_prepare` / `sp_prepexec` integration tests):

- `test_sp_prepare_with_named_nvarchar_param_succeeds`: regression test
  for [microsoft/mssql-rs#38](https://github.com/microsoft/mssql-rs/issues/38);
  prepares a statement that references
  `@db_name nvarchar(6)` and asserts a positive handle is returned, then
  unprepares it.
- `test_sp_prepare_surfaces_server_error_on_invalid_sql`: verifies the
  diagnostics fix; preparing an undeclared-variable statement returns
  `Error::SqlServerError` carrying the populated server message instead
  of a generic `ProtocolError`.

Both are integration tests; like the rest of the file they require the
DB environment variables (`DB_HOST`, `DB_USERNAME`, `SQL_PASSWORD`).

## Why the existing tests didn't catch this

- The integration tests in `mssql-tds/tests/test_rpc_results.rs` for
  `sp_prepare` (`test_sp_prepare_and_unprepare_multi_param`) only use
  `Int` user parameters. SQL Server tolerates extra named values on
  `sp_prepare` when their types happen to match what the optional
  `@options` parameter accepts (int), so all-int tests pass against a
  live server even though the code is wrong.
- No existing test exercises `sp_prepare` with `NVarchar`, `NChar`,
  `Decimal`, `DateTime2`, `Uuid`, `Vector`, or any other non-int user
  parameter type.
- No negative test asserts what happens when `sp_prepare` fails. With
  the drained errors discarded, even a CI run that happened to surface
  the bug would have reported the misleading
  `ProtocolError("Expected an integer value")`.
- No mock-server test exercises the wire-level RPC structure.
  `mssql-mock-tds` exists but is not used to assert that `sp_prepare` is
  sent without user named parameters or that ERROR tokens propagate as
  `SqlServerError`.
- The integration tests panic when `DB_HOST`/`DB_USERNAME`/`SQL_PASSWORD`
  are not set (`mssql-tds/tests/common/mod.rs`). They cannot run as
  unit-only checks, so the suite often runs in environments where the
  prepare paths simply do not execute.

## Test-coverage follow-ups

These are scoped to the `sp_prepare` regression and would have caught
this specific class of bug. They are not part of this fix.

- Add per-type integration tests for `sp_prepare`, `sp_prepexec`, and
  `sp_executesql` that cover at least: `Char`, `NChar`, `Varchar`,
  `NVarchar`, `VarcharMax`, `NVarcharMax`, `Decimal` with explicit
  precision and scale, `DateTime2`, `DateTimeOffset`, `Time`, `Uuid`,
  `VarBinary`, `Vector`. This would have caught the
  [microsoft/mssql-rs#38](https://github.com/microsoft/mssql-rs/issues/38)
  regression on the first non-int type added.
- Add a mock-TDS test that injects an ERROR token into the response
  stream for an `sp_prepare` RPC and asserts the call surfaces it as
  `SqlServerError`. This covers the diagnostics path without a live
  server.
- Make the integration helpers in `mssql-tds/tests/common/mod.rs` skip
  cleanly (instead of panicking) when the database environment variables
  are absent, so CI without a SQL Server can still run the unit-style
  tests.
