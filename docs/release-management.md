# mssql-python-rs Release Management

How changes in `mssql-rs` (Rust) flow to `mssql-python` (Python) through the
wheel build, internal Azure Artifacts PyPI feed, and public PyPI release pipelines.

## Architecture

```
mssql-rs repo (Rust)
├── mssql-tds/          ← Core TDS protocol crate
├── mssql-py-core/      ← PyO3 bindings (cdylib), produces Python wheels
└── .pipeline/OneBranch/ ← Builds wheels, uploads them with twine

        │  builds 9 stable-ABI wheels (one per supported platform)
        │  twine-uploads them as mssql-python-rs
        ▼

Azure Artifacts feed: public/mssql-rs_Public (PyPI index)
  │  dev, nightly, and release wheels for pre-release CI validation
        ▼

PyPI: mssql-python-rs
  │  standalone runtime dependency
  │  exact version pinned by mssql-python
        ▼

PyPI: mssql-python
```

`mssql-python` does not embed or repackage `mssql_py_core`. Its build and test
pipelines still download the wheel from the legacy NuGet package. Migrating that
installer to `pip install` an exact version from the Azure Artifacts feed is a
separate follow-up in the `mssql-python` repository. Public users install
`mssql-python-rs` from PyPI through the exact runtime dependency declared by
`mssql-python`.

Feed simple index (anonymous read):
`https://pkgs.dev.azure.com/sqlclientdrivers/public/_packaging/mssql-rs_Public/pypi/simple/`

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

### Scheduled version bumps

The **Bump Released Versions** Azure DevOps pipeline
(`.pipeline/bump-released-versions.yml`) opens version bump pull requests against
GitHub `microsoft/mssql-rs` on the first and third Tuesday of each month at
07:30 UTC (13:00 IST). The cron trigger (`30 7 * * 2`) fires every Tuesday and
the job then checks the UTC day of month: days 1-7 hold exactly one Tuesday, as
do days 15-21. The ordinal guard lives in the job rather than in the cron
day-of-month field because cron implementations disagree on whether a restricted
day of month and day of week combine with AND or OR. Azure Pipelines may delay or
skip scheduled runs. Running the pipeline manually bypasses the date check.

A scheduled trigger defined in the pipeline settings UI silently overrides the
YAML schedule, so do not add one.

The pipeline runs two independent tracks. A failure in one does not stop the
other; the run fails at the end if either track failed.

#### Released Rust crates

`mssql-tds` and `mssql-mock-tds` are checked against crates.io independently. If
a crate's exact `[package].version` exists among its published versions
(including older or yanked releases), the pipeline bumps it to the next minor
version: `0.1.7` becomes `0.2.0`. Unpublished source versions and crates that
return HTTP 404 are left alone. Other registry errors fail the track rather than
assuming a crate is unpublished. If the proposed next minor version is also
published, the track fails and a maintainer must choose a new version. Before any
bump is planned, the track also fails when the current `mssql-tds` and
`mssql-mock-tds` source versions differ in either direction (`mssql-tds` and
`mssql-mock-tds` must stay aligned for a release), because silently tolerating a
drifted state can leave the repo in a release-invalid configuration. If the mock
source version is ahead of the core target, the track also fails so a maintainer
can choose the versions manually. A mock-only bump is deferred until the current
`mssql-tds` version is published, because the mock crate's exact versioned
dependency must resolve from the registry when it is packaged. When `mssql-tds`
is bumped, the mock source version and the mock crate's versioned `mssql-tds`
requirement are kept aligned with the core target because the release pipeline
stamps the mock crate from the core version.

Local versions come from `cargo metadata`. `Cargo.lock` is not tracked, and
`--no-deps` means no lockfile is resolved or committed.

#### `mssql-python-rs`

The `[project].version` in `mssql-py-core/pyproject.toml` is checked against
PyPI on the same schedule and with the same rule: if the current version is
already published, bump to the next minor version. The independently versioned
`mssql-py-core` Rust crate is never touched.

#### Issues, branches, and pull requests

Each track creates or updates one tracking issue, then pushes a branch from the
default branch tip and opens a pull request that closes it:

