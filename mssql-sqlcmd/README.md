# mssql-sqlcmd

sqlcmd components in Rust. Native (ODBC) sqlcmd links this crate as a static
library and calls it through a C ABI; native sqlcmd still parses the command
line, connects and runs the batches.

## Layout

| Path | Purpose |
|---|---|
| `src/formatter/` | Output formatters. `json.rs` renders `--format json`. |
| `src/ffi.rs` | The C ABI native sqlcmd calls. |
| `include/mssql_sqlcmd.h` | C/C++ declarations for that ABI. |

## JSON output

Native sqlcmd hands each piece of output to a JSON document instead of printing
it — result sets and rows, row counts, server messages — and renders the
document once, at exit, with the connection details and exit code:

```json
{
  "sqlcmd": { "version": "18.5.1.1" },
  "connection": { "server": "localhost", "database": "master", "authentication": "SqlPassword", "encrypt": true },
  "exitCode": 0,
  "output": [
    { "type": "resultSet", "columns": ["id"], "rows": [["1"], [null]] },
    { "type": "rowsAffected", "count": 2 },
    { "type": "error", "number": 50000, "state": 1, "severity": 16, "message": "boom" }
  ]
}
```

Values are strings, as sqlcmd would print them; SQL `NULL` is JSON `null`.

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
`mssql-rs_Public` as an upstream source. The package holds one static library
per runtime native sqlcmd ships on:

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

Two pipelines of their own, separate from the Python wheels ones:

| Pipeline | File | Does |
|---|---|---|
| Official mssql-sqlcmd Build | `.pipeline/OneBranch/OfficialMssqlSqlcmdBuild.yml` | Builds every runtime and packs `mssql-sqlcmd.<version>.nupkg` as the `drop_Build_MssqlSqlcmd_Package` artifact. Runs on merges to `stable` that touch this crate or its build scripts; never publishes. |
| Official mssql-sqlcmd Release | `.pipeline/OneBranch/OfficialMssqlSqlcmdRelease.yml` | Manual. Validates the package of the Official Build run you pick and, with `publishNuGet`, publishes it to `public/mssql-rs_Public`. |

The jobs live in `.pipeline/OneBranch/mssql-sqlcmd-stages.yml`.

To release:

1. Bump `version` in `Cargo.toml`. A version on a feed can never be replaced
   or reused, so every release is a new version.
2. Merge; the Official Build produces the package with that version.
3. Run the Official Release on that build with `publishNuGet` ticked. Without
   it the run only validates: one package, not a prerelease, all nine runtimes
   present, and a version not already on the feed.
4. In msodbcsql, move the `mssql-sqlcmd` pin in `Directory.Packages.props` to
   the new version. Native sqlcmd keeps building against the version it pins
   until then.
