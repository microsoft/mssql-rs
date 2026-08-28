# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Dedicated Windows perf-lab runner for the mssql-odbc result-set benchmarks.

$ErrorActionPreference = 'Stop'

function Invoke-Native {
    # Normalize native failures into PowerShell exceptions so the outer finally
    # block always restores driver registrations and temporary worktrees.
    param([Parameter(Mandatory)][scriptblock]$Command)

    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Command
        if ($LASTEXITCODE -ne 0) {
            throw "Native command failed (exit $LASTEXITCODE): $Command"
        }
    } finally {
        $ErrorActionPreference = $previousPreference
    }
}

function Invoke-CleanupNative {
    # Cleanup is best effort because the original benchmark failure must remain the
    # run's primary error.
    param([Parameter(Mandatory)][scriptblock]$Command, [Parameter(Mandatory)][string]$Description)

    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Command
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "$Description failed with exit code $LASTEXITCODE"
        }
    } catch {
        Write-Warning "$Description failed: $($_.Exception.Message)"
    } finally {
        $ErrorActionPreference = $previousPreference
    }
}

function Get-PositiveIntEnv {
    # Parse tuning knobs consistently instead of letting each call site accept a
    # different set of malformed values.
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][int]$Default)

    $raw = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrEmpty($raw)) {
        return $Default
    }

    $parsed = 0
    if (-not [int]::TryParse(
            $raw,
            [System.Globalization.NumberStyles]::None,
            [System.Globalization.CultureInfo]::InvariantCulture,
            [ref]$parsed
        ) -or $parsed -lt 1) {
        throw "$Name must be a positive integer"
    }
    return $parsed
}

function Get-RatioEnv {
    # Ratios are passed through to the comparator, which owns their validation;
    # parse here only to keep the runner's own arithmetic culture-invariant.
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][double]$Default)

    $raw = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrEmpty($raw)) {
        return $Default
    }
    $parsed = 0.0
    if (-not [double]::TryParse(
            $raw,
            [System.Globalization.NumberStyles]::Float,
            [System.Globalization.CultureInfo]::InvariantCulture,
            [ref]$parsed
        ) -or $parsed -le 1.0) {
        throw "$Name must be a number greater than 1"
    }
    return $parsed
}

function Format-Invariant {
    param([Parameter(Mandatory)][double]$Value, [string]$Format = 'F6')

    return $Value.ToString($Format, [System.Globalization.CultureInfo]::InvariantCulture)
}

function Get-CpuSample {
    # Effective MHz = base MHz * %perf/100 (%perf exceeds 100 under turbo).
    # Temperature is normally unavailable in an Azure guest, so it stays optional.
    $perf = $null; $freq = $null; $busy = $null; $temp = $null
    try {
        $samples = (Get-Counter -Counter @(
                '\Processor Information(_Total)\% Processor Performance',
                '\Processor Information(_Total)\Processor Frequency',
                '\Processor Information(_Total)\% Processor Time') -ErrorAction Stop).CounterSamples
        $perf = [math]::Round($samples[0].CookedValue, 1)
        $freq = [math]::Round($samples[1].CookedValue, 0)
        $busy = [math]::Round($samples[2].CookedValue, 1)
    } catch { }
    $effective = if (($null -ne $perf) -and ($null -ne $freq)) {
        [math]::Round($freq * $perf / 100.0, 0)
    } else {
        $null
    }
    return [pscustomobject]@{
        PctPerf = $perf; BaseMHz = $freq; EffMHz = $effective; Busy = $busy; TempC = $temp
    }
}

function Write-CpuSample {
    # Bracket every measured pass so a confirmation round that disagrees with the
    # initial pass can be checked against the machine's own frequency/load.
    param([Parameter(Mandatory)][string]$Label)

    $sample = Get-CpuSample
    if ($script:TelemetryCsv) {
        ('{0:o},{1},{2},{3},{4},{5},{6}' -f (Get-Date), $Label, $sample.PctPerf,
            $sample.BaseMHz, $sample.EffMHz, $sample.Busy, $sample.TempC) |
            Add-Content -Path $script:TelemetryCsv -Encoding utf8
    }
    Write-Host (">>> cpu[{0}] effFreq={1}MHz base={2}MHz %perf={3} busy={4}% temp={5}" -f
        $Label, $sample.EffMHz, $sample.BaseMHz, $sample.PctPerf, $sample.Busy, $sample.TempC)
}

function ConvertTo-AffinityMask {
    # Translate the Linux-style CPU list used by the perf lab into the mask required
    # by Windows ProcessorAffinity.
    param([string]$CpuList)

    if ([string]::IsNullOrWhiteSpace($CpuList)) {
        return $null
    }

    [long]$mask = 0
    foreach ($part in ($CpuList -split ',')) {
        $value = $part.Trim()
        if ($value -match '^(\d+)-(\d+)$') {
            $first = [int]$Matches[1]
            $last = [int]$Matches[2]
            if ($first -gt $last) {
                $swap = $first
                $first = $last
                $last = $swap
            }
            for ($cpu = $first; $cpu -le $last; $cpu++) {
                $mask = $mask -bor ([long]1 -shl $cpu)
            }
        } elseif ($value -match '^\d+$') {
            $mask = $mask -bor ([long]1 -shl [int]$value)
        } else {
            throw "Invalid CPU list token '$value'"
        }
    }
    if ($mask -eq 0) {
        return $null
    }
    return $mask
}

