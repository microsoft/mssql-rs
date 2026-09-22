# mssql-python-rs Release Management

How changes in `mssql-rs` (Rust) flow to `mssql-python` (Python) through the
wheel build, internal NuGet transport, and public PyPI release pipelines.

## Architecture

```
mssql-rs repo (Rust)
├── mssql-tds/          ← Core TDS protocol crate
├── mssql-py-core/      ← PyO3 bindings (cdylib), produces Python wheels
└── .pipeline/OneBranch/ ← Builds wheels, packages into NuGet

        │  builds 9 stable-ABI wheels (one per supported platform)
        │  packages into NuGet: mssql-python-rs-wheels
        ▼

Azure Artifacts feed: mssql-rs/mssql-rs
  │  NuGet transports the same wheels for pre-release CI validation
        ▼

PyPI: mssql-python-rs
  │  standalone runtime dependency
  │  exact version pinned by mssql-python
        ▼

PyPI: mssql-python
```

`mssql-python` does not embed or repackage `mssql_py_core`. Its build and test
pipelines stage the NuGet-transported wheels and let pip select the compatible
wheel. Public users install `mssql-python-rs` from PyPI through the exact runtime
dependency declared by `mssql-python`.

## Wheel Matrix (9 wheels)

| Platform | Wheel tag | Supported CPython versions |
|---|---|---|
| Windows x64 (`win_amd64`) | `cp310-abi3` | 3.10+ |
| Windows ARM64 (`win_arm64`) | `cp310-abi3` | 3.11+ |
| Linux glibc 2.28 x64 (`manylinux_2_28_x86_64`) | `cp310-abi3` | 3.10+ |
| Linux glibc 2.28 ARM64 (`manylinux_2_28_aarch64`) | `cp310-abi3` | 3.10+ |
| Linux glibc 2.34 x64 (`manylinux_2_34_x86_64`) | `cp310-abi3` | 3.10+ |
| Linux glibc 2.34 ARM64 (`manylinux_2_34_aarch64`) | `cp310-abi3` | 3.10+ |
| Linux musl x64 (`musllinux_1_2_x86_64`) | `cp310-abi3` | 3.10+ |
| Linux musl ARM64 (`musllinux_1_2_aarch64`) | `cp310-abi3` | 3.10+ |
| macOS universal2 (`macosx_15_0_universal2`) | `cp310-abi3` | 3.10+ |

Each wheel uses Python's stable ABI and replaces the per-minor CPython wheels for
its platform. Free-threaded CPython is not supported by `abi3`.

---

## Version Scheme

### Scheduled Rust crate version bumps

The **Bump Released Crate Versions** GitHub workflow checks the default branch
against crates.io every three UTC days. A daily trigger at 08:23 UTC runs the
check on days whose Unix day number is divisible by three; unlike `*/3` in the
day-of-month field, this does not reset at month boundaries. GitHub may delay or
skip scheduled runs. **Run workflow** bypasses the date check and still targets
the default branch.

`mssql-tds` and `mssql-mock-tds` are checked independently. If a crate's exact
`[package].version` exists among its published versions (including older or
yanked releases), the workflow suggests the next minor version: `0.1.7` becomes
`0.2.0`. Unpublished source versions and crates that return HTTP 404 are left
alone. Other registry errors fail the run rather than assuming a crate is
unpublished. If the proposed next minor version is also published, the run fails
and a maintainer must choose a new version. Before any bump is planned, the
workflow also fails when the current `mssql-tds` and `mssql-mock-tds` source
versions differ in either direction (`mssql-tds` and `mssql-mock-tds` must stay
aligned for a release), because silently tolerating a drifted state can leave the
repo in a release-invalid configuration. If the mock source version is ahead of
the core target, the run also fails so a maintainer can choose the versions
manually. A mock-only bump is deferred until the current `mssql-tds` version is
published, because the mock crate's exact versioned dependency must resolve from
the registry when it is packaged. When `mssql-tds` is bumped, the mock source
version is kept aligned with the core target because the release pipeline stamps
the mock crate from the core version.

Local versions come from `cargo metadata`. The workflow does not edit files,
push branches, create PRs, publish crates, merge anything, or change the release
pipeline's existing version-stamping policy. If a matching open version-bump PR
targets the default branch, the workflow skips issue creation.

When a bump is needed, the workflow creates or updates one tracking issue labeled
`automation:crate-version-bump`. The issue includes:

1. the affected crates and suggested next minor versions;
2. TOML snippets for the manifest edits, including the mock crate's versioned
   `mssql-tds` dependency when the core crate is bumped;
