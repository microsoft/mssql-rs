# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the Rust ODBC driver and the C++ Google Benchmark suite, then run the
# same benchmark binaries against mssql-odbc and msodbcsql18 and print a
# side-by-side comparison.
#
# Unlike run_e2e.ps1, this script does NOT require Administrator. Both drivers
# are selected by the `Driver={...}` name in the connection string, so a
# comparison run never edits the registry — it only refreshes the DLL that the
# already-registered Rust driver entry points at.
#
# Prerequisites (one-time, requires Administrator):
#   The Rust driver must be registered under -RustDriver pointing at a path this
#   user can overwrite, e.g.
#     HKLM\Software\ODBC\ODBCINST.INI\mssql-odbc dev
#         Driver = C:\odbc-dev\mssql-odbc-dev.dll
#         Setup  = C:\odbc-dev\mssql-odbc-dev.dll
#     HKLM\Software\ODBC\ODBCINST.INI\ODBC Drivers
#         mssql-odbc dev = Installed
#
# Usage:
#   .\run_perf.ps1
#   .\run_perf.ps1 -Bench fetch_bench -MinTime 2.0
#   .\run_perf.ps1 -SkipBuild -Filter 'BM_Fetch.*'

param(
    # Driver name (as registered with the Driver Manager) for the Rust driver.
    [string]$RustDriver = 'mssql-odbc dev',
    # Reference driver to compare against. Empty string runs the Rust driver only.
    [string]$RefDriver = 'ODBC Driver 18 for SQL Server',
    [string]$Server = 'localhost',
    [string]$Database = 'odbcperf',
    [string]$Uid = '',
    [string]$Pwd = '',
    # Restrict to one benchmark binary (connect_bench|exec_bench|fetch_bench|datatype_bench).
    [string]$Bench = '',
    # Google Benchmark --benchmark_filter regex.
    [string]$Filter = '',
    # Seconds of measurement per benchmark case.
    [double]$MinTime = 1.0,
    # Repetitions per case; >1 makes the median/stddev columns meaningful.
    [int]$Repetitions = 3,
    [switch]$SkipBuild,
    [string]$ResultsDir = ''
)

$ErrorActionPreference = 'Stop'

$ScriptDir    = Split-Path -Parent $MyInvocation.MyCommand.Definition
$OdbcCrateDir = Resolve-Path (Join-Path $ScriptDir '..\..')
$WorkspaceDir = Resolve-Path (Join-Path $OdbcCrateDir '..')
$BuildDir     = Join-Path $ScriptDir 'build'

if (-not $ResultsDir) { $ResultsDir = Join-Path $ScriptDir 'results' }
New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

$AllBenches = @('connect_bench', 'exec_bench', 'fetch_bench', 'datatype_bench')
$Benches = if ($Bench) { @($Bench) } else { $AllBenches }

function Write-Section([string]$msg) {
    Write-Host ''
    Write-Host "=== $msg ===" -ForegroundColor Cyan
}

# ---------------------------------------------------------------------------
# Resolve where the registered Rust driver DLL lives
# ---------------------------------------------------------------------------
function Get-RegisteredDriverPath([string]$name) {
    $key = "HKLM:\Software\ODBC\ODBCINST.INI\$name"
    if (-not (Test-Path $key)) { return $null }
    $v = Get-ItemProperty -Path $key -Name 'Driver' -ErrorAction SilentlyContinue
    if ($null -eq $v) { return $null }
    return $v.Driver
}

$RustDriverPath = Get-RegisteredDriverPath $RustDriver
if (-not $RustDriverPath) {
    throw "Driver '$RustDriver' is not registered. See the header of this script for the one-time registration steps."
}

if ($RefDriver) {
    $RefDriverPath = Get-RegisteredDriverPath $RefDriver
    if (-not $RefDriverPath) {
        throw "Reference driver '$RefDriver' is not registered."
    }
}