function Get-RegistryValueState {
    # Preserve absence separately from an empty value so restoration is exact.
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Name)

    if (-not (Test-Path -LiteralPath $Path)) {
        return [pscustomobject]@{ Exists = $false; Value = $null }
    }
    $item = Get-ItemProperty -LiteralPath $Path
    $property = $item.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return [pscustomobject]@{ Exists = $false; Value = $null }
    }
    return [pscustomobject]@{ Exists = $true; Value = $property.Value }
}

function Save-DriverRegistration {
    # Snapshot every registry value the temporary benchmark registration can touch.
    param([Parameter(Mandatory)][string]$Name)

    $keyPath = "$script:OdbcInstRoot\$Name"
    return [pscustomobject]@{
        Name = $Name
        KeyPath = $keyPath
        KeyExisted = Test-Path -LiteralPath $keyPath
        Driver = Get-RegistryValueState -Path $keyPath -Name 'Driver'
        Setup = Get-RegistryValueState -Path $keyPath -Name 'Setup'
        ListEntry = Get-RegistryValueState -Path $script:DriversRegKey -Name $Name
    }
}

function Set-DriverRegistration {
    # Register candidate and baseline builds under private names without replacing
    # the machine's Microsoft ODBC registration.
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$DriverPath
    )

    $keyPath = "$script:OdbcInstRoot\$Name"
    New-Item -Path $keyPath -Force | Out-Null
    Set-ItemProperty -LiteralPath $keyPath -Name 'Driver' -Value $DriverPath
    Set-ItemProperty -LiteralPath $keyPath -Name 'Setup' -Value $DriverPath
    New-Item -Path $script:DriversRegKey -Force | Out-Null
    Set-ItemProperty -LiteralPath $script:DriversRegKey -Name $Name -Value 'Installed'
    Write-Host ">>> Registered '$Name': $DriverPath"
}

function Restore-RegistryValue {
    # Restore both the prior value and the prior existence state.
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)]$State
    )

    if ($State.Exists) {
        Set-ItemProperty -LiteralPath $Path -Name $Name -Value $State.Value
    } elseif (Test-Path -LiteralPath $Path) {
        Remove-ItemProperty -LiteralPath $Path -Name $Name -ErrorAction SilentlyContinue
    }
}

function Restore-DriverRegistration {
    # Return shared perf hosts to their pre-run registry state.
    param([Parameter(Mandatory)]$State)

    if ($State.KeyExisted) {
        Restore-RegistryValue -Path $State.KeyPath -Name 'Driver' -State $State.Driver
        Restore-RegistryValue -Path $State.KeyPath -Name 'Setup' -State $State.Setup
    } else {
        Remove-Item -LiteralPath $State.KeyPath -Recurse -Force -ErrorAction SilentlyContinue
    }
    Restore-RegistryValue -Path $script:DriversRegKey -Name $State.Name -State $State.ListEntry
    Write-Host ">>> Restored registration for '$($State.Name)'"
}

function Find-CMake {
    # Hosted Windows agents do not consistently expose Visual Studio's CMake on PATH.
    $command = Get-Command cmake -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $candidates = @(
        "$env:ProgramFiles\CMake\bin\cmake.exe",
        "$env:ProgramFiles\Microsoft Visual Studio\2022\Enterprise\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe",
        "$env:ProgramFiles\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe",
        "$env:ProgramFiles\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
    )
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }
    throw 'CMake not found'
}

function Build-Driver {
    # Isolated Cargo targets prevent candidate artifacts from contaminating the
    # detached baseline build.
    param(
        [Parameter(Mandatory)][string]$SourceRoot,
        [Parameter(Mandatory)][string]$TargetDir,
        [Parameter(Mandatory)][string]$Label
    )

    Write-Host ">>> Building $Label mssql-odbc driver..."
    $previousTarget = $env:CARGO_TARGET_DIR
    $env:CARGO_TARGET_DIR = $TargetDir
    Push-Location $SourceRoot
    try {
        Invoke-Native { cargo build -p mssql-odbc --release }
    } finally {
        Pop-Location
        if ($null -eq $previousTarget) {
            Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
        } else {
            $env:CARGO_TARGET_DIR = $previousTarget
        }
    }
}

function Find-Sqlcmd {
    $command = Get-Command sqlcmd -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }
    $probe = 'C:\Program Files\Microsoft SQL Server\Client SDK\ODBC\Tools\Binn\SQLCMD.EXE'
    if (Test-Path -LiteralPath $probe) {
        return $probe
    }
    return $null
}

function Initialize-BenchmarkPython {
    # gbench/report.py imports NumPy and SciPy at module scope even with
    # --no-utest, so Google Benchmark's comparator cannot run without them. Prefer
    # an interpreter that already has them; otherwise build a private virtualenv
    # so nothing is installed into the perf host's system Python.
    param([Parameter(Mandatory)][string]$Python, [Parameter(Mandatory)][string]$VenvRoot)

    # Missing imports are an expected probe result. PowerShell 5.1 can promote a
    # native command's redirected stderr to a terminating error under Stop, so use
    # the exit code while this function runs native Python commands.
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Python -c 'import numpy, scipy' *> $null
        if ($LASTEXITCODE -eq 0) {
            return $Python
        }

        Write-Host ">>> Provisioning NumPy/SciPy for Google Benchmark's comparator..."
        $venvPython = Join-Path $VenvRoot 'Scripts\python.exe'
        if (-not (Test-Path -LiteralPath $venvPython)) {
            & $Python -m venv $VenvRoot *> $null
            if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $venvPython)) {
                return $null
            }
        }
        # Send pip's output to the host so only the interpreter path is returned.
        & $venvPython -m pip install --quiet --upgrade pip *> $null
        & $venvPython -m pip install --quiet numpy scipy | Out-Host
        if ($LASTEXITCODE -ne 0) {
            return $null
        }
        return $venvPython
    } finally {
        $ErrorActionPreference = $previousPreference
    }
}

