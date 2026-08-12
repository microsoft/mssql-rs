# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `mssql-tds`: `SqlType::Variant` for passing `sql_variant` values as RPC / `sp_executesql`
  parameters.

- `mssql-tds`: server INFO/warning messages are now captured and retrievable.
  New public `SqlInfoMessage` and `SqlServerDiagnostics` types in
  `mssql_tds::error`, a new `Error::from_sql_diagnostics` constructor, and
  `TdsClient::info_messages()` / `TdsClient::take_info_messages()`. INFO tokens
  from batch, RPC, result-set draining, login, and bulk copy are accumulated
  instead of discarded.

- `mssql-odbc`: server informational/warning messages are surfaced as
  diagnostic records (`SQLGetDiagRec` / `SQLGetDiagField`), and successful calls
  that observed them return `SQL_SUCCESS_WITH_INFO`
  (`SQLDriverConnect`, `SQLExecDirect`, `SQLFetch`, `SQLMoreResults`,
  `SQLCloseCursor` / `SQLFreeStmt(SQL_CLOSE)`). INFO captured at end-of-rowset is
  deferred to the next result-set boundary (`SQLMoreResults` advance or cursor
  close) so it surfaces with a `SQL_SUCCESS_WITH_INFO` hint instead of being
  posted under `SQL_NO_DATA`, which many applications never inspect.

- Initial public release of the mssql-rs workspace.

### Changed

- `mssql-tds`: `Error::SqlServerError` now carries a `SqlServerDiagnostics`
  (`{ diagnostics }`) grouping server errors *and* informational messages,
  replacing the previous `{ errors: Vec<SqlErrorInfo> }` shape. The
  `Error::from_sql_error` / `Error::from_sql_errors` constructors are unchanged.
- `mssql-tds`: a failed login now surfaces **all** server ERROR tokens (not just
  the last) plus any INFO messages via `Error::SqlServerError { diagnostics }`.
- `mssql-tds`: `TdsClient::info_messages()` reflects only the current command;
  each `execute*` call resets the informational-message buffer at entry.
- `mssql-tds`: bulk copy (`BulkCopy::write_to_server_zerocopy`) resets the
  informational-message buffer at entry and accumulates INFO across all
  bulk-load batches, so messages emitted during the load (e.g. from triggers
  fired via `fire_triggers`) remain retrievable via `info_messages()` after the
  operation completes. On a mid-stream failure the completed batches' INFO is
  preserved and remains retrievable alongside the returned error.
- `mssql-tds`: `BulkCopyResult::rows_affected` now reports the number of rows the
  client serialized to the wire (matching `SqlBulkCopy.RowsCopied`) instead of the
  server's `DONE_COUNT`. Fixes a doubled count on distributed engines that
  acknowledge one load with multiple `DONE_COUNT` tokens (issue #209).

### Removed

- `mssql-tds`: the public `connection::odbc_authentication_transformer`,
  `connection::odbc_authentication_validator`, and
  `connection::odbc_supported_auth_keywords` modules. The ODBC `Authentication=`
  keyword mapping, validation, and precedence resolution now live in each
  binding (`mssql-odbc` and `mssql-py-core`); `mssql-tds` retains only the
  `TdsAuthenticationMethod` seam and takes (or asks for) a token for the
  federated-auth flows.

### Fixed

- `mssql-tds`: reading a fixed-width value that straddles a TDS packet boundary
  could return bytes from the wrong place or panic. The readers checked for
  sufficient buffered data with an `if` and read a single further packet, but a
  value can span more than two packets (and a packet can carry fewer bytes than
  the value needs), so the read proceeded against a still-short buffer. The
  check is now a loop that reads until the whole value is buffered. Affects all
  13 fixed-width readers on `TdsPacketReader`.

- `mssql-tds`: `read_varchar_u8_length` truncated strings of 128 characters or
  more, and `read_varchar_u16_length` strings of 32768 or more. The character
  count was doubled to a byte count *before* being widened to `usize`
  (`(length << 1) as usize`), so the shift overflowed the narrow type and
  silently wrapped — a 200-character string asked for 144 bytes. The widening
  now happens first (`(length as usize) << 1`).

- `mssql-tds`: a payload-free TDS packet without the end-of-message flag is now
  rejected as a protocol error. Such a packet is malformed — it neither carries
  payload nor terminates a message — but was previously consumed as a
  zero-length packet. Empty end-of-message packets remain legal.

