# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `mssql-odbc`: input parameter binding (`SQLBindParameter` with
  `SQL_PARAM_INPUT`) for the character and integer type families. Any other
  `ValueType` → `ParameterType` pairing is rejected at bind time with `HYC00`,
  including when the bound value is `SQL_NULL_DATA`: a SQL type that cannot
  carry a value cannot carry a typed NULL either, so an application never gets a
  binding that works for `NULL` and fails on its first real value. `ColumnSize`
  is validated against the `ParameterType` at bind time (`HY104`), matching
  msodbcsql's `CheckSqlPrecScale`.

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

- `mssql-odbc`: catalog functions — `SQLTables`, `SQLColumns`, `SQLPrimaryKeys`,
  `SQLForeignKeys`, `SQLSpecialColumns`, `SQLStatistics`, `SQLProcedures`
  (W variants). Each dispatches to the matching SQL Server system stored
  procedure (`sp_tables`, `sp_columns_100`, `sp_pkeys`, `sp_fkeys`,
  `sp_special_columns_100`, `sp_statistics_100`, `sp_stored_procedures`) via RPC,
  renames the ODBC 2.x column names those procedures emit to their ODBC 3.x
  equivalents, and clears the NOT NULL flags the specification mandates —
  matching msodbcsql. A supplied catalog scopes the call to that database via a
  three-part qualified procedure name; a nonexistent catalog yields an empty
  result set instead of an error, also matching msodbcsql.

- `mssql-py-core`: Arrow bulk copy now accepts `Utf8View` and `BinaryView`
  columns, allowing Polars DataFrames to load without first converting the
  DataFrame to a PyArrow table.

- Initial public release of the mssql-rs workspace.

### Changed

- `mssql-tds`: LOGIN7 now encodes Unicode field lengths as UTF-16 code units
  and rejects oversized records instead of producing malformed packets. This
  fixes login failures with non-ASCII usernames, passwords, database names,
  hostnames, and application names across all bindings.

- `mssql-tds`: `TdsClient::language()` now returns the language negotiated at
  login (from the server's `ENVCHANGE`) instead of always returning an empty
  string, matching its documentation.

- `mssql-tds`: `TdsClient::read_row_column` now returns `CursorColumn::Value` as a
  struct variant, `CursorColumn::Value { value, variant_base }`, where
  `variant_base` is the underlying `TdsDataType` a `sql_variant` value carried
  (`None` for non-variant columns and for NULL variants). The value cannot always
  recover the declared base type — `varchar` and `nvarchar` both decode to a
  string — so it has to come from the wire header. This replaces the
  `CursorColumn::Value(ColumnValues)` tuple variant; pattern matches need
  `CursorColumn::Value { value, .. }` and constructors need the field form.

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

- `mssql-odbc`: Entra ID credentials for service-principal and managed-identity
  authentication are now cached process-wide (keyed by tenant/authority,
  client id, and a digest of the secret, or by client id alone for managed
  identity) instead of being rebuilt for every connection. A burst of new
  connections for the same identity now triggers a single token acquisition,
  reused until near expiry, instead of one AAD/IMDS round-trip per connection —
  avoiding Managed Identity (IMDS) throttling and added login latency during
  connection-pool warm-up. A cached credential retains the secret it was built
  from for the life of the process; a rotated secret creates a new cache entry
  rather than replacing the old one.

### Removed

- `mssql-tds`: the public `connection::odbc_authentication_transformer`,
  `connection::odbc_authentication_validator`, and
  `connection::odbc_supported_auth_keywords` modules. The ODBC `Authentication=`
  keyword mapping, validation, and precedence resolution now live in each
  binding (`mssql-odbc` and `mssql-py-core`); `mssql-tds` retains only the
  `TdsAuthenticationMethod` seam and takes (or asks for) a token for the
  federated-auth flows.

### Fixed

- `mssql-odbc`: the `APP` connection-string keyword is now sent as the TDS login
  application name. It was recognized but ignored, so `APP_NAME()` reported the
  default `TDSX Rust Client` value without warning that `APP` had been dropped.

- `mssql-tds`: LOGIN7 record sizing now includes the optional change-password
  value, preventing a caller-supplied value from making the declared packet
  length shorter than the serialized payload.

- `mssql-tds`: idle connection resiliency (transparent session recovery) now
  works end to end. The client-side gate that authorizes a reconnect is now set
  from the server's `FEATUREEXTACK` acknowledgment — previously it was only ever
  set in tests, so reconnect never ran in production despite being negotiated on
  the wire. The recoverable session-state baseline the server sends in that
  acknowledgment is now parsed and replayed in the reconnect `LOGIN7`, which the
  server previously rejected as incomplete (error 17897, state 81).

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

- `mssql-odbc`: `SQL_ATTR_QUERY_TIMEOUT` is now enforced (AB#46385). Previously
  it was stored and reported back but silently ignored by `SQLExecute` and
  `SQLExecDirectW`, so a statement blocked server-side (e.g. behind another
  session's row lock) had no client-side escape hatch and could wait
  indefinitely even with a timeout configured. A non-zero value now bounds the
  wait; on expiry the driver sends `ATTENTION` and reports `HYT00`, matching
  msodbcsql. `0` (the ODBC default) remains unlimited. Fixes #439.
