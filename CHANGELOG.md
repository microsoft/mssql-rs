# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `mssql-tds`: `SqlType::Variant` for passing `sql_variant` values as RPC / `sp_executesql`
  parameters.

- Initial public release of the mssql-rs workspace.

### Removed

- `mssql-tds`: the public `connection::odbc_authentication_transformer`,
  `connection::odbc_authentication_validator`, and
  `connection::odbc_supported_auth_keywords` modules. The ODBC `Authentication=`
  keyword mapping, validation, and precedence resolution now live in each
  binding (`mssql-odbc` and `mssql-py-core`); `mssql-tds` retains only the
  `TdsAuthenticationMethod` seam and takes (or asks for) a token for the
  federated-auth flows.
