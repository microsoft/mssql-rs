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

function Invoke-BenchmarkLeg {
    # Keep one raw JSON file per driver/scenario and use the packet-size spelling
    # accepted by that driver.
    param(
        [Parameter(Mandatory)][string]$Scenario,
        [Parameter(Mandatory)][string]$Driver,
        [Parameter(Mandatory)][string]$Output
    )

    Write-Host ">>> Running $Scenario with $Driver..."
    $env:ODBC_BENCH_DRIVER = $Driver
    $env:ODBC_BENCH_SCENARIO = $Scenario
    $env:ODBC_BENCH_PACKET_SIZE_KEYWORD = if ($Driver -eq $MicrosoftDriverName) {
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

$RepoRoot = (Get-Location).Path
$ResultsDir = Join-Path $RepoRoot 'results'
$BaselineFile = Join-Path $RepoRoot 'mssql-odbc-bench\perf-lab\baseline-commit.txt'
$ReferenceVersionFile = Join-Path $RepoRoot 'mssql-odbc-bench\perf-lab\msodbcsql-version.txt'
$HarnessBuildDir = Join-Path $RepoRoot 'target\odbc-bench'
$CandidateTargetDir = Join-Path $RepoRoot 'target\odbc-candidate'
$BaselineTargetDir = Join-Path $RepoRoot 'target\odbc-baseline'
$CandidateDriverName = 'MSSQL Rust ODBC Perf Candidate'
$BaselineDriverName = 'MSSQL Rust ODBC Perf Baseline'
$MicrosoftDriverName = 'ODBC Driver 18 for SQL Server'
$script:OdbcInstRoot = 'HKLM:\Software\ODBC\ODBCINST.INI'
$script:DriversRegKey = "$script:OdbcInstRoot\ODBC Drivers"
$script:Repetitions = Get-PositiveIntEnv -Name 'ODBC_BENCH_REPETITIONS' -Default 5

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
$Python = $PythonCommand.Source

if (-not (Test-Path -LiteralPath $BaselineFile)) {
    throw "Baseline file not found: $BaselineFile"
}
$BaselineCommit = (Get-Content -LiteralPath $BaselineFile |
    Where-Object { $_ -notmatch '^\s*(#|$)' } |
    Select-Object -First 1).Trim()
if ($BaselineCommit -notmatch '^[0-9a-fA-F]{40}$') {
    throw "Invalid baseline commit in $BaselineFile"
}
& git rev-parse --verify --quiet "$BaselineCommit^{commit}" *> $null
if ($LASTEXITCODE -ne 0) {
    throw "Baseline commit $BaselineCommit is absent from the checkout"
}

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
$MicrosoftKey = "$script:OdbcInstRoot\$MicrosoftDriverName"
if (-not (Test-Path -LiteralPath $MicrosoftKey)) {
    throw "'$MicrosoftDriverName' was not registered after installation"
}
$MicrosoftDriver = (Get-ItemProperty -LiteralPath $MicrosoftKey -Name 'Driver').Driver
if (-not $MicrosoftDriver -or -not (Test-Path -LiteralPath $MicrosoftDriver)) {
    throw "Registered Microsoft ODBC driver path is invalid: $MicrosoftDriver"
}
$MicrosoftDriverInfo = (Get-Item -LiteralPath $MicrosoftDriver).VersionInfo
$MicrosoftDriverSha256 = (Get-FileHash -LiteralPath $MicrosoftDriver -Algorithm SHA256).Hash

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

    $context = @(
        "candidate_commit=$(& git rev-parse HEAD)",
        "baseline_commit=$BaselineCommit",
        "microsoft_driver_version=$MicrosoftVersion",
        "microsoft_driver_product_version=$($MicrosoftDriverInfo.ProductVersion)",
        "microsoft_driver_path=$MicrosoftDriver",
        "microsoft_driver_sha256=$MicrosoftDriverSha256",
        "repetitions=$script:Repetitions",
        "timestamp_utc=$([DateTime]::UtcNow.ToString('o'))",
        (& rustc -Vv | Out-String).TrimEnd(),
        (& cargo -V | Out-String).TrimEnd(),
        (& $CMake --version | Out-String).TrimEnd()
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
            (Get-Process -Id $PID).ProcessorAffinity = [IntPtr]$affinity
            Write-Host ">>> Pinned benchmark processes to CPUs: $cpuList"
        } catch {
            Write-Warning "Could not set benchmark CPU affinity: $($_.Exception.Message)"
        }
    }

    $candidateNarrow = Join-Path $ResultsDir 'odbc-candidate-narrow.json'
    $baselineNarrow = Join-Path $ResultsDir 'odbc-baseline-narrow.json'
    $candidateWide = Join-Path $ResultsDir 'odbc-candidate-wide.json'
    $baselineWide = Join-Path $ResultsDir 'odbc-baseline-wide.json'
    $microsoftNarrow = Join-Path $ResultsDir 'odbc-microsoft-narrow.json'
    $microsoftWide = Join-Path $ResultsDir 'odbc-microsoft-wide.json'

    Invoke-BenchmarkLeg -Scenario 'narrow' -Driver $CandidateDriverName -Output $candidateNarrow
    Invoke-BenchmarkLeg -Scenario 'narrow' -Driver $MicrosoftDriverName -Output $microsoftNarrow
    Invoke-BenchmarkLeg -Scenario 'narrow' -Driver $BaselineDriverName -Output $baselineNarrow
    Invoke-BenchmarkLeg -Scenario 'wide' -Driver $BaselineDriverName -Output $baselineWide
    Invoke-BenchmarkLeg -Scenario 'wide' -Driver $MicrosoftDriverName -Output $microsoftWide
    Invoke-BenchmarkLeg -Scenario 'wide' -Driver $CandidateDriverName -Output $candidateWide

    $compareArguments = @(
        (Join-Path $RepoRoot '.pipeline\scripts\compare-odbc-benchmarks.py'),
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
        '--output-dir', $ResultsDir,
        '--regression-ratio',
        $(if ($env:ODBC_BENCH_REGRESSION_RATIO) { $env:ODBC_BENCH_REGRESSION_RATIO } else { '1.10' })
    )
    if ($env:ODBC_BENCH_FAIL_ON_REGRESSION -eq '1') {
        $compareArguments += '--fail-on-regression'
    }
    # Exit 2 means the report was written and the optional gate tripped. Preserve
    # it until after the report reaches the step log.
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Python @compareArguments
        $compareExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousPreference
    }
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
    if (Test-Path -LiteralPath $BaselineTree) {
        Invoke-CleanupNative -Description 'Baseline worktree removal' -Command {
            git worktree remove --force $BaselineTree
        }
    }
    if (Test-Path -LiteralPath $BaselineTempDir) {
        Remove-Item -LiteralPath $BaselineTempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