| Track | Issue label | Branch |
| --- | --- | --- |
| Crates | `automation:crate-version-bump` | `automation/bump-released-crates` |
| `mssql-python-rs` | `automation:python-version-bump` | `automation/bump-mssql-python-rs` |

The issue exists because **PR Linked Issue Check** requires every pull request to
reference a GitHub issue or an ADO work item; the generated PR body carries
`Fixes #<issue-number>`. The pipeline queries only open issues with the track's
label, ignores pull requests returned by the same endpoint, then checks the
hidden marker in their bodies before reusing or updating one. It regenerates the
issue body while the bump is pending; put maintainer notes in issue comments. The
label is created automatically when needed; multiple matching open issues fail
the track for manual cleanup.

To release a different version, push to the open pull request. The pipeline
force-updates its branch only while the planned versions change, and it skips a
track entirely when a pull request on some *other* branch already proposes the
same transitions. When no bump is needed, nothing is written. If a bump becomes
unnecessary, close the pull request and its issue manually.

Branch replacement uses `--force-with-lease` against the exact commit inspected
by the job. A missing branch is confirmed with `git ls-remote`; network and
server failures abort rather than being treated as an absent automation branch.
This prevents a maintainer or overlapping run from being overwritten between
inspection and push.

Normal validation and review are still required before merging. The pipeline
does not publish crates, merge anything, or change the release pipeline's
existing version-stamping policy.

#### Credentials

No PAT, secret, or variable group is involved. The pipeline's source repository
is GitHub `microsoft/mssql-rs` through the `github.com_microsoft` service
connection, so `checkout: self` with `persistCredentials: true` leaves the Azure
Pipelines GitHub App installation token in the local git config under a
repo-scoped key such as
`http.https://github.com/microsoft/mssql-rs.extraheader`. The job discovers that
key, decodes the basic-auth header, and registers the token as a secret variable
so it is masked in every downstream log. The same token authenticates both the
branch push and the GitHub REST calls.

That installation token is scoped to the whole `microsoft` organization, not to
this repository alone. It is short-lived and the pipeline only ever writes to
this repo, but any step in this job briefly holds a broadly scoped credential.
A dedicated GitHub App scoped to `mssql-rs` would narrow that, at the cost of
org-admin setup and two new secrets.

GitHub is reached over the REST API with `urllib` rather than the `gh` CLI, so
the job does not depend on `gh` being present on the agent image.

If the credential cannot be found or decoded, the step fails before any branch is
pushed.

#### Creating the pipeline

The YAML alone does nothing until a pipeline definition points at it:

1. Create a new pipeline in the `mssql-rs` Azure DevOps project from the GitHub
   `microsoft/mssql-rs` repository using the `github.com_microsoft` service
   connection, with YAML path `.pipeline/bump-released-versions.yml`.
2. Add an **Exclusive lock** check to the
   `mssql-rs-version-bump-writer` environment. The YAML's sequential lock
   behavior then queues overlapping manual and scheduled runs.
3. Add an ADO **Branch control** check to the environment allowing only
   `refs/heads/main`. The YAML also rejects the deployment stage on any other
   source ref.
4. Do not add a schedule in the pipeline settings UI.
5. Run it manually once from `main` to confirm the credential extraction works.

The pipeline runs on a Microsoft-hosted `ubuntu-latest` agent. It only runs git
and Python, so it does not need the private Rust pools, which require a separate
pool authorization for any new pipeline.

The shared validation pipeline runs `scripts/test_bump_released_versions.py` in
its Windows Python test step for both PR validation and main-branch CI. Cargo
metadata, registry, git, pull request, and issue calls are mocked. The cadence
guard and the credential-key extraction are executed as real `bash`, so the
shipped shell is covered rather than a Python restatement of it.

### Updating `microsoft/mssql-python` after a PyPI release

Pipeline 2332 (`PyPI-Release`) queues the **Bump mssql-python Release Version**
pipeline only after ESRP successfully publishes the selected official wheels to
PyPI. The release pipeline passes the published distribution version and its
build ID to the worker; dry runs do not queue it.

