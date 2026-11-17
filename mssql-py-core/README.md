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

- [Connection settings](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md#connection-policy-and-migration)
  for native connection options and defaults.
- [Bulk-copy behavior](https://github.com/microsoft/mssql-rs/blob/main/mssql-py-core/docs/bulk-copy.md)
  for datetime string precision, offsets, and validation.

For contributors, the
[async pool context guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-py-core/src/pool/README.md)
maps internal types and ownership; it does not describe an enabled pooling API.

## Vector bulk-copy bridge

`BulkCopyVector(values, base_type="float32")` is the typed bridge used by host
drivers to preserve vector dimensions and SQL base type through row bulk copy.
Both base types hold float32 application values; the TDS core encodes FLOAT16.
The destination dimensions and base type must match. Existing list cells infer
their base type from the destination vector column.

Arrow bulk copy accepts `fixed_size_list<float32>` for both SQL vector base
types. The list size must match destination dimensions. Optional field metadata
`mssql.vector.base_type` (`float32` or `float16`) must match the destination.
Parent NULLs are supported; NULL elements, non-finite elements and FLOAT16
magnitudes above 65504 are rejected during serialization. Sliced lists and
chunked inputs retain normal Arrow offsets. No Python row materialization or
JSON/binary fallback is used for Arrow vectors.

## License

Licensed under the [MIT License](https://github.com/microsoft/mssql-rs/blob/main/LICENSE).
