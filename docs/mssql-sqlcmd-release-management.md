# mssql-sqlcmd Release Management

How `mssql-sqlcmd` (the Rust library native sqlcmd links) is built, versioned,
released and consumed. For the crate itself, see
[mssql-sqlcmd/README.md](../mssql-sqlcmd/README.md).

## Architecture

```text
mssql-rs repo
└── mssql-sqlcmd/              ← Rust crate: static library + C ABI (include/mssql_sqlcmd.h)

        │  built for 9 runtimes, packed as one NuGet package: mssql-sqlcmd
        ▼

Azure Artifacts feed: public/mssql-rs_Public
  │  @Local    every published version (test builds and releases)
  │  @Release  only versions promoted after a release
        ▼   (upstream: mssql-rs_Public@Release)

Azure Artifacts feed: msodbcsql_PublicPackages
        ▼   (version pinned in msodbcsql Directory.Packages.props)

msodbcsql build → native sqlcmd (the library is linked into sqlcmd)
```

Customers never install `mssql-sqlcmd`; it ships inside native sqlcmd.

## Package

One NuGet package, `mssql-sqlcmd.<version>.nupkg`:

```text
include/mssql_sqlcmd.h
runtimes/<rid>/native/mssql_sqlcmd.lib  or  libmssql_sqlcmd.a
runtimes/<rid>/native/native-static-libs.txt   (system libraries to link, as rustc reports them)
```

| Runtime (RID) | Rust target | Built on |
|---|---|---|
| `win-x64`, `win-x86`, `win-arm64` | `{x86_64,i686,aarch64}-pc-windows-msvc` | one Windows x64 agent |
| `linux-x64`, `linux-arm64` | `{x86_64,aarch64}-unknown-linux-gnu` | manylinux_2_28 image |
| `linux-musl-x64`, `linux-musl-arm64` | `{x86_64,aarch64}-unknown-linux-musl` | musllinux_1_2 image, dynamic C runtime |
| `osx-x64`, `osx-arm64` | `{x86_64,aarch64}-apple-darwin` | macOS agent |

The pack step fails if any runtime is missing.

## Branches

| Branch | Meaning |
|---|---|
| `main` | Day-to-day development. PRs merge here. |
| `stable` | What is ready to ship. `main` is merged into it when releasing. |

## Pipelines

