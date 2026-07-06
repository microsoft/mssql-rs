# ARM Test Stages: On-Demand SQL Server Hosts

## Background

The two ARM64 Linux test stages — `Test_arm64` (glibc) and `Test_alpine_arm64`
(musl) — used to connect to a long-lived SQL Server hosted in Azure Container
Instances. The connection details came from pipeline variables
`$(ACI_SQL_HOST_2022)` and `$(ACI_SQL_PASSWORD)` on port `14333`. The reason
ACI was used: the official `mcr.microsoft.com/mssql/server` image is x64-only,
so ARM agents could not host SQL locally the way the amd64 stages already do
(see `test-matrix-template.yml` and `test-matrix-template-alpine.yml`).

This document describes the replacement design. The shared ACI SQL Server is
gone. Each affected stage now boots its own SQL Server in a docker container
on an x64 1ES agent for the duration of that stage and tears it down when the
ARM tests finish. No new Azure-service dependency is introduced.

## Goals

- Remove the standing dependency on the ACI-hosted SQL Server.
- Boot SQL on-demand inside an existing 1ES pool.
- Cross-pool reachability over the existing shared VNet (private IPv4); no
  public endpoint.
- Reliable teardown including cancellation and test-failure paths.
- A knob to choose between one SQL host per stage (default) and one SQL host
  per matrix entry, for cases where shared SQL state causes test flakes.

## Non-goals

- Rework of the amd64 ARM-stage flows (they already self-host SQL).
- Decommission of the ACI resource itself (operational task).
- Removal of the `ACI_SQL_*` pipeline variable definitions (they may still be
  referenced elsewhere).

## Architecture

For each ARM test stage:

```
Build (existing)                                                   completes
   |
   |                                                       x64 1ES agent
   +---> Sql_Host_<id>  --------------------------------------------+
   |        1. derive SA password from build context                |
   |        2. boot mcr.microsoft.com/mssql/server                  |
   |        3. probe readiness via sqlcmd                           |
   |        4. publish sql-ready-<id> artifact (contains endpoint)  |
   |        5. poll ADO REST API for teardown sentinels             |
   |        6. always() docker stop + rm                            |
   |                                                                |
   |                                                       ARM 1ES agent(s)
   +---> Test_*  ---------------------------------------------------+
            (no `dependsOn` on Sql_Host_<id> — runs concurrently)
            1. derive SA password from build context (same value)
            2. download Build_Linux_ARM artifact
            3. poll ADO REST API for sql-ready-<id> artifact
            4. download endpoint, set DB_HOST/DB_PORT
            5. run docker-cargo-run.sh against $DB_HOST:$DB_PORT
            6. publish junit
            7. always() PublishPipelineArtifact@1
                       artifact = <sentinel name>      ----> releases SQL host
```

### Why no `dependsOn` between SQL host and test job?

ADO will not start a `dependsOn`-linked job until its upstream job
*completes*, even when only output variables are referenced. If the test job
declared `dependsOn: Sql_Host_<id>`, the SQL host would have to finish before
the test job started. But the SQL host's job only finishes after the
teardown sentinels are published, which only happens once the tests
*run* — a textbook deadlock. In the original design this manifested as the
SQL host blocking until its `MAX_LIFETIME_MINUTES` cap fired (2 h by
default), at which point the always() teardown step ran, the SQL container
was destroyed, and only then was the test job released — so every test
attempted to connect to a SQL host that had just been torn down and timed
out at the connect.

The fix is symmetric pipeline-artifact sentinels in both directions, with no
ADO dependency between the jobs:

| Direction | Artifact name              | Producer side                       | Consumer side                                |
|-----------|----------------------------|-------------------------------------|----------------------------------------------|
| host→test | `sql-ready-<instanceId>`   | `PublishPipelineArtifact@1` after `start.sh` | `poll-for-endpoint.sh` at start of test job  |
| test→host | `<sentinelName>` (per matrix entry) | `PublishPipelineArtifact@1` always() at end of test job | `wait-for-teardown.sh` poll loop in SQL host |

Both jobs are queued at the start of the stage and run concurrently. The
test job's first SQL-touching step blocks on the `sql-ready` sentinel; the
SQL host's wait step blocks on the `tests-done` sentinels.

### Shared SA password

`generate-sql-password-template.yml` produces a job-scoped random password
and is fine for the amd64 stages where SQL Server and the test runner share
a job. For the cross-pool ARM design, the SQL host job and the test jobs
must agree on the password without sharing a secret across jobs (artifacts
are readable by anyone with build read access; we do not want to leak the
SA password through the `sql-ready` artifact).

`sql-host/derive-sql-password.sh` deterministically derives the SA password
from build context:

```
SQL_PASSWORD = "Aa1!" || sha256(Build.BuildId "-" System.CollectionId "-" salt)[0:22]
```

