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

## Bulk-copy datetime strings

Bulk copy parses ISO datetime strings targeting `DATETIME2` and `DATETIMEOFFSET`
in Rust, preserving all seven fractional-second digits (100 ns precision).
Both space and `T` separators and a trailing `Z` are supported on every supported
Python version. Fractions longer than seven digits are rejected rather than
silently truncated. The destination column's scale is applied by the TDS
serializer, using the existing truncation behavior for lower scales.
Fixed-width `YYYY-MM-DD` dates use a direct parsing path; other supported ISO
formats retain the general parser and the same precision and validation.
Python-compatible fractional forms such as `12000012` and `12:00:00:12`
are retained; both represent `12:00:00.12`.

`DATETIME2` retains the local date and time when an offset is present.
`DATETIMEOFFSET` preserves the offset and normalizes its wire timestamp to UTC;
offsets must be whole minutes within `-14:00` through `+14:00`, and both the local
and UTC dates must be within years 1 through 9999. Offset fields are normalized
before validation, matching Python (for example, `+05:60` becomes `+06:00`).
Strings without an offset
continue to use UTC for `DATETIMEOFFSET`. Python `datetime` objects retain their
existing microsecond resolution.

## Platforms

Prebuilt wheels for CPython 3.10+:

- **Windows** — x64, and ARM64 (CPython 3.11+)
- **macOS 15+** — universal2 (x86_64 + arm64)
- **Linux** — glibc (`manylinux_2_28`, `manylinux_2_34`) and musl
  (`musllinux_1_2`), on x86_64 and aarch64

The Linux wheels require system OpenSSL: 1.1 for `manylinux_2_28`, or 3 for
`manylinux_2_34` and `musllinux_1_2` (`libssl3` on Alpine). Windows and macOS
do not require OpenSSL.

## Native connection settings

For SDK authors integrating the native runtime, see the
[connection settings reference](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md#connection-policy-and-migration).

## License

Licensed under the [MIT License](https://github.com/microsoft/mssql-rs/blob/main/LICENSE).
