# mssql-sqlcmd

sqlcmd components in Rust. Native (ODBC) sqlcmd links this crate as a static
library and calls it through a C ABI; native sqlcmd still parses the command
line, connects and runs the batches.

## Layout

| Path | Purpose |
|---|---|
| `src/formatter/` | Output formatters. `json.rs` renders `--format json`. |
| `src/ffi.rs` | The C ABI native sqlcmd calls, including `mssql_sqlcmd_version()`. |
| `include/mssql_sqlcmd.h` | C/C++ declarations for that ABI. |

## JSON output

Native sqlcmd hands each piece of output to a JSON document instead of printing
it — the batches it sends, result sets and rows, row counts, server messages —
and renders the document once, at exit, with the connection details, timing,
status and exit code. The document follows the `format json` contract of the
sqlcmd specification, `contractVersion` 1.0:

```json
{
  "contractVersion": "1.0",
  "sqlcmd": { "version": "18.7.0001.1", "platform": "win-x64" },
  "connection": { "server": "tcp:localhost,1433", "database": "master", "authentication": "SqlPassword",
                  "encrypt": true, "serverVersion": "17.00.4085", "connectMs": 167 },
  "startTime": "2026-10-03T04:17:02.118Z",
  "durationMs": 178,
  "executionStatus": "completed",
  "operationOutcome": "succeeded",
  "exitCode": 0,
  "output": [
    { "type": "batch", "index": 1, "durationMs": 7, "text": "SELECT id, price FROM t;\nPRINT 'done';\n" },
    { "type": "resultSet",
      "columns": [ { "ordinal": 0, "name": "id", "driverType": "int", "precision": 10, "scale": 0, "nullable": false },
                   { "ordinal": 1, "name": "price", "driverType": "decimal", "precision": 10, "scale": 2, "nullable": true } ],
      "rows": [ ["1", "9.99"], ["2", null] ],
      "rowsAffected": 2 },
    { "type": "message", "number": 0, "severity": 0, "state": 1, "server": "db01", "line": 2, "sqlState": "01000", "text": "done" }
  ]
}
```

- **Header.** `contractVersion` is `major.minor`: the major version changes only
  for an incompatible change. `platform` is the runtime (`win-x64`,
  `linux-musl-arm64`, ...). `serverVersion` and `connectMs` appear once sqlcmd
  has connected. `startTime` (UTC) and `durationMs` cover the whole run.
- **Status.** `executionStatus` says whether sqlcmd carried out the run:
  `completed`, `canceled` (Ctrl+C) or `invalidInvocation` (the command line was
  rejected). `operationOutcome` is `succeeded`, `failed`, `canceled` or
  `notExecuted`: with the exit code, which is unchanged by `--format json`, it
  says whether the work succeeded. No failure category is derived; the errors
  themselves say what went wrong. A statement error that does not stop the run
  is an `error` entry in a `succeeded` document, as its exit code is 0.
- **Batches.** One `batch` entry per batch sent (`GO` separates batches), with
  its duration. Its `text` is included only with `-e`, as text mode echoes it
  only with `-e`, so a password in, say, `CREATE LOGIN` is not written out by
  default.
- **Result sets.** Columns are described as the driver describes them:
  `ordinal` (from 0) and `name` (`null` when unnamed) always; `driverType`,
  `size`, `precision`, `scale` and `nullable` when the driver exposes them, and
  type names exactly as the driver reports them. Rows are positional arrays.
  Values are strings, as sqlcmd would print them: binary with a `0x` prefix,
  `-R` and `-k` applied, and cut to the same `-y`/`-Y` width as text output;
  SQL `NULL` is JSON `null`. `rowsAffected` is the result set's "(n rows
  affected)", absent under `SET NOCOUNT ON`; a statement without a result set
  gets its own `rowsAffected` entry.
- **`FOR JSON` / `FOR XML`** output is ordinary result-set rows: one string
  per row the server returned, not parsed, combined or embedded.