function Invoke-BenchmarkLeg {
    # Keep one raw JSON file per driver/scenario and use the packet-size spelling
    # accepted by that driver. An empty scenario runs every workload in one process.
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Scenario,
        [Parameter(Mandatory)][string]$Driver,
        [Parameter(Mandatory)][string]$Output
    )

    $label = if ($Scenario) { $Scenario } else { 'all scenarios' }
    Write-Host ">>> Running $label with $Driver..."
    $env:ODBC_BENCH_DRIVER = $Driver
    $env:ODBC_BENCH_SCENARIO = $Scenario
    # Windows Microsoft ODBC accepts the spaced spelling; the Rust driver and the
    # Linux runner use PacketSize.
    $env:ODBC_BENCH_PACKET_SIZE_KEYWORD = if ($Driver -eq $script:MicrosoftDriverName) {
        'Packet Size'
    } else {
        'PacketSize'
    }
    $arguments = @(
        "--benchmark_repetitions=$script:Repetitions",
        "--benchmark_out=$Output",
        '--benchmark_out_format=json'
    )
    Invoke-Native { & $script:BenchExe @arguments }
}

function Invoke-Comparator {
    # Exit 2 means the report was written and the gate tripped, so it has to reach
    # the caller instead of becoming a terminating error. The comparator's own
    # output goes straight to the host so only the exit code is returned.
    param([Parameter(Mandatory)][string[]]$Arguments)

    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $script:BenchPython $script:CompareScript @Arguments | Out-Host
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousPreference
    }
}

function Get-BenchmarkScenario {
    # The harness filters by scenario, not by benchmark id, so map each flagged
    # benchmark back to the scenario file it came out of.
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][hashtable]$ScenarioFiles)

    foreach ($scenario in $ScenarioFiles.Keys) {
        if (Select-String -LiteralPath $ScenarioFiles[$scenario] `
                -SimpleMatch -Pattern """run_name"": ""$Name""" -Quiet) {
            return $scenario
        }
    }
    throw "Cannot map benchmark '$Name' to a scenario"
}

function Get-ConfirmationTally {
    # Hits count rounds that reproduce the initial direction; the median covers the
    # confirmation rounds only.
    param(
        [Parameter(Mandatory)][string]$Kind,
        [Parameter(Mandatory)][double[]]$Ratios,
        [Parameter(Mandatory)][double]$RegressionRatio,
        [Parameter(Mandatory)][double]$ImprovementRatio
    )

    $hits = 0
    $regressionHits = 0
    foreach ($ratio in $Ratios) {
        if ($ratio -ge $RegressionRatio) { $regressionHits++ }
        if ($Kind -eq 'regression') {
            if ($ratio -ge $RegressionRatio) { $hits++ }
        } elseif ($ratio -le (1.0 / $ImprovementRatio)) {
            $hits++
        }
    }
    $sorted = @($Ratios | Sort-Object)
    $count = $sorted.Count
    $median = if ($count % 2 -eq 1) {
        $sorted[($count - 1) / 2]
    } else {
        ($sorted[$count / 2 - 1] + $sorted[$count / 2]) / 2
    }
    return [pscustomobject]@{
        Hits = $hits; RegressionHits = $regressionHits; Median = $median
    }
}

function Read-RatioFile {
    param([Parameter(Mandatory)][string]$Path)

    $ratios = @{}
    foreach ($line in Get-Content -LiteralPath $Path) {
        if (-not $line.Trim()) { continue }
        $fields = $line -split "`t"
        if ($fields.Count -lt 2) { continue }
        $value = 0.0
        if ([double]::TryParse(
                $fields[1],
                [System.Globalization.NumberStyles]::Float,
                [System.Globalization.CultureInfo]::InvariantCulture,
                [ref]$value
            )) {
            $ratios[$fields[0]] = $value
        }
    }
    return $ratios
}

