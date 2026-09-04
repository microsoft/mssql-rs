# SQL Server Configuration Scripts

This directory contains scripts used by the Azure DevOps pipeline to configure and host SQL Server instances for tests.

## sql-host/ — On-demand SQL Server for ARM test stages

Boots a SQL Server docker container on an x64 1ES agent so ARM test jobs can
run against a real SQL Server without depending on a static (ACI) IP address.
The SQL host job and the ARM test jobs run concurrently and rendezvous through
pipeline-artifact sentinels; the SA password is derived deterministically from
the build context so no secret is transported between jobs.

- **start.sh** — Receives the SA password (derived by `derive-sql-password.sh`
  in the YAML templates), generates certs (adding the host's private VNet IPv4
  as an extra SAN via `EXTRA_IP_SAN`), starts the SQL container, and publishes
  the `sql-ready-<instanceId>` sentinel carrying the endpoint plus
  `ca.crt`/`mssql.pem`.
- **wait-for-teardown.sh** — Polls for the teardown sentinel artifacts (named
  by the raw sentinel names the test jobs publish) and releases the SQL host
  once every expected sentinel has been published.
- **teardown.sh** — Stops and removes the SQL container and network.

See `.pipeline/docs/arm-sql-host-design.md` for the full design.

## Scripts

### Generate-SqlCertificates.ps1
Generates and installs self-signed certificates for SQL Server TLS encryption.

**Parameters:**
- `InstanceName` (optional): SQL Server instance name (default: "MSSQLSERVER")

**Usage:**
```powershell
.\Generate-SqlCertificates.ps1
.\Generate-SqlCertificates.ps1 -InstanceName "SQLDEV"
```

**What it does:**
- Creates a self-signed SSL certificate for SQL Server
- Configures certificate permissions for the SQL service account
- Installs the certificate in the SQL Server registry configuration
- Copies the certificate to the trusted root store
- Restarts SQL Server service to apply changes

### Enable-SqlBrowser.ps1
Enables and starts the SQL Server Browser service.

**Parameters:**
- `ServiceName` (optional): Name of the SQL Browser service (default: "SQLBrowser")

**Usage:**
```powershell
.\Enable-SqlBrowser.ps1
.\Enable-SqlBrowser.ps1 -ServiceName "SQLBrowser"
```

**What it does:**
- Checks if SQL Browser service exists
- Sets startup type to Automatic
- Starts the service if not running
- Provides detailed status information

### Enable-SqlProtocols.ps1
Enables Named Pipes and Shared Memory protocols for SQL Server via registry modification.

**Parameters:**
- `InstanceName` (optional): SQL Server instance name (default: "MSSQLSERVER")
- `SqlVersion` (optional): SQL Server version prefix (default: "MSSQL17")
- `RestartService` (optional): Whether to restart SQL Server service (default: $true)

**Usage:**
```powershell
.\Enable-SqlProtocols.ps1
.\Enable-SqlProtocols.ps1 -InstanceName "SQLDEV" -SqlVersion "MSSQL17" -RestartService $true
```

**What it does:**
- Enables Named Pipes protocol via registry
- Enables Shared Memory protocol via registry
- Optionally restarts SQL Server service to apply changes
- Provides detailed configuration status

### run-bounded.sh
Sourced helper providing `run_bounded <seconds> <command...>`. macOS ships no
coreutils `timeout`, so long-running commands are bounded by running them in the
background and killing them on overrun. Returns 124 on timeout.

### start-colima-macos.sh
Installs Docker + Colima on a hosted macOS agent and boots the VM, retrying on
the transient lima hostagent boot failures seen in ~3% of runs. VM size stays at
the long-standing 4 GiB / 4 CPU.

The docker CLI is installed with `brew install --force-bottle`, which uses
Homebrew's bottle when one exists for the platform and *fails* rather than
falling back to a source build. When it fails, `install-brew-bottle.py` installs
the newest version that *is* bottled for this platform, taken straight from
Homebrew's own registry. As of 29.8.0 there is no Intel macOS bottle, so a bare
`brew install docker` compiles the CLI and builds Go to do it: measured over 147
runs, the bottled path took 29s median and failed 1% of the time, the
source-build path took 441s median (774s max) and failed 36%. `colima` and
`lima` are still bottled on Intel and install normally.

The whole install phase (`brew update`, `colima`, the docker CLI) is bounded by
`INSTALL_TIMEOUT_SECONDS` so a slow install fails here with a message rather than
surfacing as an opaque step timeout. Each `colima start` is bounded by
`COLIMA_START_TIMEOUT_SECONDS` so a wedged boot still reaches the delete/retry
path rather than running until the pipeline step timeout. That bound sits above
the slowest healthy boot observed (509s over 113 runs in 2026-08; re-measured
2026-09 at max 453s over 123 runs) — the real failures give up within seconds, so
a shorter bound would only kill slow-but-healthy boots. `COLIMA_BUDGET_SECONDS`
then caps the retries as a whole.

**Environment overrides:** `COLIMA_CPU`, `COLIMA_MEMORY`, `COLIMA_DISK`,
`COLIMA_START_ATTEMPTS` (3), `COLIMA_START_TIMEOUT_SECONDS` (540),
`COLIMA_BUDGET_SECONDS` (480), `INSTALL_TIMEOUT_SECONDS` (300),
`DOCKER_CLI_DIR`.

