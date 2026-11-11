<!--
Included as the mssql-sqlcmd NuGet package README by
.pipeline/scripts/pack-mssql-sqlcmd.ps1. Changes appear in newly published
packages. Keep content user-facing and links absolute for package-feed pages.
-->

# mssql-sqlcmd

Rust components linked into native (ODBC) sqlcmd through a C ABI. This package
is a static library for native integrations, not a standalone sqlcmd executable,
ODBC driver, or managed .NET API.

## Package contents

```text
include/mssql_sqlcmd.h
runtimes/<rid>/native/mssql_sqlcmd.lib   (Windows)
runtimes/<rid>/native/libmssql_sqlcmd.a  (Linux, macOS)
runtimes/<rid>/native/native-static-libs.txt
```

| Platform | Runtime identifiers |
|---|---|
| Windows | `win-x64`, `win-x86`, `win-arm64` |
| Linux glibc | `linux-x64`, `linux-arm64` (manylinux_2_28 baseline) |
| Linux musl | `linux-musl-x64`, `linux-musl-arm64` (dynamic C runtime) |
| macOS | `osx-x64`, `osx-arm64` |

Restore the NuGet package from your configured feed, include `mssql_sqlcmd.h`,
and link the archive for your runtime **plus the system libraries listed in
`native-static-libs.txt`**. Rust dependencies are compiled into the archive.

macOS consumers must target macOS 10.15 or later (11.0 or later for Apple
Silicon), matching the archive's default-enabled Network.framework dependency.
The package does not supply automatic MSBuild integration.

## JSON output

The formatter collects batches, result sets, row counts, and diagnostics into
a versioned JSON document. SQL values are strings, SQL NULL is JSON `null`,
and batch text is included only when echo is enabled (`-e`).

The whole document is buffered until exit. Peak memory includes collected
values, rendered UTF-8, and the UTF-16 copy returned to sqlcmd; use streaming
text output for very large results.

See the [JSON output contract](https://github.com/microsoft/mssql-rs/blob/main/mssql-sqlcmd/docs/json-output.md)
for the schema, status fields, and an example.

## Diagnose

`sqlcmd diagnose` checks, from the bottom up, that a connection can be made
and, when it cannot, where and why, depth by depth (`--depth`), as a text or
JSON (`--format json`) report with an exit code per outcome. Its output may
contain server names, host names and IP addresses as they are; review it before
sharing it. The checks that connect use `mssql-tds`, which is 64-bit only, so
`win-x86` builds leave diagnose out: there `mssql_sqlcmd_diagnostics_run`
returns `MSSQL_SQLCMD_UNSUPPORTED`.

See the [diagnose reference](https://github.com/microsoft/mssql-rs/blob/main/mssql-sqlcmd/docs/diagnose.md)
for the depths, the report, the exit codes and what is shown or labelled.

## Telemetry

The library can export one privacy-safe usage event per run directly to
Application Insights when `APPLICATIONINSIGHTS_CONNECTION_STRING` is set.
Missing or invalid configuration is a no-op. See the
[telemetry architecture](https://github.com/microsoft/mssql-rs/blob/main/mssql-sqlcmd/docs/telemetry.md)
for the C ABI, stable schema, privacy rules, bounded shutdown, and exact
Application Insights envelope behavior.
## Localization

The diagnose reports, text and JSON, follow the locale native sqlcmd resolved
(passed through `mssql_sqlcmd_set_locale`) or the environment, in the 10
languages sqlcmd's Windows installer ships `SQLCMD.rll` in; any other language
falls back to `en-US`. In the JSON document only sqlcmd's prose is translated:
its keys, ids, codes and statuses are the same in every locale.

See the [localization guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-sqlcmd/docs/localization.md)
for how sqlcmd's text is translated end to end: the message catalogs, how translations reach the package and native sqlcmd, how the language is chosen, and what is not localized.

## Build from source

From the repository root:

```sh
cargo build -p mssql-sqlcmd --release
cargo rustc -p mssql-sqlcmd --release --lib -- --print native-static-libs
```

The archive is written to `target/release`. For packaging and release
procedures, see the
[maintainer guide](https://github.com/microsoft/mssql-rs/blob/main/docs/mssql-sqlcmd-release-management.md).

## License

MIT
