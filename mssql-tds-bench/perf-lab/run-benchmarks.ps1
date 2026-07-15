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
    try {
        $tz = Get-CimInstance -Namespace root/wmi -ClassName MSAcpi_ThermalZoneTemperature -ErrorAction Stop | Select-Object -First 1
        if ($tz) { $temp = [math]::Round(($tz.CurrentTemperature / 10.0) - 273.15, 1) }
    } catch { }
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

# Run a measured `cargo bench` invocation, optionally pinned to a CPU set (the
# Windows analog of `taskset` in run-benchmarks.sh). When no CPU set is requested
# the plain call is used so behavior is unchanged. cargo's child bench process
# inherits the affinity mask set on the cargo process at spawn, and since
# compilation dominates cargo's startup the mask is in place well before the
# bench binary launches.
function Invoke-Bench {
    param([Parameter(Mandatory)][string]$SaveBaseline)
    Write-CpuSample "$SaveBaseline-start"
    try {
        if ($null -eq $script:BenchAffinity) {
            Invoke-Native { cargo bench -p mssql-tds-bench -- --save-baseline $SaveBaseline }
            return
        }
        $proc = Start-Process -FilePath 'cargo' `
            -ArgumentList @('bench', '-p', 'mssql-tds-bench', '--', '--save-baseline', $SaveBaseline) `
            -NoNewWindow -PassThru
        try {
            $proc.ProcessorAffinity = [IntPtr]$script:BenchAffinity
        } catch {
            Write-Host ">>> WARNING: could not set ProcessorAffinity: $($_.Exception.Message)"
        }
        $proc.WaitForExit()
        if ($proc.ExitCode -ne 0) {
            throw "cargo bench (--save-baseline $SaveBaseline) failed (exit $($proc.ExitCode))"
        }
    } finally {
        Write-CpuSample "$SaveBaseline-end"
    }
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

# --- Optional CPU pinning (avoid contention with a colocated SQL Server) ---
# Mirror run-benchmarks.sh: when the lab reserves cores for SQL Server and
# publishes the free set via PERF_CLIENT_CPUS (e.g. "16-31"), pin the benchmark
# client to that DISJOINT set so the two do not fight for the same CPUs.
# BENCH_CPUS overrides locally. If neither is set the benchmarks run unpinned.
$BenchCpuList = if ($env:BENCH_CPUS) { $env:BENCH_CPUS } else { $env:PERF_CLIENT_CPUS }
$script:BenchAffinity = ConvertTo-AffinityMask $BenchCpuList
if ($null -ne $script:BenchAffinity) {
    Write-Host (">>> Pinning benchmark client to CPUs '$BenchCpuList' (affinity 0x{0:X})" -f $script:BenchAffinity)
}

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
Invoke-Bench 'candidate'

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
Invoke-Bench 'base'

# Restore the candidate source and remove the worktree.
Remove-Item -Recurse -Force $CandidateSrc
Move-Item $StashedSrc $CandidateSrc
Invoke-Native { git worktree remove --force $BaselineTree }

# --- Compare ---
Write-Host '>>> Comparing base -> candidate...'
# The critcmp table contains the ± sign (UTF-8). The previous code wrote
# comparison.txt (correct) but then rebuilt summary.md by re-reading that file
# with Get-Content, whose default encoding did not match how it was written, so
# the ± was mangled only in summary.md. Fix: capture critcmp once and build BOTH
# artifacts from that same in-memory string, written as UTF-8 without a BOM, so
# they cannot diverge. Set the console decode to UTF-8 too (guarded: a
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

# Markdown summary — the perf lab attaches results/*.md to the run's Summary tab.
# Wrap the fixed-width critcmp table in a fenced code block.
#
# Verdict: in each critcmp data row the faster side is 1.00 and the slower side
# shows its ratio, so the candidate regressed a bench when the candidate ratio
# (field 6) exceeds the threshold.
$thr = [double]($env:BENCH_REGRESSION_RATIO)
if (-not $thr) { $thr = 1.10 }
$regressions = @()
foreach ($line in ($comparison -split "\r?\n")) {
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

$summary = @(
    '## mssql-tds perf - base -> candidate'
    ''
    "**$verdict**"
    ''
    "Baseline commit: ``$BaselineCommit``"
    ''
    '```'
    $comparison
    '```'
) -join "`n"
[System.IO.File]::WriteAllText((Join-Path $ResultsDir 'summary.md'), $summary + "`n", $Utf8NoBom)

Copy-Item -Recurse -Force 'target/criterion' (Join-Path $ResultsDir 'criterion') -ErrorAction SilentlyContinue

Write-Host ">>> Done. Results in $ResultsDir"

# Fail the run when any benchmark regressed past the threshold so the pipeline
# surfaces it. Use `throw`, not `exit`: the scheduled-task wrapper relies on its
# finally block to write the EXIT_CODE/DONE sentinels, and `exit` from a called
# .ps1 terminates the whole process and would skip it (leaving run-remote to hang
# until timeout). summary.md names the offenders; the standard triage is to
# re-run to confirm a real regression versus run-to-run noise.
if ($regressions.Count -gt 0) {
    throw "PERF REGRESSION: $verdict"
}
