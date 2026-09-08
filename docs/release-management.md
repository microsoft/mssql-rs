# mssql-python-rs Release Management

How changes in `mssql-rs` (Rust) flow to `mssql-python` (Python) through the wheel build and NuGet publishing pipeline.

## Architecture

```
mssql-rs repo (Rust)
├── mssql-tds/          ← Core TDS protocol crate
├── mssql-py-core/      ← PyO3 bindings (cdylib), produces Python wheels
└── .pipeline/OneBranch/ ← Builds wheels, packages into NuGet

        │  builds 34 wheels (5 Python × 7 platforms)
        │  packages into NuGet: mssql-python-rs-wheels
        ▼

Azure Artifacts feed: mssql-rs/mssql-rs
        │  NuGet contains wheels/ folder with all .whl files
        ▼

mssql-python repo (Python)
        │  downloads NuGet, extracts native .so/.dll/.dylib from wheels
        │  repackages into mssql-python distribution
        ▼

PyPI: mssql-python
```

## Wheel Matrix (34 wheels)

| Platform | Python 3.10 | 3.11 | 3.12 | 3.13 | 3.14 |
|---|---|---|---|---|---|
| Windows x64 (`win_amd64`) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Windows ARM64 (`win_arm64`) | — | ✅ | ✅ | ✅ | ✅ |
| Linux glibc x64 (`linux_x86_64`) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Linux glibc ARM64 (`linux_aarch64`) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Linux musl x64 (`musllinux_1_2_x86_64`) | ✅ | ✅ | ✅ | ✅ | ✅ |
| Linux musl ARM64 (`musllinux_1_2_aarch64`) | ✅ | ✅ | ✅ | ✅ | ✅ |
| macOS universal2 (`macosx_15_0_universal2`) | ✅ | ✅ | ✅ | ✅ | ✅ |

> Python 3.10 does not produce `win_arm64` wheels due to limited platform support.

---

## Version Scheme

The Rust crate and NuGet transport package share the version from
`mssql-py-core/Cargo.toml`. The `mssql-python-rs` Python distribution has an
independent version in `mssql-py-core/pyproject.toml`. For example, NuGet
`mssql-python-rs-wheels 0.1.10` can contain `mssql_python_rs-0.1.0-*.whl`.

The NuGet prerelease suffix depends on the build type:

| Build Type | Version Format | Example |
|---|---|---|
| **Nightly** (scheduled) | `{version}-nightly.{YYYYMMDD}` | `0.2.0-nightly.20260217` |
| **Dev** (CI push / manual non-official) | `{version}-dev.{YYYYMMDD}.{BuildId}` | `0.2.0-dev.20260217.140071` |
| **Release** (manual official) | `{version}` | `0.2.0` |
| **PR** | Wheels built for validation only — **not published** | — |

NuGet SemVer 2.0 ordering: `dev` < `nightly` < release (no suffix).

---

## Scenario 1: Nightly Builds

**Purpose**: Produce a daily "latest from main" package that `mssql-python` CI can consume for continuous integration testing.

**What triggers it**: Scheduled cron (`0 2 * * *` UTC) on the `main` branch.

### Flow

```
1. Schedule triggers at 2 AM UTC
2. Pipeline builds 34 wheels across all platforms
3. Publish stage:
   - Extracts version from mssql-py-core/Cargo.toml (e.g., 0.2.0)
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
3. Pipeline builds 34 wheels
4. Publish stage produces: mssql-python-rs-wheels.0.2.0-dev.20260217.140071
   (BuildId ensures uniqueness even with multiple merges per day)
5. Developer tells mssql-python to use this specific version
```

### Flow: Manual trigger for ad-hoc testing