The worker is GitHub-sourced so it can reuse the same short-lived
`github.com_microsoft` installation token described above. It verifies the exact
version on PyPI, clones `microsoft/mssql-python`, and opens a draft pull request
against `main`. The pull request always advances
`eng/versions/mssql-python-rs.version` when the repository is behind the
published release.

This pipeline no longer publishes the `mssql-python-rs-wheels` NuGet package.
Until the `mssql-python` installer migration lands, its NuGet pin remains the
consumer-facing pin and cannot advance to feed-only releases. The release worker
still checks for a legacy NuGet package so it can handle older or separately
published packages; a missing package leaves the NuGet pin unchanged:

- when it exists, `eng/versions/mssql-python-rs-nuget.version` is advanced too;
- when it does not exist (including a 404 after NuGet retention), the NuGet pin
  is left unchanged and the worker still advances the public PyPI pin;
- prerelease `-dev` and `-nightly` packages never satisfy the check.

The separate installer migration must also switch the release worker to check
the Azure Artifacts PyPI feed and advance the pin consumed by `mssql-python`.
Until that follow-up lands, the worker can update the public PyPI pin, but
`mssql-python` continues using the legacy NuGet pin. Older release events never
downgrade either pin on `main` or on the pending automation branch. An unrelated
`main` change does not rebase a pending PR when its two managed pins already have
the requested values. A branch containing maintainer commits is left untouched,
and all automation-owned replacements use an exact `--force-with-lease`.

If a newer PyPI release arrives while the pending automation PR already carries
a higher legacy NuGet pin from an earlier run, the worker advances the PyPI pin
and retains that higher pending NuGet pin. The regenerated issue and PR summaries
show both transitions relative to `main`; a PyPI-only event never rolls back an
already verified NuGet package.

This YAML also requires a permanent pipeline definition after the change reaches
`main`:

1. Create a GitHub-backed pipeline named **Bump mssql-python Release Version**
   from `microsoft/mssql-rs` with the `github.com_microsoft` service connection.
2. Set its YAML path to `.pipeline/bump-mssql-python-release.yml`.
3. Add an **Exclusive lock** check to the
   `mssql-python-release-bump-writer` environment.
4. Add an ADO **Branch control** check to the environment allowing only
   `refs/heads/main`. Pipeline 2332 explicitly queues the worker from that ref,
   and the worker YAML rejects any other source ref.
5. Run it manually from `main` with a published version to verify access to
   `microsoft/mssql-python`.

The exact pipeline name is part of the release contract: pipeline 2332 resolves
that name through the Azure DevOps API before queueing the worker.

### Python distribution versions

Wheels on the feed carry the `mssql-python-rs` distribution version from
`mssql-py-core/pyproject.toml`. The Rust crate has an independent version in
`mssql-py-core/Cargo.toml`, so `mssql_python_rs-0.3.0-*.whl` can be built from a
crate with a different version.

The PyPI feed stores each distribution under the version baked into the wheel and
never accepts the same file twice, so non-official builds stamp a unique PEP 440
development version into `[project].version` before building (`Cargo.toml` is not
touched). A `ComputeWheelVersion` job resolves it once per run so every platform
wheel matches:

| Build Type | Version Format | Example |
|---|---|---|
| **Nightly** (scheduled) | `{version}.dev{YYYYMMDD}` | `0.3.0.dev20260217` |
| **Dev** (CI push / manual non-official) | `{version}.dev{YYYYMMDD}{BuildId}` | `0.3.0.dev20260217140071` |
| **Release** (official) | `{version}` (the committed value; stamping it is a no-op) | `0.3.0` |
| **PR** | Wheels built for validation only — **not published** | — |

Dates are UTC. Always pin an exact version (`==`) when consuming feed builds:

- Every `.devN` sorts *below* its base release, so `0.3.0.dev…` is older than
  `0.3.0`. Once a base version is released, dev and nightly builds stay below it
  until `[project].version` is bumped.
- The dev number embeds the build ID, so any dev build sorts above every nightly
  with the same base version. "Latest" pre-release resolution (`pip install --pre`)
  is therefore not meaningful across the two build types.