3. crisp maintainer instructions to apply the snippets, run `cargo fetch` (or
   `cargo update --workspace --offline`) to refresh `Cargo.lock`, then run
   `cargo bfmt`, `cargo bclippy`, and `cargo btest`, and finally open a PR with
   `Fixes #<issue-number>` and the version summary in the PR description.

The workflow queries only open issues labeled `automation:crate-version-bump`,
then checks the hidden marker in their bodies before reusing or updating one.
It regenerates the issue body while the bump is pending; put maintainer notes
in issue comments. The label is created automatically when needed; multiple
matching open issues fail the run for manual cleanup.
When no bump is needed, no issue is changed. If a bump becomes unnecessary, close
its issue manually.

The shared validation pipeline runs `scripts/test_bump_released_crate_versions.py`
in its Windows Python test step for both PR validation and main-branch CI.
Cargo metadata, registry, pull request, and issue requests are mocked. No
cargo-edit installation or live issue/PR writes are needed.

The workflow uses the built-in `GITHUB_TOKEN` with contents read, issues write,
and pull request read permissions. No custom secret or permission to create PRs
is needed. Normal validation and review are still required before merging.

### Python and NuGet versions

The NuGet transport package and `mssql-python-rs` Python distribution share the
version from `mssql-py-core/pyproject.toml`. The Rust crate has an independent
version in `mssql-py-core/Cargo.toml`. For example, NuGet
`mssql-python-rs-wheels 0.1.0` contains `mssql_python_rs-0.1.0-*.whl`, while the
Rust crate can have a different version.

The NuGet prerelease suffix depends on the build type:

| Build Type | Version Format | Example |
|---|---|---|
| **Nightly** (scheduled) | `{version}-nightly.{YYYYMMDD}` | `0.2.0-nightly.20260217` |
| **Dev** (CI push / manual non-official) | `{version}-dev.{YYYYMMDD}.{BuildId}` | `0.2.0-dev.20260217.140071` |
| **Release** (manual official) | `{version}` | `0.2.0` |
| **PR** | Wheels built for validation only — **not published** | — |

NuGet SemVer 2.0 ordering: `dev` < `nightly` < release (no suffix).

Earlier development packages used the Cargo version and were published as
`0.1.10-dev.*`. Those packages sort above the new `0.1.0-*` lineage. Consumers
must select or pin the intended `0.1.0` lineage rather than resolving the latest
prerelease across all versions.

---

## Scenario 1: Nightly Builds

**Purpose**: Produce a daily "latest from main" package that `mssql-python` CI can consume for continuous integration testing.

**What triggers it**: Scheduled cron (`0 2 * * *` UTC) on the `main` branch.

### Flow

```
1. Schedule triggers at 2 AM UTC
2. Pipeline builds 9 stable-ABI wheels across all platforms
3. Publish stage:
  - Extracts the Python distribution version from mssql-py-core/pyproject.toml (e.g., 0.2.0)
   - Appends -nightly.YYYYMMDD suffix
  - Packs wheels into NuGet: mssql-python-rs-wheels.0.2.0-nightly.20260217
   - OneBranch auto-publishes to mssql-rs/mssql-rs feed
4. mssql-python CI (next run) picks up the latest nightly
```

### What mssql-python does

In its pipeline or `pyproject.toml` build script, `mssql-python` references the NuGet feed:

```
# Download latest nightly wheels NuGet
nuget install mssql-python-rs-wheels -Version 0.2.0-nightly.* -Source mssql-rs/mssql-rs -Prerelease
```

Or pin a specific nightly:

```
nuget install mssql-python-rs-wheels -Version 0.2.0-nightly.20260217 -Source mssql-rs/mssql-rs
```

### Key properties

- One nightly per day (same date stamp = same version → `continueOnConflict: true` on feed)
- Always builds from `main` — represents the latest merged state
- If `main` has no changes, nightly still runs (`always: true`) to confirm nothing is broken

---

## Scenario 2: Dev Builds (Testing Changes Faster in mssql-python)

**Purpose**: When a developer makes a change in `mssql-rs` and wants to test it in `mssql-python` *before* waiting for the nightly.

### Flow: Push to main/development

```
1. Developer merges PR to main or development
2. CI trigger fires immediately
3. Pipeline builds 9 stable-ABI wheels
4. Publish stage produces: mssql-python-rs-wheels.0.2.0-dev.20260217.140071
   (BuildId ensures uniqueness even with multiple merges per day)
5. Developer tells mssql-python to use this specific version
```