Both the SQL host job and the test jobs invoke the same script, so they
agree on the value without any cross-job transport. The `Aa1!` prefix
guarantees all four character classes the SQL Server SA policy requires.
The script uses the same `SQL_PASSWORD_GENERATED` marker as the random
template so the two are interchangeable.

### Shared TLS cert chain

`mssql.conf` sets `forceencryption = 1`, so SQL Server always presents a
cert during the TDS handshake. In the colocated amd64 design, a single
`scripts/generate_cert.sh` invocation produces the cert that both the SQL
container (mounted at `/etc/ssl/...`) and the test container (installed
via the dockerentry script's `update-ca-certificates`) trust. Connections
go to `sql1` (a docker DNS name in the shared docker network), which is
also a SAN on the cert.

The cross-pool design has two complications:

1. The SQL host and the test agent are different machines, so two
   independent runs of `generate_cert.sh` produce two unrelated CAs.
   The test container's trust store has no knowledge of the cert chain
   the SQL Server is actually presenting.
2. Test clients connect by IP (the SQL host's private VNet IP), not by
   DNS name. The default cert SANs (`sql1`, `localhost`, `127.0.0.1`,
   `::1`) don't cover this.

To fix both:

* `start.sh` resolves its own private IPv4 first, then invokes
  `generate_cert.sh` with `EXTRA_IP_SAN=<that IP>`. The generated cert
  carries the SQL host's IP as a SAN.
* `start.sh` stages `ca.crt` and `mssql.pem` into the `sql-ready` artifact
  alongside `endpoint.txt`. (The CA cert is public-key material; this
  doesn't widen the threat model.)
* `poll-for-endpoint.sh` extracts both files and copies them to
  `Build.SourcesDirectory`. The existing dockerentry scripts then pick
  them up via the `cp ca.crt /usr/local/share/ca-certificates;
  update-ca-certificates` step that already runs.
* The cross-pool test step skips the local `scripts/generate_cert.sh`
  call (it would overwrite the downloaded cert with a useless local one).

### Job topology by mode

| Mode (`sqlInstanceMode`) | SQL host jobs            | Test jobs                                  |
|--------------------------|--------------------------|--------------------------------------------|
| `shared` (default)       | 1 per stage              | 1 strategy-matrix job over all targets     |
| `per-job`                | 1 per matrix target      | 1 standalone job per matrix target         |

Both modes are produced from the same `targets` parameter list. Shared mode
reconstructs `strategy.matrix` from the list with `${{ each }}`; per-job
mode fans the list out into `(SQL host + test)` job pairs. The `sql-ready`
artifact name carries the instance id (`sql-ready-shared`,
`sql-ready-shared_musl`, or `sql-ready-<target.name>` / `_musl` for per-job)
so the test side polls for the right sentinel.

### Readiness signalling (start)

1. `start.sh` runs the SQL container with `-p ${SQL_HOST_PORT}:1433` (default
   `14333` to dodge NRMS baseline rules that single out 1433).
2. `start.sh` execs `sqlcmd -Q "SELECT 1"` inside the container in a retry
   loop (default deadline 180 s).
3. On success, `start.sh` writes `${BUILD_ARTIFACTSTAGINGDIRECTORY}/sql-endpoint-${INSTANCE_ID}/endpoint.txt`
   containing a single line `IP PORT DEADLINE_EPOCH`. The deadline is the
   wall-clock UTC epoch second past which `wait-for-teardown.sh` will give
   up and let `teardown.sh` destroy the container.
4. The next YAML step is a `PublishPipelineArtifact@1` that publishes that
   directory under the artifact name `sql-ready-${INSTANCE_ID}`.
5. The test job's `poll-for-endpoint.sh` polls
   `GET .../_apis/build/builds/{Build.BuildId}/artifacts?api-version=7.1`
   until that artifact appears, downloads its zip (with retry), extracts
   `endpoint.txt` using `python3 -m zipfile -e` (no `unzip` dependency),
   refuses to proceed if the embedded deadline leaves less than
   `MIN_HOST_LIFE_REMAINING_SECONDS` (default 5 min) of host life — that
   case usually means the SQL host hit its lifetime cap while the test
   job was queued — and otherwise sets pipeline variables `DB_HOST` and
   `DB_PORT`.

If the readiness probe fails, `start.sh` exits non-zero, the publish step
is skipped, and the test job's poll script will time out at its
`MAX_WAIT_SECONDS` cap (default 30 min) and fail with a clear error.

### Shutdown signalling (release)

Each test job has a final `condition: always()` step that publishes a
pipeline artifact whose name is the agreed-upon "teardown sentinel" for
that SQL instance. `always()` causes the publish step to run on success,
failure, and cancellation, so the SQL host is released regardless of test
outcome.

The SQL host job, after publishing the `sql-ready` artifact, runs
`wait-for-teardown.sh`, which polls

```
GET {System.CollectionUri}/{System.TeamProject}
    /_apis/build/builds/{Build.BuildId}/artifacts?api-version=7.1
```

every `POLL_INTERVAL_SECONDS` (default 15 s), authenticated with
`$(System.AccessToken)`. The watcher exits 0 when every name in
`EXPECTED_SENTINELS` is present in the build's artifact list.

`EXPECTED_SENTINELS` is computed at template-eval time:

- `shared` mode: comma-joined list of all target names (each matrix entry
  publishes its own sentinel; the SQL host waits for all of them).
- `per-job` mode: a single sentinel for the paired test job.

A `MAX_LIFETIME_SECONDS` cap (default 7200 = 120 min) ensures the watcher
returns even if a test agent goes offline before publishing its sentinel.
The ADO `timeoutInMinutes` for the SQL host job is a separate hard ceiling
so a misbehaving watcher cannot pin a 1ES VM.

### Sequence diagram (shared mode)

```
ARM job(s)                        Sql_Host (x64)                 ADO Pipelines
    | (start at the same time)         |                                |
    | derive SA password               | derive SA password             |
    | (download Build artifact)        | start.sh: docker run           |
    | poll-for-endpoint.sh: GET /artifacts                              |
    |  (artifact missing — backoff)    | start.sh: probe sqlcmd         |
    |                                  | start.sh: write endpoint.txt   |
    |                                  | PublishPipelineArtifact ------>|
    |                                  |   sql-ready-shared             |
    | poll-for-endpoint.sh: GET /artifacts (sees sql-ready-shared) <----|
    | download zip, set DB_HOST/DB_PORT                                 |
    | run tests against $DB_HOST:$DB_PORT                               |
    | always(): publish "Ubuntu22" artifact ---------------------------->|
    | always(): publish "Alpine3_18" artifact -------------------------->|
    |  ...                             | wait-for-teardown.sh: poll     |
    |                                  |   sees all expected sentinels  |
    |                                  | teardown.sh always()           |
    |                                  | docker stop / rm -------------->|
```

## Configuration surface

Pipeline parameters, defined on the entry pipelines
(`.pipeline/validation-pipeline.yml` for PR/dev and
`.pipeline/validation-pipeline-ci.yml` for GitHub `main`) and threaded through
`.pipeline/templates/validation-stages.yml`:

```yaml
- name: sqlInstanceMode
  type: string
  values: [shared, per-job]
  default: shared

- name: sqlImageTag
  type: string
  default: '2025-latest'
```

Both flow through to `templates/test-matrix-template-arm64.yml` and
`templates/test-matrix-template-alpine_arm64.yml`. `maxLifetimeMinutes`
defaults to 120 inside `templates/sql-host-template.yml`; promote it to a
pipeline parameter if a use case appears.

## PR vs merge coverage

Because the SQL host is now booted on-demand (no static ACI IP to contend
for), ARM64 integration tests can run on pull requests. Rather than a
separate ARM test stage, PR coverage is folded into the existing
`Build_Linux_ARM` job: it connects cross-pool to an on-demand SQL host booted
inside the **Build** stage (`Sql_Host_build_arm`), so the ARM64 build's own
test pass exercises the full integration suite instead of skipping it. The
full multi-distro ARM test matrices still run on merge.

| Coverage                          | Trigger        | Where                                       |
|-----------------------------------|----------------|---------------------------------------------|
| `Build_Linux_ARM` integration pass | PR             | Build stage, vs `Sql_Host_build_arm`        |
| `Test_arm64`                      | non-PR (merge) | full glibc matrix (7 distros)               |
| `Test_alpine_arm64`               | non-PR (merge) | full musl matrix (Alpine 3.18–3.21)         |

`Sql_Host_build_arm` is a PR-only instance of `sql-host-template.yml` added to
the Build stage (`jobCondition: eq(Build.Reason, 'PullRequest')`). It runs
concurrently with `Build_Linux_ARM` (no `dependsOn`) and the two rendezvous
via the same `sql-ready-build_arm` / `build_arm` sentinel pair used by the
test stages. On merge the host job is skipped and the ARM build's test pass
does not run (its steps are PR-gated), so no x64 agent is idled.

Python integration tests stay `--skip-integration` on ARM: the Python test
harness builds `Server={host}` with no port, so it cannot target the SQL
host's non-1433 port without a test-source change. The substantive ARM PR
coverage is the Rust integration suite.

## Files

| Path                                                          | Role                                                           |
|---------------------------------------------------------------|----------------------------------------------------------------|
| `.pipeline/templates/sql-host-template.yml`                   | Job template for the on-demand SQL host.                       |
| `.pipeline/scripts/sql-host/derive-sql-password.sh`           | Derive shared SA password from build context.                  |
| `.pipeline/scripts/sql-host/start.sh`                         | Boot SQL container, probe readiness, stage endpoint payload.   |
| `.pipeline/scripts/sql-host/poll-for-endpoint.sh`             | Test-side: poll for sql-ready artifact, set DB_HOST/DB_PORT.   |
| `.pipeline/scripts/sql-host/wait-for-teardown.sh`             | SQL-host-side: poll ADO REST for tests-done sentinels.         |
| `.pipeline/scripts/sql-host/teardown.sh`                      | Idempotent docker cleanup for `always()` step.                 |
| `.pipeline/templates/test-matrix-template-arm64.yml`          | Refactored; targets list + sqlInstanceMode; consumes endpoint. |
| `.pipeline/templates/test-matrix-template-alpine_arm64.yml`   | Same refactor for the musl/alpine flow.                        |
| `.pipeline/templates/validation-stages.yml`                   | Converts ARM stage matrices into `targets` lists; threads `sqlInstanceMode` / `sqlImageTag`; adds PR-only `Sql_Host_build_arm` to the Build stage. |
| `.pipeline/templates/build-template-container.yml`           | ARM64 branch: derive password, poll the cross-pool endpoint, run the integration suite against it, publish the `build_arm` teardown sentinel. x64 path unchanged. |
| `.pipeline/validation-pipeline.yml`, `.pipeline/validation-pipeline-ci.yml` | Add `sqlInstanceMode` and `sqlImageTag` parameters and pass them to `validation-stages.yml`. |

The amd64 templates and `sql-setup-template.yml` are intentionally left
unchanged.

## Reliability properties

- **No deadlock.** Symmetric pipeline-artifact sentinels with no ADO
  `dependsOn` between SQL host and test jobs. Either side can be late and
  the other side polls until it appears.
- **Stale-endpoint guard.** The `sql-ready` artifact carries the SQL
  host's lifetime deadline. A late-queued test job that finds an
  about-to-expire endpoint fails fast rather than connecting to a host
  that is about to be torn down (the failure mode of the original design).
- **Readiness gate is on the test side.** The test job's first SQL-touching
  step blocks until the `sql-ready` artifact is published (which only
  happens after `start.sh`'s sqlcmd probe succeeds).
- **Teardown signal is `always()` on the test side.** Cancellation and
  failure paths still publish the sentinel.
- **Cleanup is `always()` on the SQL side and idempotent.** Cleans up even
  if the watcher hits the timeout cap or fails internally.
- **Hard timeout.** `MAX_LIFETIME_SECONDS` plus the ADO job-level
  `timeoutInMinutes` together cap the SQL host's lifetime.
- **No new Azure resources.** Uses pipeline artifacts, the ADO REST API,
  `System.AccessToken`, the existing 1ES pools, and the existing
  `mcr.microsoft.com/mssql/server` image.

## Open assumptions

- Cross-pool reachability between `RUST-1ES-POOL-WUS3` (x64) and
  `RUST-1ES-POOL-ARM-WUS3` (ARM) on the SQL host's private IPv4 and the
  published port. Confirmed at design time; a connectivity regression here
  would surface as `poll-for-endpoint.sh` succeeds (the test job sees the
  endpoint) but the subsequent test connect times out.
- `mcr.microsoft.com/mssql/server:<sqlImageTag>` ships
  `/opt/mssql-tools18/bin/sqlcmd`, which `start.sh` uses for the readiness
  probe. The current default tag (`2025-latest`) does.
- The ubuntu image used by the x64 SQL host (`RUST-1ES-UBUSLIM`) provides
  `curl` and `docker`. The latter is enforced by `DockerInstaller@0` and
  `install-ubuntu-dependency.yaml`; the former is standard in the image.
- Both ARM and x64 1ES pool agents have `python3` available (used by
  `poll-for-endpoint.sh` for JSON parsing). Standard in the Ubuntu images.

## Operational notes

- To diagnose a stuck SQL host, look at the `Wait for test-completion
  sentinels` step's logs — it prints the missing sentinel count every poll.
  Cross-reference against the test job's `Publish teardown sentinel` step
  to see whether a particular matrix entry never published.
- To diagnose a stuck test job, look at the `Wait for SQL host endpoint
  sentinel` step — it prints remaining wait time every poll. If it never
  finds the sentinel, check whether the SQL host job published it (look at
  the SQL host's own timeline).
- To force shutdown of a stuck SQL host without cancelling the build,
  manually publish a pipeline artifact with the missing sentinel name from
  any other job in the same build.
- To switch a build to per-job SQL isolation, queue with
  `sqlInstanceMode = per-job`. Cost: N additional x64 agents per stage
  (where N is matrix size).
