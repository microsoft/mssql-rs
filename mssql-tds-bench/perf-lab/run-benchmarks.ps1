# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# run-benchmarks.ps1 — perf-lab testScript for mssql-tds (Windows).
#
# Windows equivalent of run-benchmarks.sh. The shared Perf.Test.Job template
# copies the repository to the VM and launches this script from the repo root,
# with SQL_SERVER and SQL_PASSWORD injected as environment variables.
#
# Builds the mssql-tds-bench harness TWICE from the SAME (candidate) working
# tree, swapping ONLY the mssql-tds source (working tree vs a local
# `git worktree` checkout of the commit pinned in baseline-commit.txt), then
# compares with critcmp.

$ErrorActionPreference = 'Stop'

# Native tools (cargo, git, rustup) legitimately write progress to stderr. On
# Windows PowerShell 5.1 (the perf image ships Desktop 5.1) a native command's
# stderr is promoted to a *terminating* error when $ErrorActionPreference is
# 'Stop' — most reliably when the command's streams are redirected — which would
# abort the run on benign output like cargo's "Updating crates.io index". Run
# native commands with the preference relaxed and gate on the real exit code.
function Invoke-Native {
    param([Parameter(Mandatory)][scriptblock]$Command)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Command
        if ($LASTEXITCODE -ne 0) {
            throw "Native command failed (exit $LASTEXITCODE): $Command"
        }
    } finally {
        $ErrorActionPreference = $prev
    }
}

$RepoRoot   = (Get-Location).Path
$ResultsDir = Join-Path $RepoRoot 'results'
# Baseline pointer — a committed commit SHA. Advancing the baseline requires a
# PR that edits this file, so every move is reviewed and recorded in history.
$BaselineFile = Join-Path $RepoRoot 'mssql-tds-bench/perf-lab/baseline-commit.txt'
New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

# --- Connection (SQL_SERVER / SQL_PASSWORD injected by run-remote) ---
if (-not $env:SQL_SERVER)   { throw 'SQL_SERVER not set' }
if (-not $env:SQL_PASSWORD) { throw 'SQL_PASSWORD not set' }
$env:DB_HOST = $env:SQL_SERVER
if (-not $env:DB_PORT)                 { $env:DB_PORT = '1433' }
if (-not $env:DB_USERNAME)             { $env:DB_USERNAME = 'sa' }
if (-not $env:TRUST_SERVER_CERTIFICATE){ $env:TRUST_SERVER_CERTIFICATE = 'true' }

# --- Toolchain ---
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host '>>> Installing Rust toolchain via rustup...'
    Invoke-WebRequest 'https://win.rustup.rs' -OutFile 'rustup-init.exe'
    Invoke-Native { & ./rustup-init.exe -y --default-toolchain stable }
}
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

# --- git (needed for the baseline worktree) ---
# The Windows Server perf image (RUST-Win22-Sql25-1P) normally ships git, but
# install it if absent: winget first, then Chocolatey as a fallback.
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Host '>>> git not found; installing...'
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        winget install --id Git.Git -e --source winget `
            --accept-package-agreements --accept-source-agreements
    } else {
        if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
            Write-Host '>>> Installing Chocolatey...'
            Set-ExecutionPolicy Bypass -Scope Process -Force
            [System.Net.ServicePointManager]::SecurityProtocol = `
                [System.Net.ServicePointManager]::SecurityProtocol -bor 3072
            Invoke-Expression ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
        }
        choco install git -y --no-progress
    }
    # Refresh PATH so the freshly installed git resolves in this session.
    $env:PATH = [System.Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
                [System.Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        throw 'git installation failed'
    }
}

if (-not (Get-Command critcmp -ErrorAction SilentlyContinue)) {
    Write-Host '>>> Installing critcmp...'
    Invoke-Native { cargo install critcmp --version 0.1.8 --locked }
}