### Flow: Manual trigger for ad-hoc testing

```
1. Developer triggers pipeline manually from any branch
2. Pipeline builds 9 stable-ABI wheels
3. Publish stage produces: mssql-python-rs-wheels.0.2.0-dev.20260217.140095
4. Developer uses this version in mssql-python for testing
```

### How to test a specific dev build in mssql-python

1. Note the NuGet version from the pipeline output (e.g., `0.2.0-dev.20260217.140071`)
2. In the `mssql-python` build pipeline or local dev setup:

```powershell
# Download the specific dev wheels
nuget install mssql-python-rs-wheels -Version 0.2.0-dev.20260217.140071 -Source mssql-rs/mssql-rs
# Extract wheels and run mssql-python tests against them
```

3. Once validated, the change flows to nightlies automatically after merge to `main`

### Key properties

- Every push to `main`/`development` produces a unique dev package
- BuildId guarantees no version collisions
- Dev packages have lower SemVer precedence than nightlies
- PR builds do NOT publish — they only validate that wheels compile

---

## Scenario 3: Upgrading mssql-python to a New Native Package Version

**Purpose**: When `mssql-python` needs to adopt a new `mssql-python-rs` distribution version (e.g., new features or bug fixes).

### Steps in mssql-rs

1. **Make changes** to `mssql-tds` and/or `mssql-py-core`
2. **Bump `[project].version`** in `mssql-py-core/pyproject.toml` when the Python distribution version changes
   - This version controls the wheel filenames, PyPI distribution, and NuGet transport package
3. **Manage Rust crate versions independently**
   - Bump `[package].version` in a crate's `Cargo.toml` only when that Rust crate version changes
   - A Rust crate bump does not change the Python wheel or NuGet version
4. **Merge PR** — CI produces `mssql-python-rs-wheels.0.2.1-dev.YYYYMMDD.BuildId`
5. **Nightly** picks it up: `mssql-python-rs-wheels.0.2.1-nightly.YYYYMMDD`

### Steps in mssql-python

1. **Update NuGet reference** to the new version:

```yaml
# In mssql-python's build pipeline
- task: NuGetCommand@2
  inputs:
    command: restore
    # Update from 0.2.0 to 0.2.1 (or use -nightly.* for latest)
    restoreSource: mssql-rs/mssql-rs
    packages: mssql-python-rs-wheels@0.2.1-nightly.*
```

2. **Update any Python-side bindings** if the native API changed (new functions, changed signatures)
3. **Run mssql-python test suite** against the new wheels
4. **Merge and release** mssql-python with the new native core

### Sprint example

```
Sprint 42 starts. Current Python distribution: mssql-python-rs 0.2.0
Independent Rust crate version: mssql-py-core 0.1.10

Week 1:
  mssql-rs PR #101: Fix connection timeout
  → bumps pyproject.toml from 0.2.0 → 0.2.1
  → leaves the Rust crate at 0.1.10 unless that crate needs its own release
  → merges → dev package: 0.2.1-dev.20260303.11001
  → nightly: 0.2.1-nightly.20260303
  mssql-python: tests against 0.2.1-nightly.20260303 ✅

Week 2:
  mssql-rs PR #105: Add retry logic (Python distribution stays 0.2.1)
  → merges → dev package: 0.2.1-dev.20260310.11042
  → nightly: 0.2.1-nightly.20260310
  mssql-python: tests against 0.2.1-nightly.20260310 ✅

Sprint end:
  mssql-rs: Official release → mssql-python-rs-wheels.0.2.1 (clean)
  mssql-python: pins to 0.2.1 release, does its own release
```

---

## Scenario 4: Release Activities

**Purpose**: Produce a production-quality, signed, immutable release of `mssql-python-rs-wheels`.

### Pre-release checklist (mssql-rs)

- [ ] All PRs for the sprint are merged to `main`
- [ ] Latest nightly (`0.2.1-nightly.*`) is passing in `mssql-python` CI
- [ ] `[project].version` in `mssql-py-core/pyproject.toml` is correct (e.g., `0.2.1`)
- [ ] Any Rust crate selected for release has the intended independent version in its `Cargo.toml`
- [ ] No outstanding breaking changes without coordination

### Release build

1. **Run the Official Python Wheels Build** for the source commit to release
2. **Trigger the Official release pipeline** with that build selected and `publishNuGet: true`
   - This produces a clean semver NuGet: `mssql-python-rs-wheels.0.2.1`
   - OneBranch runs full SDL scanning (BinSkim, Clippy, AV)
   - Package is published to `mssql-rs/mssql-rs` feed