# ---------------------------------------------------------------------------
# Locate the MSVC toolchain. CMake's Visual Studio generator cannot always
# discover the compiler on a plain shell, so the C++ build runs inside a
# vcvars64 environment with an explicit toolset that has the CRT libraries.
# ---------------------------------------------------------------------------
function Get-VcVarsPath {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { throw 'vswhere.exe not found; install Visual Studio 2022 with the C++ workload.' }
    $root = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $root) { throw 'No Visual Studio installation with the C++ toolset was found.' }
    $vcvars = Join-Path $root 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found under $root" }
    return $vcvars
}

# vcvars64 defaults to the newest *installed* toolset, which is not necessarily
# one with the CRT import libraries present. Pick the highest toolset that
# actually has msvcrtd.lib so the CMake compiler probe links.
function Get-VcToolsetVersion([string]$vcvars) {
    $msvcRoot = Join-Path (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $vcvars))) 'Tools\MSVC'
    if (-not (Test-Path $msvcRoot)) { return '' }
    $candidate = Get-ChildItem $msvcRoot |
        Where-Object { Test-Path (Join-Path $_.FullName 'lib\x64\msvcrtd.lib') } |
        Sort-Object Name -Descending |
        Select-Object -First 1
    if (-not $candidate) { return '' }
    # vcvars expects a major.minor version (e.g. 14.44), not the full toolset number.
    $parts = $candidate.Name.Split('.')
    return "$($parts[0]).$($parts[1])"
}