```
1. Developer triggers pipeline manually from any branch
2. Pipeline builds 34 wheels
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

**Purpose**: When `mssql-python` needs to adopt a new version of the native core (e.g., new features, bug fixes).

### Steps in mssql-rs

1. **Make changes** to `mssql-tds` and/or `mssql-py-core`
2. **Bump version** in `mssql-py-core/Cargo.toml` (and `mssql-tds/Cargo.toml` if changed)
   - First code change in a sprint bumps the version
   - Subsequent changes in the same sprint do NOT bump again
3. **Merge PR** — CI produces `mssql-python-rs-wheels.0.2.1-dev.YYYYMMDD.BuildId`
4. **Nightly** picks it up: `mssql-python-rs-wheels.0.2.1-nightly.YYYYMMDD`

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
Sprint 42 starts. Current released: mssql-py-core 0.2.0

Week 1:
  mssql-rs PR #101: Fix connection timeout (bumps 0.2.0 → 0.2.1)
  → merges → dev package: 0.2.1-dev.20260303.11001
  → nightly: 0.2.1-nightly.20260303
  mssql-python: tests against 0.2.1-nightly.20260303 ✅

Week 2:
  mssql-rs PR #105: Add retry logic (no version bump — stays 0.2.1)
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
- [ ] Version in `mssql-py-core/Cargo.toml` is correct (e.g., `0.2.1`)
- [ ] No outstanding breaking changes without coordination

### Release build

1. **Trigger the Official pipeline manually** with `isOfficial: true`
  - This produces a clean semver NuGet: `mssql-python-rs-wheels.0.2.1`
   - OneBranch runs full SDL scanning (BinSkim, Clippy, AV)
   - Package is published to `mssql-rs/mssql-rs` feed

2. **Tag the release commit**:

```bash
git tag -a v0.2.1 -m "Release 0.2.1"
git push origin v0.2.1
```

3. **Create release branch** (for potential hotfixes):

```bash
git checkout -b release/0.2.1 v0.2.1
git push origin release/0.2.1
```

### Post-release in mssql-python

1. Update NuGet reference to the clean release version: `mssql-python-rs-wheels@0.2.1`
2. Run full test suite
3. Update `mssql-python` version (e.g., bump to `1.4.0`)
4. Publish to PyPI

### Hotfix process

If a critical bug is found after release:

```
main            ──●──●──●──●──  (sprint 43 work continues with 0.3.0)
                       │
release/0.2.1   ───────●── cherry-pick fix
                       │
                       └── bump to 0.2.2, trigger Official pipeline
                           → mssql-python-rs-wheels.0.2.2
                           → tag v0.2.2
```

Steps:
1. Cherry-pick fix to `release/0.2.1`
2. Bump `mssql-py-core/Cargo.toml` to `0.2.2`
3. Trigger Official pipeline from `release/0.2.1` branch
4. Tag `v0.2.2`, update `mssql-python` to use `0.2.2`

---

## Pipeline Files Reference

| File | Purpose |
|---|---|
| `.pipeline/OneBranch/NonOfficialPythonWheelsPublish.yml` | Pipeline entry point (NonOfficial) — triggers, schedule, nugetPublishing config |
| `.pipeline/OneBranch/stages.yml` | Build + Publish stages — 5 build jobs, NuGet packaging |
| `.pipeline/templates/build-python-wheels-template.yml` | Shared wheel build template (manylinux, musllinux, Windows, macOS) |
| `.pipeline/templates/install-dependencies.yml` | Dependency installation (Rust, Python, etc.) |
| `.pipeline/templates/cargo-authenticate-template.yml` | Cargo registry authentication |
| `.pipeline/validation-pipeline.yml` | CI/PR validation pipeline (non-OneBranch) |

## NuGet Package Structure

```
mssql-python-rs-wheels.0.1.10.nupkg
├── mssql-python-rs-wheels.nuspec
└── wheels/
  ├── mssql_python_rs-0.1.0-cp310-cp310-win_amd64.whl
  ├── mssql_python_rs-0.1.0-cp310-cp310-linux_x86_64.whl
  ├── mssql_python_rs-0.1.0-cp310-cp310-linux_aarch64.whl
  ├── mssql_python_rs-0.1.0-cp310-cp310-musllinux_1_2_x86_64.whl
  ├── mssql_python_rs-0.1.0-cp310-cp310-musllinux_1_2_aarch64.whl
  ├── mssql_python_rs-0.1.0-cp310-cp310-macosx_15_0_universal2.whl
  ├── mssql_python_rs-0.1.0-cp311-cp311-win_amd64.whl
  ├── mssql_python_rs-0.1.0-cp311-cp311-win_arm64.whl
    ├── ... (34 wheels total)
  └── mssql_python_rs-0.1.0-cp314-cp314-macosx_15_0_universal2.whl
```

## Traceability

Every NuGet package description includes:
- Git commit SHA (first 8 chars)
- Azure DevOps build number

```
mssql-python-rs-wheels 0.1.10
Description: Python wheels containing the mssql-python-rs TDS core and ODBC driver. Commit: a1b2c3d4. Build: 20260217.1
```

This creates: **NuGet version → git tag → exact source commit → pipeline run with logs**.

## Feed Retention Guidelines

| Package Type | Suggested Retention |
|---|---|
| Release (`0.2.1`) | Permanent |
| Nightly (`0.2.1-nightly.*`) | 30 days |
| Dev (`0.2.1-dev.*`) | 7 days |

Configure retention policies on the `mssql-rs/mssql-rs` Azure Artifacts feed to auto-clean old prerelease packages.
