# mssql-python-rs

The Rust-based native runtime for connecting to Microsoft SQL Server and Azure
SQL, shipped as prebuilt platform wheels. This distribution bundles two native
components:

- **`mssql_py_core`** — a PyO3 extension that implements the SQL Server TDS
  protocol in Rust. It backs high-throughput paths (such as bulk copy) for the
  [`mssql-python`](https://github.com/microsoft/mssql-python) driver.
- **`mssqlodbc`** — a Rust ODBC driver for SQL Server.

`mssql-python-rs` is the native runtime that
[`mssql-python`](https://github.com/microsoft/mssql-python) depends on.

## Platforms

Prebuilt CPython 3.10+ wheels for:

- **Windows** — x64, and ARM64 (CPython 3.11+)
- **macOS** — universal2 (x86_64 + arm64)
- **Linux** — glibc (`manylinux_2_28`, `manylinux_2_34`) and musl
  (`musllinux_1_2`), on x86_64 and aarch64

## License

Licensed under the [MIT License](https://github.com/microsoft/mssql-rs/blob/main/LICENSE).