# --- Kernel network tuning for high connection churn ---
# The concurrent_connects benchmark opens tens of thousands of short-lived TCP
# connections, which can exhaust the dynamic port range and fail new connects
# with WSAEADDRNOTAVAIL. Widen the IPv4/IPv6 dynamic port range and shorten the
# TIME_WAIT delay. Best-effort: ignore failures (e.g. insufficient privilege).
Write-Host '>>> Tuning dynamic ports / TIME_WAIT for connection benchmarks...'
try {
    netsh int ipv4 set dynamicport tcp start=1024 num=64511 | Out-Null
    netsh int ipv6 set dynamicport tcp start=1024 num=64511 | Out-Null
    New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters' `
        -Name 'TcpTimedWaitDelay' -Value 30 -PropertyType DWord -Force | Out-Null
} catch {
    Write-Host ">>> Network tuning skipped: $($_.Exception.Message)"
}

# --- Resolve and verify the baseline commit (from baseline-commit.txt) ---
if (-not (Test-Path $BaselineFile)) {
    throw "Baseline file not found: $BaselineFile"
}
$BaselineCommit = (Get-Content $BaselineFile |
    Where-Object { $_ -notmatch '^\s*(#|$)' } |
    Select-Object -First 1).Trim()
if ($BaselineCommit -notmatch '^[0-9a-fA-F]{7,40}$') {
    throw "$BaselineFile does not contain a valid commit SHA (got: '$BaselineCommit')"
}
& git rev-parse --verify --quiet "$BaselineCommit^{commit}" *> $null
if ($LASTEXITCODE -ne 0) {
    throw "Baseline commit '$BaselineCommit' not found in the shipped repository. Ensure the checkout fetches full history."
}
Write-Host ">>> Baseline commit: $BaselineCommit"

# --- Release-grade sampling for the lab ---
# Heavier than the lighter defaults baked into criterion_config() (which keep a
# local `cargo bench` fast). Pre-set any of these to override.
if (-not $env:BENCH_WARMUP_SECS) { $env:BENCH_WARMUP_SECS = '10' }
if (-not $env:BENCH_SECS)        { $env:BENCH_SECS = '30' }
if (-not $env:BENCH_SAMPLES)     { $env:BENCH_SAMPLES = '30' }

# Note: client CPU pinning (PERF_CLIENT_CPUS) is applied on Linux via taskset in
# run-benchmarks.sh; a Windows equivalent (ProcessorAffinity) is not yet wired up.

# --- Warm-up pass (discarded) ---
# Candidate is measured first and baseline second; prime SQL Server and the OS
# once (fast, discarded) so both measured runs start warm and the candidate
# doesn't pay a cold-cache penalty on the largest benches (e.g. the 20 MB LOB).
Write-Host '>>> Warm-up pass (discarded)...'
$origWarm = $env:BENCH_WARMUP_SECS; $origSecs = $env:BENCH_SECS; $origSamples = $env:BENCH_SAMPLES
$env:BENCH_WARMUP_SECS = '1'; $env:BENCH_SECS = '1'; $env:BENCH_SAMPLES = '10'
# Warm-up failures are non-fatal (matches run-benchmarks.sh's `|| true`). Relax
# the error preference so cargo's stderr progress under the stream redirect does
# not abort the run, and deliberately ignore the exit code.
& {
    $ErrorActionPreference = 'Continue'
    cargo bench -p mssql-tds-bench -- --save-baseline warmup *> $null
}
$env:BENCH_WARMUP_SECS = $origWarm; $env:BENCH_SECS = $origSecs; $env:BENCH_SAMPLES = $origSamples

# --- Candidate run (mssql-tds = working tree) ---
Write-Host '>>> Candidate benchmarks...'
Invoke-Native { cargo bench -p mssql-tds-bench -- --save-baseline candidate }

