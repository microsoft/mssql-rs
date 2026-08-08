# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build and run the fetch-throughput A/B benchmark: native
# "ODBC Driver 18 for SQL Server" vs the Rust mssql-odbc dev driver.
#
# Mirrors run_e2e.ps1's toolchain for building the Rust cdylib (msodbcsql18.dll),
# then runs the benchmark WITHOUT touching the registry: fetch_bench loads each
# driver DLL directly (its own tiny driver manager) via the `dll:<path>` leg
# syntax, so the unregistered Rust dev driver runs with zero admin rights and
# both A/B legs execute on an identical, DM-free code path (a fairer compare).
#
# Requires: a live SQL Server, msodbcsql18 installed, MSVC + CMake (VS "C++
# CMake tools" component). NO administrator needed.
#
# Usage: .\run_bench.ps1 [-Release] [-Rows N] [-Reps R] [-Warmup W]
#                        [-VcVarsVer 14.44] [-NativeDll PATH] [-RustDll PATH]
#
# Connection info resolution (matches the P8 harness contract):
#   ODBC_TEST_SERVER = <DB_HOST>,<DB_PORT>     from mssql-tds/.env
#   ODBC_TEST_UID    = <DB_USERNAME>           from mssql-tds/.env
#   ODBC_TEST_PWD    = $env:SQL_PASSWORD, else contents of C:\tmp\password
#   ODBC_TEST_DATABASE = tempdb  ODBC_TEST_TRUST_CERT = Yes
# Any ODBC_TEST_* already set in the environment is respected and not overwritten.

param(
    [switch]$Release,
    [long]$Rows = 200000,
    [int]$Reps = 9,
    [int]$Warmup = 1,
    # MSVC toolset to select in vcvars64. Some machines ship a default toolset
    # missing the x64 CRT import libs; pin a known-good one here. Set to "" to
    # use the environment's default toolset.
    [string]$VcVarsVer = "14.44",
    [string]$NativeDll = "",
    [string]$RustDll = ""
)

$ErrorActionPreference = "Stop"

$ScriptDir    = Split-Path -Parent $MyInvocation.MyCommand.Definition
$OdbcCrateDir = Resolve-Path (Join-Path $ScriptDir "..\..")
$WorkspaceDir = Resolve-Path (Join-Path $OdbcCrateDir "..")
$BuildType    = if ($Release) { "release" } else { "debug" }
$CMakeBuildType = if ($Release) { "Release" } else { "Debug" }

# Parse KEY=VALUE lines out of an .env file into a hashtable (ignores # comments).
function Read-DotEnv([string]$Path) {
    $map = @{}
    if (-not (Test-Path $Path)) { return $map }
    foreach ($line in Get-Content -Path $Path) {
        $t = $line.Trim()
        if (-not $t -or $t.StartsWith("#")) { continue }
        $eq = $t.IndexOf("=")
        if ($eq -lt 1) { continue }
        $k = $t.Substring(0, $eq).Trim()
        $v = $t.Substring($eq + 1).Trim().Trim('"').Trim("'")
        $map[$k] = $v
    }
    return $map
}

# Resolve the SQL password without ever echoing it: env var first, then file.
function Resolve-Password {
    if ($env:SQL_PASSWORD) { return $env:SQL_PASSWORD }
    $pwFile = "C:\tmp\password"
    if (Test-Path $pwFile) { return (Get-Content -Raw -Path $pwFile).TrimEnd("`r", "`n") }
    throw "No password: set `$env:SQL_PASSWORD or provide $pwFile"
}

function Get-VsRoot {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $root = & $vswhere -latest -products '*' -property installationPath 2>$null | Select-Object -First 1
        if ($root) { return $root }
    }
    throw "Visual Studio not found (vswhere). Install VS 2022 with the C++ workload."
}

# Resolve the native msodbcsql18.dll path: explicit arg, then env override, then
# the HKLM ODBCINST registration, then the system32 default.
function Resolve-NativeDll {
    if ($NativeDll) { return (Resolve-Path $NativeDll).Path }
    if ($env:ODBC_BENCH_NATIVE_DLL) { return $env:ODBC_BENCH_NATIVE_DLL }
    $reg = "HKLM:\Software\ODBC\ODBCINST.INI\ODBC Driver 18 for SQL Server"
    if (Test-Path $reg) {
        $d = Get-ItemProperty -Path $reg -Name "Driver" -ErrorAction SilentlyContinue
        if ($d -and $d.Driver -and (Test-Path $d.Driver)) { return $d.Driver }
    }
    $sys = Join-Path $env:WINDIR "system32\msodbcsql18.dll"
    if (Test-Path $sys) { return $sys }
    throw "Could not locate native msodbcsql18.dll; pass -NativeDll <path>."
}

