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
  from batch, RPC, result-set draining, and login are accumulated instead of
  discarded.

- `mssql-odbc`: server informational/warning messages are surfaced as
  diagnostic records (`SQLGetDiagRec` / `SQLGetDiagField`), and successful calls
  that observed them return `SQL_SUCCESS_WITH_INFO`
  (`SQLDriverConnect`, `SQLExecDirect`, `SQLFetch`, `SQLMoreResults`,
  `SQLCloseCursor` / `SQLFreeStmt(SQL_CLOSE)`).

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

### Removed

- `mssql-tds`: the public `connection::odbc_authentication_transformer`,
  `connection::odbc_authentication_validator`, and
  `connection::odbc_supported_auth_keywords` modules. The ODBC `Authentication=`
  keyword mapping, validation, and precedence resolution now live in each
  binding (`mssql-odbc` and `mssql-py-core`); `mssql-tds` retains only the
  `TdsAuthenticationMethod` seam and takes (or asks for) a token for the
  federated-auth flows.