Earlier pre-release builds were published as the NuGet package
`mssql-python-rs-wheels` (`{version}-dev.*` / `{version}-nightly.*`). That package
is no longer produced.

---

## Scenario 1: Nightly Builds

**Purpose**: Produce a daily "latest from main" package that `mssql-python` CI can consume for continuous integration testing.

**What triggers it**: Scheduled cron (`0 2 * * *` UTC) on the `main` branch.

### Flow

```
1. Schedule triggers at 2 AM UTC
2. ComputeWheelVersion resolves {version}.devYYYYMMDD (e.g., 0.3.0.dev20260217)
   from [project].version in mssql-py-core/pyproject.toml
3. Every build job stamps that version and builds 9 stable-ABI wheels
4. Publish stage:
   - Validates wheel names, metadata, and ODBC payloads against the stamped version
   - twine-uploads the wheels to the public/mssql-rs_Public feed's PyPI index
5. The feed contains the new nightly version. `mssql-python` CI continues using
   its legacy NuGet pin until the installer migration follow-up lands.
```

### mssql-python consumer migration

After the separate installer migration in the `mssql-python` repository,
`mssql-python` can install an exact nightly from the feed:

```
python -m pip install \
  --index-url https://pkgs.dev.azure.com/sqlclientdrivers/public/_packaging/mssql-rs_Public/pypi/simple/ \
  mssql-python-rs==0.3.0.dev20260217
```

The migration must also update the release-bump worker to check the feed and
advance the pin consumed by `mssql-python`.

### Key properties

- One nightly per day. Retrying a failed Publish stage re-uploads only the wheels
  that are missing from the feed; files already published with the same SHA-256
  are skipped, and a published file with a different SHA-256 fails the upload.
- Always builds from `main` — represents the latest merged state
- If `main` has no changes, nightly still runs (`always: true`) to confirm nothing is broken

---

## Scenario 2: Dev Builds (Testing Changes Faster in mssql-python)

**Purpose**: When a developer makes a change in `mssql-rs` and wants to test it in `mssql-python` *before* waiting for the nightly.

### Flow: Push to main/development

```
1. Developer merges PR to main or development
2. CI trigger fires immediately
3. Pipeline stamps {version}.devYYYYMMDD{BuildId} and builds 9 stable-ABI wheels
4. Publish stage uploads: mssql-python-rs==0.3.0.dev20260217140071
   (BuildId ensures uniqueness even with multiple merges per day)
5. Developer tells mssql-python to use this specific version
```

### Flow: Manual trigger for ad-hoc testing

```
1. Developer triggers pipeline manually from any branch
2. Pipeline stamps {version}.devYYYYMMDD{BuildId} and builds 9 stable-ABI wheels
3. Publish stage uploads: mssql-python-rs==0.3.0.dev20260217140095
4. Developer uses this version in mssql-python for testing
```

### How to test a specific dev build in mssql-python

1. Note the version from the pipeline's `Compute wheel version` job or the feed
   (e.g., `0.3.0.dev20260217140071`)
2. In the `mssql-python` build pipeline or local dev setup:

```powershell
python -m pip install `
  --index-url https://pkgs.dev.azure.com/sqlclientdrivers/public/_packaging/mssql-rs_Public/pypi/simple/ `
  mssql-python-rs==0.3.0.dev20260217140071