# --- Baseline run (mssql-tds source swapped to the baseline commit) ---
# Materialize the baseline mssql-tds via a local worktree, then replace the
# workspace's mssql-tds source in place. The harness (mssql-tds-bench) and its
# `path = "../mssql-tds"` dependency are unchanged, so mssql-tds is the only
# variable. Swapping the source keeps a single mssql-tds in the workspace and
# avoids a Cargo lockfile package collision.
$BaselineTree = Join-Path ([System.IO.Path]::GetTempPath()) "perf-baseline-$([System.Guid]::NewGuid().ToString('N'))"
Write-Host ">>> Adding baseline worktree for $BaselineCommit at $BaselineTree..."
Invoke-Native { git worktree add --detach $BaselineTree $BaselineCommit }

Write-Host '>>> Swapping mssql-tds source to the baseline...'
$CandidateSrc = Join-Path $RepoRoot 'mssql-tds'
$StashedSrc   = Join-Path $RepoRoot '.mssql-tds-candidate'
Move-Item $CandidateSrc $StashedSrc
Copy-Item -Recurse (Join-Path $BaselineTree 'mssql-tds') $CandidateSrc

Write-Host '>>> Baseline benchmarks...'
Invoke-Native { cargo bench -p mssql-tds-bench -- --save-baseline base }

# Restore the candidate source and remove the worktree.
Remove-Item -Recurse -Force $CandidateSrc
Move-Item $StashedSrc $CandidateSrc
Invoke-Native { git worktree remove --force $BaselineTree }

# --- Compare ---
Write-Host '>>> Comparing base -> candidate...'
& {
    $ErrorActionPreference = 'Continue'
    critcmp base candidate | Tee-Object -FilePath (Join-Path $ResultsDir 'comparison.txt')
    if ($LASTEXITCODE -ne 0) { throw "critcmp failed (exit $LASTEXITCODE)" }
}

# Markdown summary — the perf lab attaches results/*.md to the run's Summary tab
# (task.uploadsummary). Wrap the fixed-width critcmp table in a fenced code block.
#
# Verdict: in each critcmp data row the faster side is 1.00 and the slower side
# shows its ratio, so the candidate regressed a bench when the candidate ratio
# (field 6) exceeds the threshold.
$thr = [double]($env:BENCH_REGRESSION_RATIO)
if (-not $thr) { $thr = 1.10 }
$regressions = @()
foreach ($line in (Get-Content (Join-Path $ResultsDir 'comparison.txt'))) {
    $f = @($line -split '\s+' | Where-Object { $_ -ne '' })
    if ($f.Count -ge 6 -and $f[1] -match '^[0-9]+\.[0-9]+$' -and $f[5] -match '^[0-9]+\.[0-9]+$') {
        $cand = [double]$f[5]
        if ($cand -ge $thr) { $regressions += [pscustomobject]@{ Name = $f[0]; Ratio = $cand } }
    }
}
$pct = [int][math]::Round(($thr - 1) * 100)
$warn = [char]::ConvertFromUtf32(0x26A0) + [char]::ConvertFromUtf32(0xFE0F)
$check = [char]::ConvertFromUtf32(0x2705)
if ($regressions.Count -gt 0) {
    $worst = $regressions | Sort-Object Ratio -Descending | Select-Object -First 1
    $wpct = [int][math]::Round(($worst.Ratio - 1) * 100)
    $verdict = "$warn $($regressions.Count) benchmark(s) slower by >=$pct% vs baseline (worst: $($worst.Name) +$wpct%)"
} else {
    $verdict = "$check No benchmark slower by >=$pct% vs baseline"
}

$comparison = Get-Content (Join-Path $ResultsDir 'comparison.txt') -Raw
@(
    '## mssql-tds perf - base -> candidate'
    ''
    "**$verdict**"
    ''
    "Baseline commit: ``$BaselineCommit``"
    ''
    '```'
    $comparison.TrimEnd()
    '```'
) -join "`n" | Set-Content -Path (Join-Path $ResultsDir 'summary.md') -Encoding UTF8

Copy-Item -Recurse -Force 'target/criterion' (Join-Path $ResultsDir 'criterion') -ErrorAction SilentlyContinue

Write-Host ">>> Done. Results in $ResultsDir"