### Optional mssql-py-core source tag

The release pipeline's `tagRelease` option is separate from NuGet publication. It reads
`[package].version` from `mssql-py-core/Cargo.toml`, then creates the corresponding tag
and release branch. For a Rust crate version of `0.1.10`, it creates:

```bash
git tag -a v0.1.10 -m "Release 0.1.10"
git push origin v0.1.10
```

```bash
git checkout -b release/0.1.10 v0.1.10
git push origin release/0.1.10
```

Do not derive this tag or branch from the Python/NuGet version when the versions differ.

### Post-release in mssql-python

1. Update NuGet reference to the clean release version: `mssql-python-rs-wheels@0.2.1`
2. Run full test suite
3. Update `mssql-python` version (e.g., bump to `1.4.0`)
4. Publish to PyPI

### Releasing existing Official Build artifacts

`.pipeline/OneBranch/OfficialPythonWheelsRelease.yml` consumes the selected
`Official Python Wheels Build` run without rebuilding it. All release switches
default to `false`. Official wheel artifacts are always downloaded and validated
against that build's source metadata, even when NuGet publishing is disabled.

| Switch | Behavior |
|---|---|
| `publishNuGet` | Prepare, pack, verify and publish the NuGet package after wheel validation. When false, these NuGet-only steps are omitted; wheel download and validation still run. |
| `publishMssqlTds` | Select `mssql-tds` for crates.io publication through ESRP. |
| `publishMssqlMockTds` | Select `mssql-mock-tds` for crates.io publication through ESRP. |
| `validateCratesOnly` | Validate the crate artifact and run selected-crate preflights without ESRP publication. Can also validate the artifact with neither crate selected. |
| `tagRelease` | Tag and branch the selected build's `mssql-py-core` version. This does not tag Rust crate versions. |

The wheel validation/NuGet and Rust crate stages have no dependency on each other.
The crate stage downloads only crate artifacts and does not wait for wheel
validation or NuGet. A failure in either wheel validation or NuGet does not block
crates. OneBranch's per-job policy validation still applies. When publishing both
crates, the core must become available on
crates.io before the mock is published; mock-only publication requires the core
version to be available already.

The Rust job graph is `ValidateCrates -> RegistryPreflight -> PublishCore ->
CoreAvailable -> PublishMock -> MockAvailable` when both crates are selected.
Core-only stops after `CoreAvailable`; mock-only checks the existing core in
`RegistryPreflight` before `PublishMock`. With `validateCratesOnly`, publication
and post-publication waits are omitted. Selected versions must still be absent,
and mock-only still requires its existing core dependency. Selecting both crates
for validation does not wait for an unpublished proposed core version.

Crates.io HTTP requests run only in read-only custom Windows jobs on the
`Azure Pipelines` pool (`windows-2022`), supported by GovernedTemplates'
`Windows.Custom.Job.yml`. Those jobs run without a container or a user-specified
`target: host`; policy validation and the prohibition on custom-pool release
tasks remain enabled. They receive no ESRP variable group or release service
connection. Offline crate validation and ESRP publication remain
governed. ESRP waits for completion; the network availability job then confirms
that consumers can resolve the crate before a dependent publication starts.

Every Rust job re-downloads `drop_Build_RustCrates` from the same immutable
`resources.pipeline.officialBuild` run and checks the original manifest, archive
hashes, and mock dependency. Publishers do not consume files produced by registry
jobs. No crate job relies on another job's filesystem or job-local variables.

Tagging always waits for successful wheel validation, plus NuGet publication when
`publishNuGet` is selected. It never waits for Rust crate publication. Leaving all
switches off is a wheel validation-only run: no NuGet packaging or publication,
ESRP, Git tags, or release-branch writes.

NuGet and tag metadata come from the selected build's exact source commit, not
the release pipeline's checkout or the current branch tip. A missing branch,
commit, or required source file fails the operation without a checkout fallback.
Governed Git jobs configure OneBranch's built-in checkout with `ob_git_fetchDepth` and
`ob_git_persistCredentials`; do not add a second `checkout: self`. A duplicate
checkout can relocate the repository while governed task restrictions prevent
updating the source path. Git commands use the explicit source directory, and
wheel validation and packaging remain inside the governed build container.
Custom registry jobs declare an explicit `checkout: self`, which replaces Azure
Pipelines' implicit checkout rather than adding a second one. Native template
preview confirms exactly one checkout per job.