# Run mssql-python tests against the installed runtime
```

3. Once validated, the change flows to nightlies automatically after merge to `main`

### Key properties

- Every push to `main`/`development` produces a unique dev version
- BuildId guarantees no version collisions
- Pin exact versions; see [Python distribution versions](#python-distribution-versions)
  for how dev and nightly versions sort
- PR builds do NOT publish — they only validate that wheels compile

---

## Scenario 3: Upgrading mssql-python to a New Native Package Version

**Purpose**: When `mssql-python` needs to adopt a new `mssql-python-rs` distribution version (e.g., new features or bug fixes).

### Steps in mssql-rs

1. **Make changes** to `mssql-tds` and/or `mssql-py-core`
2. **Bump `[project].version`** in `mssql-py-core/pyproject.toml` when the Python distribution version changes
   - This version controls the wheel filenames and the feed/PyPI distribution version
3. **Manage Rust crate versions independently**
   - Bump `[package].version` in a crate's `Cargo.toml` only when that Rust crate version changes
   - A Rust crate bump does not change the Python wheel version
4. **Merge PR** — CI uploads `mssql-python-rs==0.2.1.devYYYYMMDD{BuildId}`
5. **Nightly** uploads `mssql-python-rs==0.2.1.devYYYYMMDD`

### Steps in mssql-python

1. **Update the pinned version** installed from the feed:

```yaml
# In mssql-python's build pipeline
- script: >
    python -m pip install
    --index-url https://pkgs.dev.azure.com/sqlclientdrivers/public/_packaging/mssql-rs_Public/pypi/simple/
    mssql-python-rs==0.2.1.dev20260303
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
  → merges → dev build: 0.2.1.dev2026030311001
  → nightly: 0.2.1.dev20260303
  mssql-python: tests against 0.2.1.dev20260303 ✅

Week 2:
  mssql-rs PR #105: Add retry logic (Python distribution stays 0.2.1)
  → merges → dev build: 0.2.1.dev2026031011042
  → nightly: 0.2.1.dev20260310
  mssql-python: tests against 0.2.1.dev20260310 ✅

Sprint end:
  mssql-rs: Official release → mssql-python-rs==0.2.1 (clean)
  mssql-python: pins to 0.2.1 release, does its own release
