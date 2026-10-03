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
status and exit code:

```json
{
  "formatVersion": 1,
  "sqlcmd": { "version": "18.7.0001.1", "platform": "win-x64" },
  "connection": { "server": "tcp:localhost,1433", "database": null, "authentication": "SqlPassword",
                  "encrypt": true, "serverVersion": "17.00.4085", "connectMs": 167 },
  "startTime": "2026-10-03T04:17:02.118Z",
  "durationMs": 178,
  "status": "success",
  "exitCode": 0,
  "output": [
    { "type": "batch", "index": 1, "durationMs": 7, "text": "SELECT id, price FROM t;\nPRINT 'done';\n" },
    { "type": "resultSet",
      "columns": [ { "name": "id", "type": "int" }, { "name": "price", "type": "decimal(10,2)" } ],
      "rows": [ ["1", "9.99"], ["2", null] ],
      "rowsAffected": 2 },
    { "type": "message", "number": 0, "state": 1, "severity": 0, "line": 2, "message": "done" },
    { "type": "batch", "index": 2, "durationMs": 1, "text": "UPDATE t SET price = 1;\nRAISERROR('boom', 16, 1);\n" },
    { "type": "rowsAffected", "count": 1 },
    { "type": "error", "number": 50000, "state": 1, "severity": 16, "line": 2, "message": "boom" }
  ]
}
```

- **Header.** `formatVersion` changes only for a change that could break a
  reader. `platform` is the runtime (`win-x64`, `linux-musl-arm64`, ...).
  `serverVersion` and `connectMs` appear once sqlcmd has connected.
  `startTime` (UTC) and `durationMs` cover the whole run.
- **Status.** `status` is `failed` when sqlcmd exits non-zero or was
  cancelled, with `failure.kind` saying why: `connection`, `authentication`,
  `query` (a statement error stopped the run, e.g. under `-b`), `timeout` (the
  last error before the failure was a timeout; sqlcmd does not stop on one),
  `cancelled` or `other` (e.g. `:EXIT(7)`). A statement error that does not
  stop the run is an `error` entry in a `success` document.
- **Batches.** One `batch` entry per batch sent (`GO` separates batches), with
  its duration. Its `text` is included only with `-e`, as text mode echoes it
  only with `-e`, so a password in, say, `CREATE LOGIN` is not written out by
  default.
- **Result sets.** `columns` give each column's name and its type as T-SQL
  declares it. Values are strings, as sqlcmd would print them (binary with a
  `0x` prefix); SQL `NULL` is JSON `null`. `rowsAffected` is the result set's
  "(n rows affected)", absent under `SET NOCOUNT ON`; a statement without a
  result set gets its own `rowsAffected` entry.
- **`FOR JSON` / `FOR XML`.** The server's own conversion is kept. Its rows,
  split into chunks by the server, are joined: JSON is written as the server
  sent it (`"serverFormat": "json", "json": [...]`) once it is known to parse,
  or as a string (`"text"`) when it does not, as with `WITHOUT_ARRAY_WRAPPER`
  over several rows; XML is a string (`"serverFormat": "xml", "xml": "..."`).
  `FOR JSON` over no rows gives `"json": null`. These entries have no
  `rowsAffected`, which would count chunks.
- **Messages and errors** carry `number`, `state`, `severity`, and `line` and
  `procedure` when the server reports them.

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

| Pipeline | File | When | Package version |
|---|---|---|---|
| NonOfficial Python Wheels Publish | `.pipeline/OneBranch/NonOfficialPythonWheelsPublish.yml` | Every merge to `main` and nightly; anyone can queue it | `<crate>-dev.<date>.<build>` / `<crate>-nightly.<date>`, published to `mssql-rs_Public` automatically |
| Official mssql-sqlcmd Build | `.pipeline/OneBranch/OfficialMssqlSqlcmdBuild.yml` | Every update of `stable` | The crate version, kept as the `drop_Build_MssqlSqlcmd_Package` artifact; never published |
| ADO-Release Nuget mssql-sqlcmd | `.pipeline/OneBranch/OfficialMssqlSqlcmdRelease.yml` | Manual | Publishes an Official Build's package, without a suffix |

All three build the same jobs, from `.pipeline/OneBranch/mssql-sqlcmd-jobs.yml`.
The non-official pipeline is for testing changes: point native sqlcmd at a
`-dev` or `-nightly` version. The release pipeline is separate from the Python
release.

To release:

1. Bump `version` in `Cargo.toml`. A version on a feed can never be replaced
   or reused, so every release is a new version.
2. Merge to `main`, then to `stable`; the Official Build produces the package.
3. Run ADO-Release Nuget mssql-sqlcmd on that build with `publishNuGet` ticked.
   Without it the run only validates: one package, not a prerelease, all nine
   runtimes present, and a version not already on the feed.
4. Promote the new version to the **Release** view of `mssql-rs_Public`
   (Artifacts → mssql-rs_Public → mssql-sqlcmd → the version → Promote →
   `@Release`). Versions left only in the feed's local view are deleted after
   30 days.
5. In msodbcsql, move the `mssql-sqlcmd` pin in `Directory.Packages.props` to
   the new version. msodbcsql reaches the package through the
   `mssql-rs_Public@Release` upstream, so it sees only promoted versions, and
   native sqlcmd keeps building against the version it pins until then.