For release-pipeline changes, use the ADO Preview API first to inspect expanded
gates and OneBranch policy/checkout settings without queuing a run. Preview proves
the selected agent context, not runtime pool authorization or registry reachability.
Once a live run is authorized, select a known successful Official Build and leave all switches
off to exercise genuine artifacts in the governed container without publishing.
That all-off run does not exercise the crate registry jobs. For an authorized
non-publishing crate preflight run, set `validateCratesOnly: true` and select
`publishMssqlTds` and/or `publishMssqlMockTds`; leave `publishNuGet` and `tagRelease`
false. Use an Official Build whose selected crate versions are not yet published.
Mock-only validation also requires its core version to be available already.
Local regression tests also cover missing wheels, incorrect names/versions,
missing ODBC payloads, and exact-source metadata failures.

### Hotfix process

If a critical bug is found after release:

1. Cherry-pick the fix to the appropriate maintenance branch
2. Bump `[project].version` in `mssql-py-core/pyproject.toml` for the Python hotfix (for example, `0.2.1` → `0.2.2`)
3. Bump `[package].version` in `mssql-py-core/Cargo.toml` only if the Rust crate also needs a new independent version
4. Run the Official Python Wheels Build for the hotfix commit
5. Trigger the Official release pipeline with `publishNuGet: true`
   - This publishes `mssql-python-rs-wheels.0.2.2`
6. Enable `tagRelease` only when creating the separate Cargo-versioned mssql-py-core tag and release branch
7. Update `mssql-python` to use `mssql-python-rs==0.2.2`

---

## Pipeline Files Reference

| File | Purpose |
|---|---|
| `.pipeline/OneBranch/NonOfficialPythonWheelsPublish.yml` | Pipeline entry point (NonOfficial) — triggers, schedule, nugetPublishing config |
| `.pipeline/OneBranch/stages.yml` | Build + Publish stages — 5 build jobs, NuGet packaging |
| `.pipeline/OneBranch/OfficialPythonWheelsRelease.yml` | Independent opt-in NuGet and Rust crate releases from an existing Official Build, plus optional `mssql-py-core` tagging |
| `.pipeline/templates/build-python-wheels-template.yml` | Shared wheel build template (manylinux, musllinux, Windows, macOS) |
| `.pipeline/templates/install-dependencies.yml` | Dependency installation (Rust, Python, etc.) |
| `.pipeline/templates/cargo-authenticate-template.yml` | Cargo registry authentication |
| `.pipeline/validation-pipeline.yml` | CI/PR validation pipeline (non-OneBranch) |

## NuGet Package Structure

```
mssql-python-rs-wheels.0.1.0.nupkg
├── mssql-python-rs-wheels.nuspec
└── wheels/
  ├── mssql_python_rs-0.1.0-cp310-abi3-win_amd64.whl
  ├── mssql_python_rs-0.1.0-cp310-abi3-win_arm64.whl
  ├── mssql_python_rs-0.1.0-cp310-abi3-manylinux_2_28_x86_64.whl
  ├── mssql_python_rs-0.1.0-cp310-abi3-manylinux_2_28_aarch64.whl
  ├── mssql_python_rs-0.1.0-cp310-abi3-manylinux_2_34_x86_64.whl
  ├── mssql_python_rs-0.1.0-cp310-abi3-manylinux_2_34_aarch64.whl
  ├── mssql_python_rs-0.1.0-cp310-abi3-musllinux_1_2_x86_64.whl
  ├── mssql_python_rs-0.1.0-cp310-abi3-musllinux_1_2_aarch64.whl
  └── mssql_python_rs-0.1.0-cp310-abi3-macosx_15_0_universal2.whl
```

## Traceability

Every NuGet package description includes:
- Git commit SHA (first 8 chars)
- Azure DevOps build number

```
mssql-python-rs-wheels 0.1.0
Description: Python wheels containing the mssql-python-rs TDS core and ODBC driver. Commit: a1b2c3d4. Build: 20260217.1
```

This creates: **NuGet version → package description → source commit and pipeline run with logs**.
When `tagRelease` is enabled, the separate Cargo-versioned tag and release branch provide
additional source traceability for `mssql-py-core`; a NuGet release does not require that tag.

## Feed Retention Guidelines

| Package Type | Suggested Retention |
|---|---|
| Release (`0.2.1`) | Permanent |
| Nightly (`0.2.1-nightly.*`) | 30 days |
| Dev (`0.2.1-dev.*`) | 7 days |

Configure retention policies on the `mssql-rs/mssql-rs` Azure Artifacts feed to auto-clean old prerelease packages.