- **Messages and errors** carry what the console shows for them, in its order:
  `number` (Msg), `severity` (Level), `state`, and when known `server`,
  `procedure`, `line` and `source` (the driver, e.g. `Microsoft ODBC Driver 18 for
  SQL Server`, or `Sqlcmd`, when the server did not send it). `sqlState` is the
  ODBC SQLSTATE (`28000` for a refused login, `08001` for a connection failure,
  `HYT00` for a timeout), the stable field to classify errors by. `text` is the
  message itself.

The whole document is held in memory until sqlcmd exits, because it begins
with details only known then (the exit code). Peak memory is a few times the
size of the output: the collected values, the rendered UTF-8 text, and the
UTF-16 copy handed back to sqlcmd. For very large results, text output still
streams.
## Building for native sqlcmd

```text
cargo build -p mssql-sqlcmd --release
```

produces `target/release/mssql_sqlcmd.lib` (Windows) or `libmssql_sqlcmd.a`.
The library has no dependencies beyond the Rust standard library; the system
libraries it needs at link time are listed by:

```text
cargo rustc -p mssql-sqlcmd --release --lib -- --print native-static-libs
```

## NuGet package

Native sqlcmd does not build this crate. It restores the `mssql-sqlcmd` NuGet
package, pinned in its `Directory.Packages.props`, from its own feed, which has
the Release view of `mssql-rs_Public` (`mssql-rs_Public@Release`) as an
upstream source. The package holds one static library per runtime native
sqlcmd ships on:

```text
include/mssql_sqlcmd.h
runtimes/<rid>/native/mssql_sqlcmd.lib          (win-*)
runtimes/<rid>/native/libmssql_sqlcmd.a         (linux-*, osx-*)
runtimes/<rid>/native/native-static-libs.txt
```

| Runtime | Rust target |
|---|---|
| `win-x64`, `win-x86`, `win-arm64` | `{x86_64,i686,aarch64}-pc-windows-msvc` |
| `linux-x64`, `linux-arm64` | `{x86_64,aarch64}-unknown-linux-gnu`, built on manylinux_2_28 |
| `linux-musl-x64`, `linux-musl-arm64` | `{x86_64,aarch64}-unknown-linux-musl`, dynamic C runtime |
| `osx-x64`, `osx-arm64` | `{x86_64,aarch64}-apple-darwin` |

`native-static-libs.txt` is one line: the system libraries that runtime's
library needs, exactly as rustc reports them (`-l` flags, or `.lib` names on
Windows). Consumers link these rather than a hard-coded list.

To build the same layout locally:

```text
.pipeline/scripts/build-mssql-sqlcmd-native.ps1 -OutputDirectory <dir>        # Windows
.pipeline/scripts/build-mssql-sqlcmd-native.sh <rust-target> <rid> <dir>       # Linux, macOS
.pipeline/scripts/pack-mssql-sqlcmd.ps1 -ArtifactsDirectory <dir> -StagingDirectory <staging> -RequiredRids <rids>
```

then `nuget pack <staging>/mssql-sqlcmd.nuspec`, or point native sqlcmd's
`SQLCMD_RUST_PACKAGE_DIR` at the staging directory directly.

## Pipelines and releases

Test builds (`-dev` on every merge to `main`, `-nightly` each night) are
published to the `mssql-rs_Public` feed automatically. Releases are built by the
Official mssql-sqlcmd Build when `stable` changes, published by hand with the
ADO-Release Nuget mssql-sqlcmd pipeline, and then promoted to the feed's
`Release` view, which is what msodbcsql consumes.

The full process (pipelines, versions, feed retention, the release steps,
the msodbcsql side, and troubleshooting) is in
[docs/mssql-sqlcmd-release-management.md](https://github.com/microsoft/mssql-rs/blob/main/docs/mssql-sqlcmd-release-management.md).
