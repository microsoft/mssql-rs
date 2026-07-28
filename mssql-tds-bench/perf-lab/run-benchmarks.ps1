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

# Convert a taskset-style CPU list ("16-31", "8,9,10", "8-11,14") into a Win32
# process-affinity bitmask. Returns $null when the list is empty. Mirrors the
# `taskset -c` contract that run-benchmarks.sh consumes on Linux.
function ConvertTo-AffinityMask {
    param([string]$CpuList)
    if ([string]::IsNullOrWhiteSpace($CpuList)) { return $null }
    [long]$mask = 0
    foreach ($part in ($CpuList -split ',')) {
        $p = $part.Trim()
        if ($p -eq '') { continue }
        if ($p -match '^(\d+)-(\d+)$') {
            $lo = [int]$Matches[1]; $hi = [int]$Matches[2]
            if ($lo -gt $hi) { $tmp = $lo; $lo = $hi; $hi = $tmp }
            for ($c = $lo; $c -le $hi; $c++) { $mask = $mask -bor ([long]1 -shl $c) }
        } elseif ($p -match '^\d+$') {
            $mask = $mask -bor ([long]1 -shl [int]$p)
        } else {
            throw "PERF_CLIENT_CPUS/BENCH_CPUS: unrecognized token '$p' (expected CPU numbers or ranges like 16-31)"
        }
    }
    if ($mask -eq 0) { return $null }
    return $mask
}

# Sample effective CPU frequency and busy% once (best-effort). Used to bracket
# each measured pass so we can see whether the second (baseline) pass runs at a
# different frequency or utilization than the first (candidate) — i.e. whether
# the hardware is actually the variable, or something else is (e.g. the client
# contending with SQL Server for cores). Effective MHz = base MHz * %perf/100
# (%perf can exceed 100 under turbo). Temperature is usually unavailable inside
# an Azure guest; it is captured only if the ACPI thermal zone is exposed.
function Get-CpuSample {
    $perf = $null; $freq = $null; $busy = $null; $temp = $null
    try {
        $s = (Get-Counter -Counter @(
            '\Processor Information(_Total)\% Processor Performance',
            '\Processor Information(_Total)\Processor Frequency',
            '\Processor Information(_Total)\% Processor Time') -ErrorAction Stop).CounterSamples
        $perf = [math]::Round($s[0].CookedValue, 1)
        $freq = [math]::Round($s[1].CookedValue, 0)
        $busy = [math]::Round($s[2].CookedValue, 1)
    } catch { }
    # CPU temperature is not exposed to Azure guests (no ACPI thermal zone), so
    # we do not probe it here; the frequency/busy signal above is what matters.
    $eff = if (($null -ne $perf) -and ($null -ne $freq)) { [math]::Round($freq * $perf / 100.0, 0) } else { $null }
    [pscustomobject]@{ PctPerf = $perf; BaseMHz = $freq; EffMHz = $eff; Busy = $busy; TempC = $temp }
}

# Append a labeled CPU sample to the telemetry CSV and echo it to the log.
function Write-CpuSample {
    param([string]$Label)
    $s = Get-CpuSample
    if ($script:TelemetryCsv) {
        ('{0:o},{1},{2},{3},{4},{5},{6}' -f (Get-Date), $Label, $s.PctPerf, $s.BaseMHz, $s.EffMHz, $s.Busy, $s.TempC) |
            Add-Content -Path $script:TelemetryCsv -Encoding utf8
    }
    Write-Host (">>> cpu[{0}] effFreq={1}MHz base={2}MHz %perf={3} busy={4}% temp={5}" -f $Label, $s.EffMHz, $s.BaseMHz, $s.PctPerf, $s.Busy, $s.TempC)
}