# --- Build the Rust driver ----------------------------------------------------
Write-Host "=== Building mssql-odbc ($BuildType) ==="
Push-Location $OdbcCrateDir
if ($Release) { cargo build --release } else { cargo build }
$TargetDir = $null
try {
    $meta = cargo metadata --format-version 1 --no-deps 2>$null | ConvertFrom-Json
    if ($meta -and $meta.target_directory) { $TargetDir = $meta.target_directory }
} catch { }
Pop-Location
if (-not $TargetDir) { $TargetDir = Join-Path $WorkspaceDir "target" }

$RustDriverPath = if ($RustDll) { $RustDll } else { Join-Path $TargetDir "$BuildType\msodbcsql18.dll" }
if (-not (Test-Path $RustDriverPath)) { Write-Error "Rust driver not found at $RustDriverPath" }
$RustDriverPath = (Resolve-Path $RustDriverPath).Path
$NativeDriverPath = Resolve-NativeDll
Write-Host "Native driver: $NativeDriverPath"
Write-Host "Rust driver:   $RustDriverPath"

# --- Resolve connection env (only fill what isn't already set) ----------------
$envFile = Join-Path $WorkspaceDir "mssql-tds\.env"
$dotenv  = Read-DotEnv $envFile

if (-not $env:ODBC_TEST_SERVER) {
    $h = $dotenv["DB_HOST"]; if (-not $h) { $h = "127.0.0.1" }
    # The Windows ODBC driver on this host fails to resolve the literal
    # "localhost" ("No such host is known"); 127.0.0.1 connects reliably.
    if ($h -eq "localhost") { $h = "127.0.0.1" }
    $p = $dotenv["DB_PORT"]; if (-not $p) { $p = "1433" }
    $env:ODBC_TEST_SERVER = "$h,$p"
}
if (-not $env:ODBC_TEST_UID) {
    $u = $dotenv["DB_USERNAME"]; if (-not $u) { $u = "sa" }
    $env:ODBC_TEST_UID = $u
}
if (-not $env:ODBC_TEST_PWD) { $env:ODBC_TEST_PWD = Resolve-Password }
if (-not $env:ODBC_TEST_DATABASE)   { $env:ODBC_TEST_DATABASE = "tempdb" }
if (-not $env:ODBC_TEST_TRUST_CERT) { $env:ODBC_TEST_TRUST_CERT = "Yes" }

Write-Host "Connection: server=$($env:ODBC_TEST_SERVER) uid=$($env:ODBC_TEST_UID) db=$($env:ODBC_TEST_DATABASE) (password hidden)"

# --- Configure + build the benchmark target (MSVC/Ninja inside vcvars64) ------
$VsRoot    = Get-VsRoot
$VcVars    = Join-Path $VsRoot "VC\Auxiliary\Build\vcvars64.bat"
$VsCMake   = Join-Path $VsRoot "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
$CMakeExe  = if (Test-Path $VsCMake) { $VsCMake } elseif (Get-Command cmake -ErrorAction SilentlyContinue) { (Get-Command cmake).Source } else { throw "cmake not found." }
$Installer = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer'
$VerFlag   = if ($VcVarsVer) { "-vcvars_ver=$VcVarsVer" } else { "" }

Write-Host ""
Write-Host "=== Configuring + building fetch_bench (MSVC $VcVarsVer / Ninja) ==="
Push-Location $ScriptDir
$cfg = "set `"PATH=$Installer;%PATH%`" && call `"$VcVars`" $VerFlag >nul && " +
       "`"$CMakeExe`" -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=$CMakeBuildType -DODBC_E2E_FORCE_UNICODE=ON && " +
       "`"$CMakeExe`" --build build --target fetch_bench"
& $env:ComSpec /c $cfg
$buildExit = $LASTEXITCODE
Pop-Location
if ($buildExit -ne 0) { throw "fetch_bench build FAILED (exit $buildExit)" }

$BenchExe = $null
foreach ($cand in @(
    (Join-Path $ScriptDir "build\fetch_bench.exe"),
    (Join-Path $ScriptDir "build\$CMakeBuildType\fetch_bench.exe"))) {
    if (Test-Path $cand) { $BenchExe = $cand; break }
}
if (-not $BenchExe) { Write-Error "fetch_bench.exe not found under build/" }

# --- Run the A/B benchmark (both legs direct-load, no registry) ---------------
Write-Host ""
Write-Host "=== Running fetch-throughput A/B ==="
& $BenchExe --rows $Rows --reps $Reps --warmup $Warmup `
    --driver "native=dll:$NativeDriverPath" `
    --driver "rust=dll:$RustDriverPath"
$exit = $LASTEXITCODE
if ($exit -ne 0) { throw "benchmark FAILED (exit $exit)" }