$RepoRoot = (Get-Location).Path
$ResultsDir = Join-Path $RepoRoot 'results'
$InitialDir = Join-Path $ResultsDir 'initial'
$ConfirmDir = Join-Path $ResultsDir 'confirm'
$PlanFile = Join-Path $ResultsDir 'confirm-plan.txt'
$BaselineFile = Join-Path $RepoRoot 'mssql-odbc-bench\perf-lab\baseline-commit.txt'
$ReferenceVersionFile = Join-Path $RepoRoot 'mssql-odbc-bench\perf-lab\msodbcsql-version.txt'
# One shared snapshot query for both perf labs; do not fork a second copy.
$SqlConfigSql = Join-Path $RepoRoot 'mssql-tds-bench\perf-lab\sql-config-dump.sql'
$script:CompareScript = Join-Path $RepoRoot '.pipeline\scripts\compare-odbc-benchmarks.py'
$HarnessBuildDir = Join-Path $RepoRoot 'target\odbc-bench'
$CandidateTargetDir = Join-Path $RepoRoot 'target\odbc-candidate'
$BaselineTargetDir = Join-Path $RepoRoot 'target\odbc-baseline'
$CandidateDriverName = 'MSSQL Rust ODBC Perf Candidate'
$BaselineDriverName = 'MSSQL Rust ODBC Perf Baseline'
$script:MicrosoftDriverName = 'ODBC Driver 18 for SQL Server'
$script:OdbcInstRoot = 'HKLM:\Software\ODBC\ODBCINST.INI'
$script:DriversRegKey = "$script:OdbcInstRoot\ODBC Drivers"
$script:Repetitions = Get-PositiveIntEnv -Name 'ODBC_BENCH_REPETITIONS' -Default 5
# Confirmation defaults match the fixed-baseline mssql-tds runner: four targeted
# re-runs, reproduction required in a majority (3 of 4).
$ConfirmRuns = Get-PositiveIntEnv -Name 'ODBC_BENCH_CONFIRM_RUNS' -Default 4
$ConfirmQuorum = Get-PositiveIntEnv -Name 'ODBC_BENCH_CONFIRM_QUORUM' `
    -Default ([int][math]::Floor($ConfirmRuns / 2) + 1)
if ($ConfirmQuorum -gt $ConfirmRuns) {
    throw 'ODBC_BENCH_CONFIRM_QUORUM must not exceed ODBC_BENCH_CONFIRM_RUNS'
}
$ImprovementMax = Get-PositiveIntEnv -Name 'ODBC_BENCH_IMPROVEMENT_VERIFY_MAX' -Default 3
$RegressionRatio = Get-RatioEnv -Name 'ODBC_BENCH_REGRESSION_RATIO' -Default 1.05
$ImprovementRatio = Get-RatioEnv -Name 'ODBC_BENCH_IMPROVEMENT_VERIFY_RATIO' `
    -Default $RegressionRatio
# A confirmed candidate-vs-pinned-baseline regression fails the run by default;
# set ODBC_BENCH_FAIL_ON_REGRESSION=0 to publish the report without gating.
$GateEnabled = $env:ODBC_BENCH_FAIL_ON_REGRESSION -ne '0'

if (-not $env:SQL_SERVER) {
    throw 'SQL_SERVER not set'
}
if (-not $env:SQL_PASSWORD) {
    throw 'SQL_PASSWORD not set'
}

if (-not $env:ODBC_BENCH_SERVER) { $env:ODBC_BENCH_SERVER = $env:SQL_SERVER }
if (-not $env:ODBC_BENCH_DATABASE) { $env:ODBC_BENCH_DATABASE = 'tempdb' }
if (-not $env:ODBC_BENCH_UID) {
    $env:ODBC_BENCH_UID = if ($env:DB_USERNAME) { $env:DB_USERNAME } else { 'sa' }
}
if (-not $env:ODBC_BENCH_PWD) { $env:ODBC_BENCH_PWD = $env:SQL_PASSWORD }
if (-not $env:ODBC_BENCH_TRUST_CERT) { $env:ODBC_BENCH_TRUST_CERT = 'Yes' }
if (-not $env:ODBC_BENCH_ENCRYPT) { $env:ODBC_BENCH_ENCRYPT = 'Mandatory' }
if (-not $env:ODBC_BENCH_PACKET_SIZE) { $env:ODBC_BENCH_PACKET_SIZE = '32768' }

New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

$script:TelemetryCsv = Join-Path $ResultsDir 'cpu-telemetry.csv'
'timestamp,label,pct_processor_performance,base_freq_mhz,eff_freq_mhz,cpu_busy_pct,temp_c' |
    Set-Content -Path $script:TelemetryCsv -Encoding utf8

# --- Large-buffer and scheduling controls (Windows equivalents) ---
# Each retrieval allocates bound rowset buffers for up to 600 columns by 1024
# rows. glibc's MALLOC_MMAP_THRESHOLD_/MALLOC_TRIM_THRESHOLD_ used by the Linux
# runner have no Windows counterpart that a parent process can set, so the
# Windows side targets the same goal - stable large-allocation cost and stable
# clocks - with the controls that do exist: keep the child off the debug heap
# (which adds per-allocation bookkeeping to exactly these multi-MB buffers), run
# at High priority so the fetch loop is not preempted by agent housekeeping, and
# ask for the High performance power scheme so the CPU frequency this run
# records stays flat. Priority and power scheme are restored in the finally
# block; both are best effort.
$PreviousNoDebugHeapExists = Test-Path Env:_NO_DEBUG_HEAP
$PreviousNoDebugHeap = $env:_NO_DEBUG_HEAP
$PreviousPriority = $null
$PreviousPowerScheme = $null
$PreviousAffinity = $null

# --- No connection-churn network tuning here (deliberate) ---
# mssql-tds-bench widens the ephemeral port range and enables TIME_WAIT reuse
# because its concurrent_connects benchmark opens tens of thousands of
# short-lived TCP connections. This harness opens ONE connection in OdbcSession,
# holds it for the whole process, and measures only statement execution and
# fetching, so there is no port pressure to relieve.

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host '>>> Installing Rust toolchain...'
    & (Join-Path $RepoRoot '.pipeline\scripts\InstallRustup.ps1')
}
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo not found after Rust setup'
}
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    throw 'git not found'
}
$CMake = Find-CMake
$PythonCommand = Get-Command python -ErrorAction SilentlyContinue
if (-not $PythonCommand) {
    $PythonCommand = Get-Command python3 -ErrorAction SilentlyContinue
}
if (-not $PythonCommand) {
    throw 'Python 3 not found'
}
$script:BenchPython = $PythonCommand.Source