# Run a measured `cargo bench` invocation and bracket it with CPU samples. The
# client CPU pinning (when requested) is applied to the harness process before
# these run, so cargo inherits it — see the pinning block below.
function Invoke-Bench {
    param([Parameter(Mandatory)][string]$SaveBaseline, [string]$Filter)
    # Client CPU pinning is applied once to THIS process (see the pinning block
    # below); cargo and the bench binary it spawns inherit the affinity, so we
    # just run cargo the normal way here. Using the call operator (not
    # Start-Process) keeps stdout streaming to the run log and yields a reliable
    # exit code via $LASTEXITCODE. $Filter, when set, is a Criterion benchmark-id
    # regex that limits the run to specific benchmarks (used by auto-confirm).
    Write-CpuSample "$SaveBaseline-start"
    try {
        $benchArgs = @('bench', '-p', 'mssql-tds-bench', '--', '--save-baseline', $SaveBaseline)
        if ($Filter) { $benchArgs += $Filter }
        Invoke-Native { cargo @benchArgs }
    } finally {
        Write-CpuSample "$SaveBaseline-end"
    }
}

# Parse a critcmp comparison table and emit the benchmarks whose candidate ratio
# (the 6th whitespace field) meets or exceeds $Threshold, as objects with
# Name/Ratio. critcmp prints the faster side as 1.00 and the slower side as its
# ratio, so a candidate ratio >= threshold means the candidate regressed.
function Get-CritcmpRegressions {
    param([string]$Comparison, [double]$Threshold)
    foreach ($line in ($Comparison -split "\r?\n")) {
        $f = @($line -split '\s+' | Where-Object { $_ -ne '' })
        if ($f.Count -ge 6 -and $f[1] -match '^[0-9]+\.[0-9]+$' -and $f[5] -match '^[0-9]+\.[0-9]+$') {
            $cand = [double]$f[5]
            if ($cand -ge $Threshold) { [pscustomobject]@{ Name = $f[0]; Ratio = $cand } }
        }
    }
}

# Fast, discarded run that primes SQL Server's buffer pool and the OS page cache
# so the measured pass that follows starts warm. Run before BOTH the candidate
# and baseline passes: the baseline mssql-tds is rebuilt after a long candidate
# pass, which evicts caches, so without a re-warm the baseline looks spuriously
# slower on the I/O-heavy benches (LOB, packet-size). $Filter optionally limits
# it to a Criterion benchmark-id regex.
function Invoke-WarmupPass {
    param([string]$Filter)
    Write-Host (">>> Warm-up pass (discarded)" + $(if ($Filter) { " [$Filter]" } else { "" }) + "...")
    $ow = $env:BENCH_WARMUP_SECS; $os = $env:BENCH_SECS; $oa = $env:BENCH_SAMPLES
    $env:BENCH_WARMUP_SECS = '1'; $env:BENCH_SECS = '1'; $env:BENCH_SAMPLES = '10'
    $wargs = @('bench', '-p', 'mssql-tds-bench', '--', '--save-baseline', 'warmup')
    if ($Filter) { $wargs += $Filter }
    & {
        $ErrorActionPreference = 'Continue'
        cargo @wargs *> $null
    }
    $env:BENCH_WARMUP_SECS = $ow; $env:BENCH_SECS = $os; $env:BENCH_SAMPLES = $oa
}

$RepoRoot   = (Get-Location).Path
$ResultsDir = Join-Path $RepoRoot 'results'
# Baseline pointer — a committed commit SHA. Advancing the baseline requires a
# PR that edits this file, so every move is reviewed and recorded in history.
$BaselineFile = Join-Path $RepoRoot 'mssql-tds-bench/perf-lab/baseline-commit.txt'
New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

# CPU telemetry file: bracketed effective-frequency/busy/temp samples written
# around each measured pass (see Write-CpuSample) so we can validate whether CPU
# frequency or contention differs between the candidate and baseline passes.
$script:TelemetryCsv = Join-Path $ResultsDir 'cpu-telemetry.csv'
'timestamp,label,pct_processor_performance,base_freq_mhz,eff_freq_mhz,cpu_busy_pct,temp_c' |
    Set-Content -Path $script:TelemetryCsv -Encoding utf8