| Pipeline | Definition | YAML | Starts | Publishes |
|---|---|---|---|---|
| GH-Non-Official Python Wheels Publish | [2231](https://sqlclientdrivers.visualstudio.com/mssql-rs/_build?definitionId=2231) | `.pipeline/OneBranch/NonOfficialPythonWheelsPublish.yml` | every merge to `main`; nightly at 02:00 UTC; PRs to `main` (build only); manual | test versions (`-dev` / `-nightly`), automatically |
| Official mssql-sqlcmd Build | [2347](https://sqlclientdrivers.visualstudio.com/mssql-rs/_build?definitionId=2347) | `.pipeline/OneBranch/OfficialMssqlSqlcmdBuild.yml` | every update of `stable`; manual | nothing; keeps the package as the `drop_Build_MssqlSqlcmd_Package` artifact |
| ADO-Release Nuget mssql-sqlcmd | [2348](https://sqlclientdrivers.visualstudio.com/mssql-rs/_build?definitionId=2348) | `.pipeline/OneBranch/OfficialMssqlSqlcmdRelease.yml` | manual only | the clean version, when `publishNuGet` is ticked |

- The NonOfficial pipeline is the repo's shared test-build pipeline; with
  `buildMssqlSqlcmd: true` it builds and publishes the sqlcmd package.
  PR builds build but never publish.
- The Official build and the release pipeline are sqlcmd's own.
- All three build the same jobs, from `.pipeline/OneBranch/mssql-sqlcmd-jobs.yml`.
- Official pipelines must be registered in the product catalog (classification
  **Production**, service **SQL Server Rust Client**). 2347 is registered.

## Versions

The version comes from `version` in `mssql-sqlcmd/Cargo.toml`.

| Built by | Version | Example |
|---|---|---|
| Official build | the crate version | `0.1.0` |
| NonOfficial, nightly schedule | `<crate>-nightly.<yyyymmdd>` | `0.1.0-nightly.20261005` |
| NonOfficial, any other run | `<crate>-dev.<yyyymmdd>.<buildId>` | `0.1.0-dev.20261005.180123` |

- **`-dev`** identifies one specific build, one per merge to `main`: use it to
  test a change right after it merges.
- **`-nightly`** is one build per day of whatever `main` holds: use it as
  "latest as of today".
- **A version on a feed can never be replaced or reused.** Every release needs
  a new version in `Cargo.toml`. The release pipeline refuses a version that is
  already on the feed.

## Feed retention

`mssql-rs_Public` keeps at most **20 versions per package** and deletes the
oldest ones beyond that, except versions **downloaded in the last 30 days**.
Versions **promoted to the `Release` view are never deleted**.

Test builds are never promoted and clean themselves up. A released version must
be promoted, or a busy stream of `-dev` builds will push it out.

## Releasing

Done by the release owner, the person who runs the release pipeline.

1. **Bump the version.** Set `version` in `mssql-sqlcmd/Cargo.toml`, merge to
   `main`.
2. **Merge `main` into `stable`.** The Official mssql-sqlcmd Build (2347) starts
   on its own. Check it succeeds: all 9 runtimes, and the package job.
3. **Run the release.** Queue ADO-Release Nuget mssql-sqlcmd (2348), select that
   Official build as the resource, and tick `publishNuGet`.
   - Without `publishNuGet` the run only validates.
   - It checks: exactly one package, no prerelease suffix, all 9 runtimes,
     and a version not already on the feed.
   - It publishes to `public/mssql-rs_Public` (the `Local` view).
4. **Promote it.** Azure DevOps → Artifacts → `mssql-rs_Public` →
   `mssql-sqlcmd` → the version → **Promote** → `Release`.
5. **Update msodbcsql.** Change the `mssql-sqlcmd` version in msodbcsql's
   `Directory.Packages.props`. msodbcsql sees only promoted versions, so it keeps
   building with the version it pins until then.

## Consumer: msodbcsql

- **Windows:** `NuGetRestore/NuGetRestore.proj` restores the package with the
  others, and `sqlcmd.props` links the library for the platform being built.
- **Linux and macOS:** `.pipelines/scripts/fetch-nuget-package.sh mssql-sqlcmd`
  downloads the same pinned version into `packages/`, where the sqlcmd Makefile
  finds it.
- **Upstream:** `msodbcsql_PublicPackages` reads `mssql-rs_Public@Release` as
  an upstream source. Adding it is a one-time change by a feed admin
  (Cheena Malhotra, David Engel, Mahendra Chavan, Milos Cimfl, or a project
  admin). No cross-project write permission is needed.
- **Opt-in:** until a released version is reachable, restoring the package is
  off unless `SQLCMD_RUST_PACKAGE_ENABLED=true`. Without the package, sqlcmd
  builds as before, without the Rust features.

## Testing a change before a release

- **From the feed:** pin a `-dev` or `-nightly` version in msodbcsql (or fetch
  it) after the change merges to `main`.
- **From a local build:** build and pack the same layout with
  `.pipeline/scripts/build-mssql-sqlcmd-native.{ps1,sh}` and
  `.pipeline/scripts/pack-mssql-sqlcmd.ps1`, then point native sqlcmd at it with
  `SQLCMD_RUST_PACKAGE_DIR=<staging directory>`. See the crate README.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Official build fails in **PolicyValidation → Inventory Compliance Validation**: "Pipeline is missing service assignment or classification in product catalog" | the pipeline is not registered in the product catalog | Assign it to the service (classification **Production**) through the link in the error, e.g. `https://product-catalog-web.prod.space.microsoft.com/ownership/build/7207cf78-9b57-4b4b-b274-c803cac0efe0/b95cf060-8083-439d-8ef1-405d5bf219d8/<definitionId>`. See [aka.ms/pipelineassignment](https://aka.ms/pipelineassignment). Needs a service admin. |
| A new pipeline waits on **Checkpoint.Authorization** for `RUST-X64-WUS3` / `RUST-ARM64-WUS3` | the agent pools are not yet authorized for it | A pool admin permits it once from the run page. |
| Release fails: version already on the feed | the version was published before | Bump `version` in `Cargo.toml`; versions cannot be reused. |
| Release fails: prerelease version | the selected build is not an Official build | Select a run of the Official mssql-sqlcmd Build (2347). |
| No `-dev` package after a run | the run was a PR build | PR builds never publish; merges, nightly and manual runs do. |
| msodbcsql cannot find a released version | not promoted, or the upstream is missing | Promote it to `Release`; check `msodbcsql_PublicPackages` has the `mssql-rs_Public@Release` upstream. |
