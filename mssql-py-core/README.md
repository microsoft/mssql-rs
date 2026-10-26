<!--
Published verbatim as the mssql-python-rs PyPI project description (see
`readme` in pyproject.toml); it is republished on the next release. Keep the
content user-facing and every link absolute: repo-relative links do not
resolve on PyPI.
-->

# mssql-python-rs

The Rust-based native runtime for connecting to Microsoft SQL Server and Azure
SQL, shipped as prebuilt platform wheels. This distribution bundles two native
components:

- **`mssql_py_core`** — a PyO3 extension that implements the SQL Server TDS
  protocol in Rust. It backs high-throughput paths (such as bulk copy) for the
  [`mssql-python`](https://github.com/microsoft/mssql-python) driver.
- **`mssqlodbc`** (alpha) — an ODBC driver for SQL Server, implemented in
  Rust. It exposes the native ODBC C API. It is under active development and
  validation for use as an opt-in backend for the `mssql-python` driver. It is
  not yet a complete general-purpose replacement for Microsoft ODBC Driver 18
  for SQL Server.

`mssql-python-rs` is the native runtime that
[`mssql-python`](https://github.com/microsoft/mssql-python) depends on.

## Native connection options

Eligible TCP connections default to MultiSubnetFailover (MSF), racing resolved
addresses without a first-address delay. For `PyCoreConnection` and
`PyAsyncConnection.connect`, pass `multi_subnet_failover=False` in the native
context dictionary to opt out, or `True` to opt in explicitly. Case-insensitive
`"Yes"`/`"No"` strings are also accepted. Omit the key or pass `None` to inherit
startup defaults; `False` remains an explicit opt-out. Other types and strings
are errors.

Before opening any connections or pools, applications can call
`mssql_py_core.set_default_multi_subnet_failover(False)`. Explicit connection
settings outrank this default, which outranks the
`MSSQL_RS_MULTISUBNET_FAILOVER_DEFAULT=true|false` environment variable, then the
built-in enabled default. Defaults freeze on first use; late setters fail.
The Python setter affects the py-core module, not a separately loaded ODBC
driver. Use the environment before process startup to configure both.

Transparent Network IP Resolution (TNIR) is not implemented.
`transparent_network_ip_resolution=False` or `"Disabled"` is accepted as a
compatibility no-op, **not an MSF opt-out**. `True` or `"Enabled"` raises an
unsupported-feature exception; malformed values are rejected.

These are native dictionary options, not an ODBC connection-string parser.
Higher-level clients must forward them to py-core. The existing `mssql-python`
MSF mapper preserves omitted and explicit `"No"` values in async and bulk-copy
contexts; its TNIR keyword forwarding requires a separate consumer update.
For ODBC connections, use `MultiSubnetFailover=No` or the corresponding
connection attribute. See the
[connection policy](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/README.md#connection-policy-and-migration)
for applicability, address limits, and recovery behavior.

## Platforms

Prebuilt CPython 3.10+ wheels for:

- **Windows** — x64, and ARM64 (CPython 3.11+)
- **macOS** — universal2 (x86_64 + arm64)
- **Linux** — glibc (`manylinux_2_28`, `manylinux_2_34`) and musl
  (`musllinux_1_2`), on x86_64 and aarch64

## License

Licensed under the [MIT License](https://github.com/microsoft/mssql-rs/blob/main/LICENSE).