# --- Connection (SQL_SERVER / SQL_PASSWORD injected by run-remote) ---
if (-not $env:SQL_SERVER)   { throw 'SQL_SERVER not set' }
if (-not $env:SQL_PASSWORD) { throw 'SQL_PASSWORD not set' }
$env:DB_HOST = $env:SQL_SERVER
if (-not $env:DB_PORT)                 { $env:DB_PORT = '1433' }
if (-not $env:DB_USERNAME)             { $env:DB_USERNAME = 'sa' }
if (-not $env:TRUST_SERVER_CERTIFICATE){ $env:TRUST_SERVER_CERTIFICATE = 'true' }

# The perf lab always has a server provisioned and injected, so a failure to
# connect must FAIL the run, not skip it. This flag makes the benches' try_connect
# panic instead of returning None (see mssql-tds-bench/src/lib.rs); without it an
# unreachable server would skip every benchmark, leave comparison.txt empty, and
# the gate would pass spuriously green.
$env:BENCH_REQUIRE_SERVER = '1'

# --- SQL Server configuration snapshot (validate the instance is tuned) ---
# Dump the effective memory / MAXDOP / cost-threshold / affinity, tempdb file
# placement, durability/recovery, and trace flags so we can confirm the perf
# tuning took and has not drifted. Best-effort — never fail the run over it.
$SqlConfigSql = Join-Path $RepoRoot 'mssql-tds-bench/perf-lab/sql-config-dump.sql'
try {
    $sqlcmdExe = (Get-Command sqlcmd -ErrorAction SilentlyContinue).Source
    if (-not $sqlcmdExe) {
        $probe = 'C:\Program Files\Microsoft SQL Server\Client SDK\ODBC\Tools\Binn\SQLCMD.EXE'
        if (Test-Path $probe) { $sqlcmdExe = $probe }
    }
    if ($sqlcmdExe -and (Test-Path $SqlConfigSql)) {
        Write-Host '>>> Capturing SQL Server configuration snapshot...'
        & {
            $ErrorActionPreference = 'Continue'
            & $sqlcmdExe -S $env:SQL_SERVER -U $env:DB_USERNAME -P $env:SQL_PASSWORD -C -b -y 0 -Y 30 -i $SqlConfigSql |
                Tee-Object -FilePath (Join-Path $ResultsDir 'sql-config.txt')
        }
    } else {
        Write-Host '>>> Skipping SQL config snapshot (sqlcmd or query file not found).'
    }
} catch {
    Write-Host ">>> SQL config snapshot skipped: $($_.Exception.Message)"
}