if (-not (Test-Path -LiteralPath $BaselineFile)) {
    throw "Baseline file not found: $BaselineFile"
}
$BaselineCommit = (Get-Content -LiteralPath $BaselineFile |
    Where-Object { $_ -notmatch '^\s*(#|$)' } |
    Select-Object -First 1).Trim()
if ($BaselineCommit -notmatch '^[0-9a-fA-F]{40}$') {
    throw "Invalid baseline commit in $BaselineFile"
}
Invoke-Native { git rev-parse --verify --quiet "$BaselineCommit^{commit}" *> $null }

if (-not (Test-Path -LiteralPath $ReferenceVersionFile)) {
    throw "Reference version file not found: $ReferenceVersionFile"
}
$MicrosoftVersion = (Get-Content -LiteralPath $ReferenceVersionFile |
    Where-Object { $_ -notmatch '^\s*(#|$)' } |
    Select-Object -First 1).Trim()
if ($MicrosoftVersion -notmatch '^[0-9]+(\.[0-9]+){3}$') {
    throw "Invalid Microsoft ODBC version in $ReferenceVersionFile"
}
& (Join-Path $RepoRoot '.pipeline\scripts\install-msodbcsql.ps1') `
    -Version $MicrosoftVersion
$MicrosoftKey = "$script:OdbcInstRoot\$script:MicrosoftDriverName"
if (-not (Test-Path -LiteralPath $MicrosoftKey)) {
    throw "'$script:MicrosoftDriverName' was not registered after installation"
}
$MicrosoftDriver = (Get-ItemProperty -LiteralPath $MicrosoftKey -Name 'Driver').Driver
if (-not $MicrosoftDriver -or -not (Test-Path -LiteralPath $MicrosoftDriver)) {
    throw "Registered Microsoft ODBC driver path is invalid: $MicrosoftDriver"
}
$MicrosoftDriverInfo = (Get-Item -LiteralPath $MicrosoftDriver).VersionInfo
$MicrosoftDriverSha256 = (Get-FileHash -LiteralPath $MicrosoftDriver -Algorithm SHA256).Hash

# --- SQL Server configuration snapshot (validate the instance is tuned) ---
# Memory, MAXDOP, cost threshold, affinity, tempdb placement, recovery, and trace
# flags. Best-effort: a missing snapshot must not cost a whole lab run.
$SqlcmdExe = Find-Sqlcmd
if ($SqlcmdExe -and (Test-Path -LiteralPath $SqlConfigSql)) {
    Write-Host '>>> Capturing SQL Server configuration snapshot...'
    Invoke-CleanupNative -Description 'SQL config snapshot' -Command {
        & $SqlcmdExe -S $env:ODBC_BENCH_SERVER -U $env:ODBC_BENCH_UID `
            -P $env:ODBC_BENCH_PWD -C -b -y 0 -Y 30 -i $SqlConfigSql |
            Tee-Object -FilePath (Join-Path $ResultsDir 'sql-config.txt')
    }
} else {
    Write-Host '>>> Skipping SQL config snapshot (sqlcmd or query file not found).'
}

$BaselineTempDir = Join-Path ([System.IO.Path]::GetTempPath()) "odbc-perf-$([System.Guid]::NewGuid().ToString('N'))"
$BaselineTree = Join-Path $BaselineTempDir 'worktree'
$CandidateState = $null
$BaselineState = $null
$CandidateRegistrationAttempted = $false
$BaselineRegistrationAttempted = $false
$TableCleanupArmed = $false
$script:BenchExe = ''
$AdminExe = ''
$HarnessRuntimeDir = Join-Path $HarnessBuildDir 'Release'