### build-macos-docker-toolchain.py
Assembles the macOS docker toolchain payload for one architecture: the install
prefixes of the `docker`, `colima` and `lima` Homebrew bottles, plus the Ubuntu
guest disk image colima boots, plus a `manifest.json` recording versions, source
URLs and digests. Published as a Universal Package by
`.pipeline/macos-docker-toolchain-pipeline.yml`, so macOS jobs install the
toolchain from our own feed rather than reaching Homebrew, ghcr.io and
github.com at job time.

The whole prefix is packaged, not just `bin/`: `limactl` resolves
`../share/lima/lima-guestagent.*` and `../libexec/lima/*` relative to itself, so
a bin-only payload yields a lima that cannot boot a VM. The build fails if the
guest agent is missing rather than shipping one that would.

The guest image is not hard-coded. colima embeds a table of
`<arch> <runtime> <url> <sha512> <filename>` for the image release it expects, so
both the image and the checksum to verify it against are read out of the binary
being packaged — colima 0.10.3 wants colima-core v0.10.4, which a hand-maintained
mapping would get wrong.

Runs on Linux and cross-builds both macOS payloads, so it verifies the Mach-O
architecture of every binary it packs; a mis-resolved bottle fails the build
instead of shipping.

**Usage:** `build-macos-docker-toolchain.py --arch x86_64|arm64 --out <dir>`
`--macos-major` sets the oldest macOS the payload must run on (default 14);
bottles built for an older macOS run on newer hosts, so this is a compatibility
floor rather than a target.

### install-brew-bottle.py
Installs the newest Homebrew bottle of a formula that exists for the running
platform, by reading Homebrew's OCI registry on ghcr.io directly. Used as the
docker CLI fallback above, and generic enough to cover `colima` or `lima` if they
lose their Intel bottles too.

Bottles are content-addressed, so the download is verified against the digest the
registry advertises rather than a checksum vendored here. Bottles built for an
older macOS are accepted (they run on newer hosts) but never a newer one; a macOS
major the script doesn't recognize is an error rather than a guess, because
guessing means handing the agent a binary it can't run. Formulas bottled `:all`
are not selected — the CLIs installed here aren't, and taking one blindly is
worse than reporting no match.

Homebrew spells a formula revision two ways: the registry tag is `29.7.2-1` but
the ref names inside it read `29.7.2.<platform>.1`, so the two are matched
separately. Only the newest `MAX_VERSIONS_SCANNED` (15) versions are examined —
each costs a registry round-trip, and this keeps a no-match failure from eating
the caller's install budget before it reports.

**Usage:** `install-brew-bottle.py <formula> <dest-bin-dir>`; prints the resolved
version. `extract_bin` flattens just the executables (what the docker CLI
fallback needs); `extract_prefix` keeps the whole install tree (what a packaged
lima needs). `BOTTLE_ARCH_OVERRIDE` and `BOTTLE_MACOS_MAJOR_OVERRIDE` select a
platform other than the running one — used by the toolchain producer to
cross-build both macOS payloads from Linux, and handy for testing.
Covered by `test_install_brew_bottle.py`.

### start-sql-server-macos.sh
Starts the SQL Server test container inside the Colima VM. Retries the image
pull, recreates the container when SQL Server hits a SQLPAL startup crash
(~13% of macOS runs), dumps container state plus logs on every failed attempt,
and exits non-zero when the server never becomes reachable.

The macOS job is capped at 60 minutes, so every retry is bounded by wall clock
as well as by attempt count: `SETUP_BUDGET_SECONDS` covers the whole step,
`PULL_BUDGET_SECONDS` carves out the pull phase so a slow registry cannot
starve the retries, and `READY_TIMEOUT_SECONDS` bounds each container attempt.
The pipeline step adds a `timeoutInMinutes` backstop over the top.

Every bound is sized above the measured worst case of the *successful* runs, so
it only ever catches a hang: pulls run p50 292s / p95 478s / max 724s over 112
runs, and a healthy readiness wait maxes out at 157s over 97 runs. An earlier
480s pull budget would have killed 4.5% of runs outright.

**Environment:** requires `SQL_PASSWORD`. Overrides: `SQL_IMAGE`,
`SQL_CONTAINER`, `SETUP_BUDGET_SECONDS` (1200), `PULL_BUDGET_SECONDS` (900),
`READY_TIMEOUT_SECONDS` (240), `MAX_START_ATTEMPTS` (3),
`PROBE_LOGIN_TIMEOUT_SECONDS` (5), `PROBE_INTERVAL_SECONDS` (3).

## Pipeline Integration

These scripts are referenced in the Azure DevOps pipeline template:

```yaml
- task: PowerShell@2
  displayName: 'Generate Certificate for TLS encryption'
  inputs:
    targetType: 'filePath'
    filePath: '.pipeline/scripts/Generate-SqlCertificates.ps1'
    arguments: '-InstanceName "MSSQLSERVER"'

- task: PowerShell@2
  displayName: 'Enable SQL Browser service'
  inputs:
    targetType: 'filePath'
    filePath: '.pipeline/scripts/Enable-SqlBrowser.ps1'
    arguments: '-ServiceName "SQLBrowser"'

- task: PowerShell@2
  displayName: 'Enable Named Pipes and Shared Memory protocols'
  inputs:
    targetType: 'filePath'
    filePath: '.pipeline/scripts/Enable-SqlProtocols.ps1'
    arguments: '-InstanceName "MSSQLSERVER" -SqlVersion "MSSQL17" -RestartService $true'
```

## Prerequisites

- PowerShell with Administrator privileges
- SQL Server installed on the target machine
- Access to Windows registry for protocol configuration

## Error Handling

Both scripts include comprehensive error handling and will:
- Display clear status messages
- Exit with non-zero code on critical failures
- Provide troubleshooting information for common issues