# --- Toolchain ---
# Reuse the repo's canonical rustup installer (the same script the real CI stages
# use, shipped to the VM at .pipeline\scripts\) rather than a second, drifting
# copy. It passes no --default-toolchain, so the repo's rust-toolchain.toml
# (channel = "1.95") drives the version the benches build under, and it sets the
# cargo bin dir on the in-process PATH.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host '>>> Installing Rust toolchain via .pipeline\scripts\InstallRustup.ps1...'
    & (Join-Path $RepoRoot '.pipeline/scripts/InstallRustup.ps1')
}
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
# Fail loud if the toolchain still isn't available: the lab must not proceed to a
# silent no-op run.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'Rust toolchain install failed: cargo not found after InstallRustup.ps1'
}

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
if ($BaselineCommit -notmatch '^[0-9a-fA-F]{40}$') {
    throw "$BaselineFile does not contain a valid 40-character commit SHA (got: '$BaselineCommit')"
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

# --- Optional CPU pinning (avoid contention with a colocated SQL Server) ---
# Mirror run-benchmarks.sh: when the lab reserves cores for SQL Server and
# publishes the free set via PERF_CLIENT_CPUS (e.g. "16-31"), pin the benchmark
# client to that DISJOINT set so the two do not fight for the same CPUs.
# BENCH_CPUS overrides locally. If neither is set the benchmarks run unpinned.
$BenchCpuList = if ($env:BENCH_CPUS) { $env:BENCH_CPUS } else { $env:PERF_CLIENT_CPUS }
$script:BenchAffinity = ConvertTo-AffinityMask $BenchCpuList
if ($null -ne $script:BenchAffinity) {
    # Pin THIS PowerShell process to the reserved core set; cargo and the bench
    # binary it spawns inherit the affinity mask at creation, so the whole client
    # runs disjoint from SQL Server's cores. Setting affinity on the harness
    # process is more robust than launching each cargo run via Start-Process,
    # whose -PassThru ExitCode is null unless the handle is cached and whose child
    # stdout is lost under the detached scheduled-task wrapper.
    try {
        (Get-Process -Id $PID).ProcessorAffinity = [IntPtr]$script:BenchAffinity
        Write-Host (">>> Pinned benchmark client (this process + children) to CPUs '$BenchCpuList' (affinity 0x{0:X})" -f $script:BenchAffinity)
    } catch {
        Write-Host ">>> WARNING: could not set ProcessorAffinity: $($_.Exception.Message)"
    }
}

# --- Build both sides, then interleave per bench binary --------------------
# Make each benchmark's candidate and baseline measurements adjacent in time
# (cancels the slow drift that otherwise makes the second, baseline pass look
# spuriously slower) by building BOTH bench binaries up front and running them
# per-binary back-to-back instead of all-candidate-then-all-baseline. Criterion
# writes to $env:CRITERION_HOME; both sides point at the shared target/criterion
# so critcmp can compare them. The two sides build into separate target dirs so
# both persist. Interleaving per bench BINARY (not per individual bench) keeps
# setup cost - and total run time - the same as the old two-pass approach.

# Returns @{ bench-name = exe-path } for the built bench binaries. $TargetDir
# sets CARGO_TARGET_DIR so the two sides build into distinct trees.
function Get-BenchBinaries {
    param([Parameter(Mandatory)][string]$TargetDir)
    $bins = @{}
    $prev = $env:CARGO_TARGET_DIR
    $env:CARGO_TARGET_DIR = $TargetDir
    try {
        $lines = & {
            $ErrorActionPreference = 'Continue'
            cargo bench -p mssql-tds-bench --no-run --message-format=json 2>$null
        }
        foreach ($line in $lines) {
            if (-not $line) { continue }
            try { $m = $line | ConvertFrom-Json } catch { continue }
            if ($m.executable -and ($m.target.kind -contains 'bench')) {
                $bins[$m.target.name] = $m.executable
            }
        }
    } finally {
        if ($null -eq $prev) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
        else { $env:CARGO_TARGET_DIR = $prev }
    }
    $bins
}

# Run every bench binary once per side, candidate then baseline back-to-back,
# saving to Criterion baselines $CandName / $BaseName; $Filter optionally limits
# to a Criterion benchmark-id regex. Both binaries write to the shared
# target/criterion via CRITERION_HOME. The child processes inherit the client CPU
# pinning set on this process earlier.
function Invoke-Interleave {
    param([Parameter(Mandatory)][string]$CandName, [Parameter(Mandatory)][string]$BaseName, [string]$Filter)
    $env:CRITERION_HOME = Join-Path $RepoRoot 'target/criterion'
    try {
        foreach ($name in @($script:CandBins.Keys)) {
            $cpath = $script:CandBins[$name]
            $bpath = $script:BaseBins[$name]
            if (-not $bpath) { Write-Host ">>> WARN: no baseline binary for '$name'; skipping"; continue }
            $cargs = @('--bench', '--save-baseline', $CandName); if ($Filter) { $cargs += $Filter }
            $bargs = @('--bench', '--save-baseline', $BaseName); if ($Filter) { $bargs += $Filter }
            Write-Host ">>> [$name] candidate..."
            Invoke-Native { & $cpath @cargs }
            Write-Host ">>> [$name] baseline..."
            Invoke-Native { & $bpath @bargs }
        }
    } finally {
        Remove-Item Env:CRITERION_HOME -ErrorAction SilentlyContinue
    }
}

$CandidateSrc = Join-Path $RepoRoot 'mssql-tds'
$StashedSrc   = Join-Path $RepoRoot '.mssql-tds-candidate'
$BaselineTree = Join-Path ([System.IO.Path]::GetTempPath()) "perf-baseline-$([System.Guid]::NewGuid().ToString('N'))"
function Set-BaselineSource {
    Move-Item $script:CandidateSrc $script:StashedSrc
    Copy-Item -Recurse (Join-Path $script:BaselineTree 'mssql-tds') $script:CandidateSrc
}
function Restore-CandidateSource {
    Remove-Item -Recurse -Force $script:CandidateSrc
    Move-Item $script:StashedSrc $script:CandidateSrc
}

Write-Host '>>> Building candidate bench binaries (target/)...'
$script:CandBins = Get-BenchBinaries (Join-Path $RepoRoot 'target')
if ($script:CandBins.Count -eq 0) { throw 'no candidate bench binaries found' }

Write-Host ">>> Adding baseline worktree for $BaselineCommit at $BaselineTree..."
Invoke-Native { git worktree add --detach $BaselineTree $BaselineCommit }
Write-Host '>>> Building baseline bench binaries (target-base/)...'
Set-BaselineSource
$script:BaseBins = Get-BenchBinaries (Join-Path $RepoRoot 'target-base')
Restore-CandidateSource
Invoke-Native { git worktree remove --force $BaselineTree }
if ($script:BaseBins.Count -eq 0) { throw 'no baseline bench binaries found' }

# Warm-up once; interleaving keeps each candidate/baseline pair adjacent so one
# warm-up is enough to prime SQL Server / the OS caches.
Invoke-WarmupPass

Write-Host '>>> Interleaving candidate/baseline per bench binary...'
Write-CpuSample 'interleave-start'
Invoke-Interleave 'candidate' 'base'
Write-CpuSample 'interleave-end'

# --- Compare ---
Write-Host '>>> Comparing base -> candidate...'
# The critcmp table contains the ± sign (UTF-8). Capture critcmp once and build
# every artifact from that same in-memory string, written as UTF-8 without a BOM,
# so they cannot diverge. Set the console decode to UTF-8 too (guarded: a
# console-less host can reject the setter) so the capture itself is UTF-8-clean.
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

$comparison = & {
    $ErrorActionPreference = 'Continue'
    $out = critcmp base candidate | Out-String
    if ($LASTEXITCODE -ne 0) { throw "critcmp failed (exit $LASTEXITCODE)" }
    $out
}
$comparison = $comparison.TrimEnd()
Write-Host $comparison
[System.IO.File]::WriteAllText((Join-Path $ResultsDir 'comparison.txt'), $comparison + "`n", $Utf8NoBom)

$thr = [double]($env:BENCH_REGRESSION_RATIO)
if (-not $thr) { $thr = 1.10 }
$regressions = @(Get-CritcmpRegressions $comparison $thr)

# --- Auto-confirm regressions (re-measure only the offenders, interleaved) ---
# A strict gate can trip on a transient single-benchmark outlier. Re-measure ONLY
# the benchmarks that tripped - interleaved per binary, same as the main run - and
# keep as a real regression only those that trip AGAIN. Both binaries are already
# built, so this just replays the offenders (adds only their run time).
$gateComparison = $comparison
$gateRegressions = $regressions
$confirmComparison = $null
if ($regressions.Count -gt 0) {
    $filter = (($regressions | ForEach-Object { '^' + $_.Name + '$' }) -join '|')
    Write-Host (">>> Gate tripped by: " + (($regressions | ForEach-Object { $_.Name }) -join ', '))
    Write-Host ">>> Auto-confirm: re-measuring only those benchmarks (filter: $filter)"
    Invoke-WarmupPass $filter
    Invoke-Interleave 'candidate_confirm' 'base_confirm' $filter

    Write-Host '>>> Auto-confirm comparison (base_confirm -> candidate_confirm):'
    $confirmComparison = & {
        $ErrorActionPreference = 'Continue'
        $out = critcmp base_confirm candidate_confirm | Out-String
        if ($LASTEXITCODE -ne 0) { throw "critcmp (confirm) failed (exit $LASTEXITCODE)" }
        $out
    }
    $confirmComparison = $confirmComparison.TrimEnd()
    Write-Host $confirmComparison
    [System.IO.File]::WriteAllText((Join-Path $ResultsDir 'confirm.txt'), $confirmComparison + "`n", $Utf8NoBom)
    $gateComparison = $confirmComparison
    $gateRegressions = @(Get-CritcmpRegressions $confirmComparison $thr)
}
Remove-Item -Recurse -Force (Join-Path $RepoRoot 'target-base') -ErrorAction SilentlyContinue

# --- Verdict (based on the gate comparison: the re-measured offenders after
# auto-confirm, or the full run when nothing tripped) ---
$pct = [int][math]::Round(($thr - 1) * 100)
$warn = [char]::ConvertFromUtf32(0x26A0) + [char]::ConvertFromUtf32(0xFE0F)
$check = [char]::ConvertFromUtf32(0x2705)
if ($gateRegressions.Count -gt 0) {
    $worst = $gateRegressions | Sort-Object Ratio -Descending | Select-Object -First 1
    $wpct = [int][math]::Round(($worst.Ratio - 1) * 100)
    $verdict = "$warn $($gateRegressions.Count) benchmark(s) slower by >=$pct% vs baseline (worst: $($worst.Name) +$wpct%)"
} else {
    $verdict = "$check No benchmark slower by >=$pct% vs baseline"
}

$summaryLines = @(
    '## mssql-tds perf - base -> candidate'
    ''
    "**$verdict**"
    ''
)
if ($regressions.Count -gt 0) {
    $summaryLines += '_Auto-confirm re-ran the gate-tripping benchmark(s); the verdict reflects that re-measurement. Benchmarks that tripped once but not on the re-run are treated as transient noise._'
    $summaryLines += ''
}
$summaryLines += @(
    "Baseline commit: ``$BaselineCommit``"
    ''
    '```'
    $comparison
    '```'
)
if ($regressions.Count -gt 0) {
    $summaryLines += @(
        ''
        '### Auto-confirm re-run (offenders only)'
        ''
        ('Tripped on the first pass: ' + (($regressions | ForEach-Object { $_.Name }) -join ', '))
        ''
        '```'
        $confirmComparison
        '```'
    )
}
$summary = $summaryLines -join "`n"
[System.IO.File]::WriteAllText((Join-Path $ResultsDir 'summary.md'), $summary + "`n", $Utf8NoBom)

Copy-Item -Recurse -Force 'target/criterion' (Join-Path $ResultsDir 'criterion') -ErrorAction SilentlyContinue

Write-Host ">>> Done. Results in $ResultsDir"

# Fail the run only on CONFIRMED regressions (the gate comparison). Use `throw`,
# not `exit`: the scheduled-task wrapper relies on its finally block to write the
# EXIT_CODE/DONE sentinels, and `exit` from a called .ps1 terminates the whole
# process and would skip it (leaving run-remote to hang until timeout). summary.md
# names the offenders and shows the auto-confirm re-run.
if ($gateRegressions.Count -gt 0) {
    throw "PERF REGRESSION: $verdict"
}
if ($regressions.Count -gt 0) {
    Write-Host ">>> Auto-confirm cleared all $($regressions.Count) initial regression(s) as transient; passing."
}
