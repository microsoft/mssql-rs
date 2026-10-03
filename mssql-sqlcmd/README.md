# mssql-sqlcmd

sqlcmd components in Rust. Native (ODBC) sqlcmd links this crate as a static
library and calls it through a C ABI; native sqlcmd still parses the command
line, connects and runs the batches.

## Layout

| Path | Purpose |
|---|---|
| `src/ffi.rs` | The C ABI native sqlcmd calls. |
| `include/mssql_sqlcmd.h` | C/C++ declarations for that ABI. |

The library exports `mssql_sqlcmd_version()` so far; the features sqlcmd
hands to it are added on top.

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