```

---

## Scenario 4: Release Activities

**Purpose**: Produce a production-quality, signed, immutable release of the `mssql-python-rs` wheels.

### Pre-release checklist (mssql-rs)

- [ ] All PRs for the sprint are merged to `main`
- [ ] Latest nightly (`0.2.1.devYYYYMMDD`) is passing in `mssql-python` CI
- [ ] `[project].version` in `mssql-py-core/pyproject.toml` is correct (e.g., `0.2.1`)
- [ ] Any Rust crate selected for release has the intended independent version in its `Cargo.toml`
- [ ] No outstanding breaking changes without coordination

### Release build

1. **Run the Official Python Wheels Build** for the source commit to release
2. **Trigger the Official release pipeline** with that build selected and `publishToFeed: true`
   - This uploads the clean-semver wheels `mssql-python-rs==0.2.1` to the Azure Artifacts PyPI feed via `twine`
   - OneBranch runs full SDL scanning (BinSkim, Clippy, AV)
   - Wheels are published to the `mssql-rs_Public` feed's PyPI index

### Optional mssql-py-core source tag

The release pipeline's `tagRelease` option is separate from feed publication. It reads
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

Do not derive this tag or branch from the Python distribution version when the versions differ.

### Post-release in mssql-python

1. Pin the clean release version from the feed: `mssql-python-rs==0.2.1`
2. Run full test suite
3. Update `mssql-python` version (e.g., bump to `1.4.0`)
4. Publish to PyPI

### Releasing existing Official Build artifacts

`.pipeline/OneBranch/OfficialPythonWheelsRelease.yml` consumes the selected
`Official Python Wheels Build` run without rebuilding it. All release switches
default to `false`. Official wheel artifacts are always downloaded and validated
against that build's source metadata, even when feed publishing is disabled.

| Switch | Behavior |
|---|---|
| `publishToFeed` | Collect, authenticate and `twine upload` the validated wheels to the Azure Artifacts PyPI feed after wheel validation. When false, these upload-only steps are omitted; wheel download and validation still run. |
| `publishMssqlTds` | Select `mssql-tds` for crates.io publication through ESRP. |
| `publishMssqlMockTds` | Select `mssql-mock-tds` for crates.io publication through ESRP. |
| `validateCratesOnly` | Validate the crate artifact and run selected-crate preflights without ESRP publication. Can also validate the artifact with neither crate selected. |
| `tagRelease` | Tag and branch the selected build's `mssql-py-core` version. This does not tag Rust crate versions. |

The wheel validation/feed publish and Rust crate stages have no dependency on each other.
The crate stage downloads only crate artifacts and does not wait for wheel
validation or feed publish. A failure in either wheel validation or feed publish does not block
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

Tagging always waits for successful wheel validation, plus feed publication when
`publishToFeed` is selected. It never waits for Rust crate publication. Leaving all
switches off is a wheel validation-only run: no wheel upload or publication,
ESRP, Git tags, or release-branch writes.

Feed upload and tag metadata come from the selected build's exact source commit, not
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
`publishMssqlTds` and/or `publishMssqlMockTds`; leave `publishToFeed` and `tagRelease`
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
5. Trigger the Official release pipeline with `publishToFeed: true`
   - This publishes `mssql-python-rs==0.2.2` wheels to the Azure Artifacts PyPI feed
6. Enable `tagRelease` only when creating the separate Cargo-versioned mssql-py-core tag and release branch
7. Update `mssql-python` to use `mssql-python-rs==0.2.2`

---

## Pipeline Files Reference

| File | Purpose |
|---|---|
| `.pipeline/OneBranch/NonOfficialPythonWheelsPublish.yml` | Pipeline entry point (NonOfficial) — triggers, schedule, feed variables; `nugetPublishing` config for the `mssql-sqlcmd` NuGet package |
| `.pipeline/OneBranch/stages.yml` | Build + Publish stages — wheel version computation, build jobs, twine upload of wheels, `mssql-sqlcmd` NuGet publish |
| `.pipeline/OneBranch/OfficialPythonWheelsRelease.yml` | Independent opt-in wheel feed upload and Rust crate releases from an existing Official Build, plus optional `mssql-py-core` tagging |
| `.pipeline/scripts/compute-python-wheel-version.ps1` | Resolves the run's PEP 440 wheel version once |
| `.pipeline/scripts/stamp-python-wheel-version.ps1` / `.sh` | Stamps that version into `[project].version` of `pyproject.toml` before building |
| `.pipeline/scripts/twine-upload.ps1` | Uploads wheels missing from the feed; skips identical published files and rejects mismatched ones |
| `.pipeline/templates/build-python-wheels-template.yml` | Shared wheel build template (manylinux, musllinux, Windows, macOS) |
| `.pipeline/templates/install-dependencies.yml` | Dependency installation (Rust, Python, etc.) |
| `.pipeline/templates/cargo-authenticate-template.yml` | Cargo registry authentication |
| `.pipeline/validation-pipeline.yml` | CI/PR validation pipeline (non-OneBranch) |

## Feed Package Layout

Each build adds one version of `mssql-python-rs` to the feed's PyPI index, with one
wheel per platform:

```
mssql-python-rs 0.3.0.dev20260217140071
├── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-win_amd64.whl
├── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-win_arm64.whl
├── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-manylinux_2_28_x86_64.whl
├── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-manylinux_2_28_aarch64.whl
├── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-manylinux_2_34_x86_64.whl
├── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-manylinux_2_34_aarch64.whl
├── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-musllinux_1_2_x86_64.whl
├── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-musllinux_1_2_aarch64.whl
└── mssql_python_rs-0.3.0.dev20260217140071-cp310-abi3-macosx_15_0_universal2.whl
```

## Traceability

The version identifies the producing pipeline run:

- **Dev**: the trailing digits after the date are the Azure DevOps `BuildId`
  (`0.3.0.dev20260217140071` → build `140071`), whose run records the source commit.
- **Nightly**: the date identifies that night's scheduled run of `main`.
- **Release**: the Official release pipeline run records the selected Official
  Build run ID and its source commit.

When `tagRelease` is enabled, the separate Cargo-versioned tag and release branch provide
additional source traceability for `mssql-py-core`; a feed release does not require that tag.

## Feed Retention Guidelines

| Package Type | Suggested Retention |
|---|---|
| Release (`0.2.1`) | Permanent |
| Nightly (`0.2.1.devYYYYMMDD`) | 30 days |
| Dev (`0.2.1.devYYYYMMDD{BuildId}`) | 7 days |

Configure retention policies on the `public/mssql-rs_Public` Azure Artifacts feed to auto-clean old pre-release versions.