function Invoke-InVcVars([string]$command) {
    $vcvars = Get-VcVarsPath
    $ver = Get-VcToolsetVersion $vcvars
    $verArg = if ($ver) { " -vcvars_ver=$ver" } else { '' }
    & $env:ComSpec /c "call `"$vcvars`"$verArg >nul && $command"
    if ($LASTEXITCODE -ne 0) { throw "Command failed under vcvars: $command" }
}

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
if (-not $SkipBuild) {
    Write-Section 'Building Rust driver (release)'
    Push-Location $OdbcCrateDir
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally {
        Pop-Location
    }

    $BuiltDll = Join-Path $WorkspaceDir 'target\release\msodbcsql18.dll'
    if (-not (Test-Path $BuiltDll)) { throw "Built driver not found at $BuiltDll" }

    Write-Host "[  DRIVER ] Copying $BuiltDll -> $RustDriverPath"
    Copy-Item -Path $BuiltDll -Destination $RustDriverPath -Force

    Write-Section 'Building C++ benchmarks (Release)'
    Invoke-InVcVars "cd /d `"$ScriptDir`" && cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release -DODBC_PERF_FORCE_UNICODE=ON"
    Invoke-InVcVars "cd /d `"$ScriptDir`" && cmake --build build"
}

# ---------------------------------------------------------------------------
# Run one benchmark binary against one driver
# ---------------------------------------------------------------------------
function Invoke-Bench([string]$benchName, [string]$driverName, [string]$label) {
    $exe = Join-Path $BuildDir "Release\$benchName.exe"
    if (-not (Test-Path $exe)) { $exe = Join-Path $BuildDir "$benchName.exe" }
    if (-not (Test-Path $exe)) { throw "Benchmark binary not found: $benchName" }

    $out = Join-Path $ResultsDir "$benchName.$label.json"

    $env:ODBC_TEST_DRIVER   = $driverName
    $env:ODBC_TEST_SERVER   = $Server
    $env:ODBC_TEST_DATABASE = $Database
    $env:ODBC_TEST_UID      = $Uid
    $env:ODBC_TEST_PWD      = $Pwd

    $benchArgs = @(
        "--benchmark_out=$out",
        '--benchmark_out_format=json',
        "--benchmark_min_time=$($MinTime)s",
        "--benchmark_repetitions=$Repetitions",
        '--benchmark_report_aggregates_only=true'
    )
    if ($Filter) { $benchArgs += "--benchmark_filter=$Filter" }

    Write-Host "[   RUN   ] $benchName  driver='$driverName'"
    & $exe @benchArgs | Out-Host
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[  FAIL   ] $benchName exited $LASTEXITCODE for '$driverName'" -ForegroundColor Red
    }
    return $out
}

# ---------------------------------------------------------------------------
# Compare two result files
# ---------------------------------------------------------------------------
function Compare-Results([string]$rustJson, [string]$refJson) {
    if (-not (Test-Path $rustJson) -or -not (Test-Path $refJson)) { return @() }

    # Keep only median aggregates so a single row per case is compared. A case
    # that failed is recorded with a null time so it shows up as an error row
    # rather than silently disappearing from the table.
    function Read-Medians([string]$path) {
        $map = @{}
        $data = Get-Content $path -Raw | ConvertFrom-Json
        foreach ($b in $data.benchmarks) {
            $name = $b.name -replace '_(median|mean|stddev|cv)$', ''
            if ($b.PSObject.Properties.Name -contains 'error_occurred' -and $b.error_occurred) {
                $map[$name] = $null
                continue
            }
            if ($b.run_type -eq 'aggregate' -and $b.aggregate_name -ne 'median') { continue }
            if ($map.ContainsKey($name) -and $null -eq $map[$name]) { continue }
            $map[$name] = $b.real_time * (Get-TimeScale $b.time_unit)
        }
        return $map
    }

    $rust = Read-Medians $rustJson
    $ref  = Read-Medians $refJson

    $rows = @()
    foreach ($name in $rust.Keys) {
        if (-not $ref.ContainsKey($name)) { continue }
        $r = $rust[$name]
        $m = $ref[$name]
        $rows += [pscustomobject]@{
            Benchmark     = $name
            'mssql-odbc'  = if ($null -eq $r) { 'ERROR' } else { Format-Duration $r }
            'msodbcsql18' = if ($null -eq $m) { 'ERROR' } else { Format-Duration $m }
            Ratio         = if ($null -eq $r -or $null -eq $m -or $m -le 0) { '-' } else { [math]::Round($r / $m, 2) }
            Verdict       = if ($null -eq $r) { 'unsupported' }
                            elseif ($null -eq $m -or $m -le 0) { 'n/a' }
                            elseif ($r -lt $m * 0.95) { 'faster' }
                            elseif ($r -gt $m * 1.05) { 'slower' }
                            else { 'parity' }
        }
    }
    return $rows
}

# Normalize Google Benchmark time units to nanoseconds.
function Get-TimeScale([string]$unit) {
    switch ($unit) {
        'ns' { 1 }
        'us' { 1000 }
        'ms' { 1000000 }
        's'  { 1000000000 }
        default { 1 }
    }
}

function Format-Duration([double]$ns) {
    if ($ns -ge 1000000) { return "{0:N2} ms" -f ($ns / 1000000) }
    if ($ns -ge 1000)    { return "{0:N2} us" -f ($ns / 1000) }
    return "{0:N2} ns" -f $ns
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
Write-Section 'Configuration'
Write-Host "  Rust driver : $RustDriver  ($RustDriverPath)"
if ($RefDriver) { Write-Host "  Reference   : $RefDriver  ($RefDriverPath)" }
Write-Host "  Server      : $Server / $Database"
Write-Host "  Auth        : $(if ($Uid) { "SQL login '$Uid'" } else { 'Windows integrated' })"
Write-Host "  Benchmarks  : $($Benches -join ', ')"

$allRows = @()
foreach ($b in $Benches) {
    Write-Section "Benchmark: $b"
    $rustJson = Invoke-Bench $b $RustDriver 'mssql-odbc'
    if ($RefDriver) {
        $refJson = Invoke-Bench $b $RefDriver 'msodbcsql18'
        $allRows += Compare-Results $rustJson $refJson
    }
}

if ($allRows.Count -gt 0) {
    Write-Section 'Comparison (median real time; Ratio < 1.0 = mssql-odbc faster)'
    $allRows | Sort-Object { if ($_.Ratio -eq '-') { [double]::MaxValue } else { [double]$_.Ratio } } |
        Format-Table -AutoSize | Out-String | ForEach-Object { Write-Output $_ }
}

Write-Host ''
Write-Host "Raw JSON results in: $ResultsDir"