try {
    $env:_NO_DEBUG_HEAP = '1'
    Write-Host '>>> Building the fixed C++ benchmark harness...'
    try {
        Invoke-Native {
            & $CMake -S (Join-Path $RepoRoot 'mssql-odbc-bench') `
                -B $HarnessBuildDir -G 'Visual Studio 17 2022' -A x64
        }
    } catch {
        $gxx = Get-Command g++ -ErrorAction SilentlyContinue
        $ninja = Get-Command ninja -ErrorAction SilentlyContinue
        if (-not $gxx -or -not $ninja) {
            throw
        }
        Write-Host '>>> Visual Studio C++ tools unavailable; using MinGW and Ninja...'
        Remove-Item -LiteralPath $HarnessBuildDir -Recurse -Force -ErrorAction SilentlyContinue
        $HarnessRuntimeDir = $HarnessBuildDir
        $env:PATH = "$([System.IO.Path]::GetDirectoryName($gxx.Source));$env:PATH"
        Invoke-Native {
            & $CMake -S (Join-Path $RepoRoot 'mssql-odbc-bench') `
                -B $HarnessBuildDir -G Ninja -DCMAKE_BUILD_TYPE=Release `
                "-DCMAKE_CXX_COMPILER=$($gxx.Source)"
        }
    }
    Invoke-Native { & $CMake --build $HarnessBuildDir --config Release --parallel }

    # The pinned Google Benchmark v1.9.1 checkout ships the comparator we report
    # with; using the copy from this build tree keeps the tool and the harness on
    # the same version.
    $GbenchCompare = Join-Path $HarnessBuildDir '_deps\googlebenchmark-src\tools\compare.py'
    if (-not (Test-Path -LiteralPath $GbenchCompare)) {
        throw "Google Benchmark comparator not found: $GbenchCompare"
    }
    $GbenchArguments = @('--gbench-compare', $GbenchCompare)
    $StatsPython = Initialize-BenchmarkPython -Python $script:BenchPython `
        -VenvRoot (Join-Path $RepoRoot 'target\odbc-bench-venv')
    if (-not $StatsPython) {
        throw "NumPy/SciPy are required by Google Benchmark's compare.py"
    }
    $script:BenchPython = $StatsPython

    Build-Driver -SourceRoot $RepoRoot -TargetDir $CandidateTargetDir -Label 'candidate'

    New-Item -ItemType Directory -Force -Path $BaselineTempDir | Out-Null
    Write-Host ">>> Adding baseline worktree for $BaselineCommit..."
    Invoke-Native { git worktree add --detach $BaselineTree $BaselineCommit }
    Build-Driver -SourceRoot $BaselineTree -TargetDir $BaselineTargetDir -Label 'baseline'

    $CandidateDriver = Join-Path $CandidateTargetDir 'release\msodbcsql18.dll'
    $BaselineDriver = Join-Path $BaselineTargetDir 'release\msodbcsql18.dll'
    $script:BenchExe = Join-Path $HarnessRuntimeDir 'mssql_odbc_bench.exe'
    $AdminExe = Join-Path $HarnessRuntimeDir 'mssql_odbc_bench_admin.exe'
    foreach ($requiredFile in @(
            $CandidateDriver,
            $BaselineDriver,
            $MicrosoftDriver,
            $script:BenchExe,
            $AdminExe
        )) {
        if (-not (Test-Path -LiteralPath $requiredFile)) {
            throw "Expected build output not found: $requiredFile"
        }
    }

    $CandidateState = Save-DriverRegistration -Name $CandidateDriverName
    $CandidateRegistrationAttempted = $true
    Set-DriverRegistration -Name $CandidateDriverName -DriverPath $CandidateDriver
    $BaselineState = Save-DriverRegistration -Name $BaselineDriverName
    $BaselineRegistrationAttempted = $true
    Set-DriverRegistration -Name $BaselineDriverName -DriverPath $BaselineDriver

    $CandidateCommit = (Invoke-Native { git rev-parse HEAD } | Out-String).Trim()
    $RustcVersion = (Invoke-Native { rustc -Vv } | Out-String).TrimEnd()
    $CargoVersion = (Invoke-Native { cargo -V } | Out-String).TrimEnd()
    $CMakeVersion = (Invoke-Native { & $CMake --version } | Out-String).TrimEnd()
    $context = @(
        "candidate_commit=$CandidateCommit",
        "baseline_commit=$BaselineCommit",
        "microsoft_driver_version=$MicrosoftVersion",
        "microsoft_driver_product_version=$($MicrosoftDriverInfo.ProductVersion)",
        "microsoft_driver_path=$MicrosoftDriver",
        "microsoft_driver_sha256=$MicrosoftDriverSha256",
        "repetitions=$script:Repetitions",
        "regression_ratio=$(Format-Invariant $RegressionRatio 'F4')",
        "confirm_runs=$ConfirmRuns",
        "confirm_quorum=$ConfirmQuorum",
        "gbench_compare=$(if ($GbenchArguments.Count) { $GbenchCompare } else { 'disabled' })",
        "bench_python=$script:BenchPython",
        "timestamp_utc=$([DateTime]::UtcNow.ToString('o'))",
        $RustcVersion,
        $CargoVersion,
        $CMakeVersion
    ) -join "`n"
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText(
        (Join-Path $ResultsDir 'odbc-context.txt'),
        $context + "`n",
        $utf8NoBom
    )

    Write-Host '>>> Creating deterministic benchmark tables...'
    $TableCleanupArmed = $true
    $env:ODBC_BENCH_DRIVER = $CandidateDriverName
    $env:ODBC_BENCH_SCENARIO = ''
    Invoke-Native { & $AdminExe setup }

    $cpuList = if ($env:BENCH_CPUS) { $env:BENCH_CPUS } else { $env:PERF_CLIENT_CPUS }
    $affinity = ConvertTo-AffinityMask -CpuList $cpuList
    if ($null -ne $affinity) {
        try {
            $PreviousAffinity = (Get-Process -Id $PID).ProcessorAffinity
            (Get-Process -Id $PID).ProcessorAffinity = [IntPtr]$affinity
            Write-Host ">>> Pinned benchmark processes to CPUs: $cpuList"
        } catch {
            $PreviousAffinity = $null
            Write-Warning "Could not set benchmark CPU affinity: $($_.Exception.Message)"
        }
    }
    # Child processes inherit this process's priority class, so raising it here
    # covers every benchmark leg.
    try {
        $PreviousPriority = (Get-Process -Id $PID).PriorityClass
        (Get-Process -Id $PID).PriorityClass = 'High'
        Write-Host '>>> Benchmark processes raised to High priority'
    } catch {
        $PreviousPriority = $null
        Write-Warning "Could not raise benchmark priority: $($_.Exception.Message)"
    }
    try {
        $activeScheme = (& powercfg /getactivescheme | Out-String)
        if ($activeScheme -match '([0-9a-fA-F-]{36})') {
            $PreviousPowerScheme = $Matches[1]
            & powercfg /setactive 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c *> $null
            if ($LASTEXITCODE -eq 0) {
                Write-Host '>>> Active power scheme set to High performance'
            } else {
                $PreviousPowerScheme = $null
            }
        }
    } catch {
        $PreviousPowerScheme = $null
        Write-Warning "Could not set the High performance power scheme: $($_.Exception.Message)"
    }

    $candidateNarrow = Join-Path $ResultsDir 'odbc-candidate-narrow.json'
    $baselineNarrow = Join-Path $ResultsDir 'odbc-baseline-narrow.json'
    $candidateWide = Join-Path $ResultsDir 'odbc-candidate-wide.json'
    $baselineWide = Join-Path $ResultsDir 'odbc-baseline-wide.json'
    $microsoftNarrow = Join-Path $ResultsDir 'odbc-microsoft-narrow.json'
    $microsoftWide = Join-Path $ResultsDir 'odbc-microsoft-wide.json'

    Write-CpuSample 'initial-start'
    Invoke-BenchmarkLeg -Scenario 'narrow' -Driver $CandidateDriverName -Output $candidateNarrow
    Invoke-BenchmarkLeg -Scenario 'narrow' -Driver $script:MicrosoftDriverName -Output $microsoftNarrow
    Invoke-BenchmarkLeg -Scenario 'narrow' -Driver $BaselineDriverName -Output $baselineNarrow
    Invoke-BenchmarkLeg -Scenario 'wide' -Driver $BaselineDriverName -Output $baselineWide
    Invoke-BenchmarkLeg -Scenario 'wide' -Driver $script:MicrosoftDriverName -Output $microsoftWide
    Invoke-BenchmarkLeg -Scenario 'wide' -Driver $CandidateDriverName -Output $candidateWide
    Write-CpuSample 'initial-end'

    $threeWayArguments = @(
        '--baseline', $baselineNarrow,
        '--baseline', $baselineWide,
        '--candidate', $candidateNarrow,
        '--candidate', $candidateWide,
        '--reference', $microsoftNarrow,
        '--reference', $microsoftWide,
        '--reference-label', "Microsoft ODBC $MicrosoftVersion",
        '--baseline-commit', $BaselineCommit,
        '--reference-version', $MicrosoftVersion,
        '--repetitions', [string]$script:Repetitions,
        '--regression-ratio', (Format-Invariant $RegressionRatio),
        '--improvement-ratio', (Format-Invariant $ImprovementRatio),
        '--improvement-max', [string]$ImprovementMax
    ) + $GbenchArguments

    # --- Initial pass: five-sample medians pick what deserves re-measurement ---
    # The initial verdict never gates on its own; it only produces the plan.
    Write-Host '>>> Comparing the initial three-driver pass...'
    $initialExit = Invoke-Comparator -Arguments ($threeWayArguments + @(
            '--output-dir', $InitialDir,
            '--plan-out', $PlanFile,
            '--no-summary',
            '--no-fail-on-regression'
        ))
    if ($initialExit -ne 0) {
        throw "Initial ODBC benchmark comparison failed (exit $initialExit)"
    }

    $scenarioFiles = @{ narrow = $candidateNarrow; wide = $candidateWide }
    $plan = @()
    foreach ($line in Get-Content -LiteralPath $PlanFile) {
        if (-not $line.Trim()) { continue }
        $fields = $line -split "`t"
        $entry = [pscustomobject]@{
            Kind = $fields[0]
            Name = $fields[1]
            Scenario = Get-BenchmarkScenario -Name $fields[1] -ScenarioFiles $scenarioFiles
        }
        Write-Host ">>> Initial pass flagged $($entry.Kind): $($entry.Name) (ratio $($fields[2]))"
        $plan += $entry
    }

    $confirmationArguments = @()
    if ($plan.Count -gt 0) {
        # One process covers both scenarios; only narrow those legs when the other
        # workload has nothing to confirm.
        $selected = @($plan | ForEach-Object { $_.Scenario } | Sort-Object -Unique)
        $confirmScenario = if ($selected.Count -eq 1) { $selected[0] } else { '' }

        Write-Host ((">>> Auto-confirm: re-measuring {0} benchmark(s) over {1} round(s); " +
                "a result counts only when it reproduces in >= {2} of {1}.") -f
            $plan.Count, $ConfirmRuns, $ConfirmQuorum)
        $roundRatios = @()
        for ($round = 1; $round -le $ConfirmRuns; $round++) {
            $roundDir = Join-Path $ConfirmDir "round$round"
            New-Item -ItemType Directory -Force -Path $roundDir | Out-Null
            Write-Host ">>> Confirmation round $round/$ConfirmRuns..."
            Write-CpuSample "confirm$round-start"
            # Keep each pair adjacent, and alternate which side runs first so a
            # stable position effect cancels across the default four rounds.
            if ($round % 2 -eq 1) {
                Invoke-BenchmarkLeg -Scenario $confirmScenario -Driver $CandidateDriverName `
                    -Output (Join-Path $roundDir 'candidate.json')
                Invoke-BenchmarkLeg -Scenario $confirmScenario -Driver $BaselineDriverName `
                    -Output (Join-Path $roundDir 'baseline.json')
            } else {
                Invoke-BenchmarkLeg -Scenario $confirmScenario -Driver $BaselineDriverName `
                    -Output (Join-Path $roundDir 'baseline.json')
                Invoke-BenchmarkLeg -Scenario $confirmScenario -Driver $CandidateDriverName `
                    -Output (Join-Path $roundDir 'candidate.json')
            }
            Write-CpuSample "confirm$round-end"
            $roundArguments = @(
                '--baseline', (Join-Path $roundDir 'baseline.json'),
                '--candidate', (Join-Path $roundDir 'candidate.json'),
                '--repetitions', [string]$script:Repetitions,
                '--regression-ratio', (Format-Invariant $RegressionRatio),
                '--improvement-ratio', (Format-Invariant $ImprovementRatio),
                '--output-dir', $roundDir,
                '--ratios-out', (Join-Path $roundDir 'ratios.txt'),
                '--no-summary',
                '--no-fail-on-regression'
            ) + $GbenchArguments
            $previousPreference = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            try {
                & $script:BenchPython $script:CompareScript @roundArguments *>&1 |
                    Out-File -FilePath (Join-Path $roundDir 'comparison.log') -Encoding utf8
                $roundExit = $LASTEXITCODE
            } finally {
                $ErrorActionPreference = $previousPreference
            }
            if ($roundExit -ne 0) {
                throw "Confirmation round $round comparison failed (exit $roundExit)"
            }
            $roundRatios += , (Read-RatioFile -Path (Join-Path $roundDir 'ratios.txt'))
        }

        foreach ($entry in $plan) {
            $observed = @()
            foreach ($table in $roundRatios) {
                if ($table.ContainsKey($entry.Name)) {
                    $observed += $table[$entry.Name]
                }
            }
            if ($observed.Count -ne $ConfirmRuns) {
                throw ("Expected $ConfirmRuns confirmation ratios for '$($entry.Name)'; " +
                    "found $($observed.Count)")
            }
            # Median of the confirmation rounds only. The initial pass is excluded
            # on purpose: a benchmark is re-measured because that pass was extreme,
            # so letting it vote again would let the outlier under test decide its
            # own verdict and could contradict a quorum that cleared it.
            $tally = Get-ConfirmationTally -Kind $entry.Kind -Ratios $observed `
                -RegressionRatio $RegressionRatio -ImprovementRatio $ImprovementRatio
            Write-Host (((">>> {0}: reproduced {1}/{2} in the initial direction, " +
                    "regressed {3}/{2}, confirmation median {4}") -f
                    $entry.Name, $tally.Hits, $ConfirmRuns, $tally.RegressionHits,
                    (Format-Invariant $tally.Median)))
            $confirmationArguments += @(
                '--confirmation', $entry.Name, [string]$tally.Hits,
                [string]$tally.RegressionHits,
                (Format-Invariant $tally.Median)
            )
        }
    }

    $finalArguments = $threeWayArguments + @(
        '--output-dir', $ResultsDir,
        '--confirm-runs', [string]$ConfirmRuns,
        '--confirm-quorum', [string]$ConfirmQuorum
    ) + $confirmationArguments
    if (-not $GateEnabled) {
        $finalArguments += '--no-fail-on-regression'
    }
    # Exit 2 means the report was written and the gate tripped. Preserve it until
    # after the report reaches the step log.
    $compareExitCode = Invoke-Comparator -Arguments $finalArguments
    if ($compareExitCode -ne 0 -and $compareExitCode -ne 2) {
        throw "ODBC benchmark comparison failed (exit $compareExitCode)"
    }

    Write-Host ''
    Write-Host '===== summary.md ====='
    Get-Content -LiteralPath (Join-Path $ResultsDir 'summary.md') | ForEach-Object { Write-Host $_ }
    Write-Host '===== end summary.md ====='
    Write-Host ">>> ODBC benchmark results written to $ResultsDir"
    if ($compareExitCode -eq 2) {
        throw 'ODBC benchmark regression gate failed; see summary.md above'
    }
} finally {
    if ($TableCleanupArmed -and (Test-Path -LiteralPath $AdminExe)) {
        Write-Host '>>> Removing ODBC benchmark tables...'
        $env:ODBC_BENCH_DRIVER = $CandidateDriverName
        $env:ODBC_BENCH_SCENARIO = ''
        Invoke-CleanupNative -Description 'Benchmark table cleanup' -Command { & $AdminExe cleanup }
    }
    if ($BaselineRegistrationAttempted) {
        try { Restore-DriverRegistration -State $BaselineState } catch { Write-Warning $_ }
    }
    if ($CandidateRegistrationAttempted) {
        try { Restore-DriverRegistration -State $CandidateState } catch { Write-Warning $_ }
    }
    if ($PreviousPowerScheme) {
        Invoke-CleanupNative -Description 'Power scheme restore' -Command {
            & powercfg /setactive $PreviousPowerScheme
        }
    }
    if ($PreviousPriority) {
        try { (Get-Process -Id $PID).PriorityClass = $PreviousPriority } catch { Write-Warning $_ }
    }
    if ($null -ne $PreviousAffinity) {
        try { (Get-Process -Id $PID).ProcessorAffinity = $PreviousAffinity } catch { Write-Warning $_ }
    }
    if ($PreviousNoDebugHeapExists) {
        $env:_NO_DEBUG_HEAP = $PreviousNoDebugHeap
    } else {
        Remove-Item Env:_NO_DEBUG_HEAP -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $BaselineTree) {
        Invoke-CleanupNative -Description 'Baseline worktree removal' -Command {
            git worktree remove --force $BaselineTree
        }
    }
    if (Test-Path -LiteralPath $BaselineTempDir) {
        Remove-Item -LiteralPath $BaselineTempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
