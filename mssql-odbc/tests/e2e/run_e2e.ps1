# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the Rust ODBC driver, register it in the Windows registry,
# and run C++ gtest e2e tests against it via the ODBC Driver Manager.
#
# The test fixture itself does NOT handle driver registration — this script
# (or manual registry edits) is required. See run_e2e.sh for Linux/macOS.
#
# Requires: Administrator privileges (writes to HKLM).
# Usage: .\run_e2e.ps1 [-Release] [-Retries N]
#
# -Retries N reruns each failing test up to N extra times (ctest
# --repeat until-pass:N+1). A test that passes on any attempt counts as a
# pass; the suite only fails if a test still fails after all retries.

param(
    [switch]$Release,
    [int]$Retries = 0
)

$ErrorActionPreference = "Stop"

$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Definition
$OdbcCrateDir = Resolve-Path (Join-Path $ScriptDir "..\..")
$WorkspaceDir = Resolve-Path (Join-Path $OdbcCrateDir "..")
$BuildType   = if ($Release) { "release" } else { "debug" }

$DriverRegKey  = "HKLM:\Software\ODBC\ODBCINST.INI\ODBC Driver 18 for SQL Server"
$DriversRegKey = "HKLM:\Software\ODBC\ODBCINST.INI\ODBC Drivers"
$DriverName    = "ODBC Driver 18 for SQL Server"

# Track whether we modified the registry so cleanup knows what to do.
$script:OrigDriver = $null
$script:OrigSetup  = $null
$script:HadExistingKey = $false
$script:Registered = $false

function Save-OriginalRegistration {
    if (Test-Path $DriverRegKey) {
        $script:HadExistingKey = $true
        $script:OrigDriver = (Get-ItemProperty -Path $DriverRegKey -Name "Driver" -ErrorAction SilentlyContinue).Driver
        $script:OrigSetup  = (Get-ItemProperty -Path $DriverRegKey -Name "Setup"  -ErrorAction SilentlyContinue).Setup
    }
}

function Register-Driver([string]$DriverPath) {
    Save-OriginalRegistration

    if (-not (Test-Path $DriverRegKey)) {
        New-Item -Path $DriverRegKey -Force | Out-Null
    }
    Set-ItemProperty -Path $DriverRegKey -Name "Driver" -Value $DriverPath
    Set-ItemProperty -Path $DriverRegKey -Name "Setup"  -Value $DriverPath

    if (-not (Test-Path $DriversRegKey)) {
        New-Item -Path $DriversRegKey -Force | Out-Null
    }
    Set-ItemProperty -Path $DriversRegKey -Name $DriverName -Value "Installed"

    $script:Registered = $true
    Write-Host "[  DRIVER ] Registered in HKLM: $DriverPath"
}

function Restore-Registration {
    if (-not $script:Registered) { return }

    if ($script:HadExistingKey) {
        if ($script:OrigDriver) {
            Set-ItemProperty -Path $DriverRegKey -Name "Driver" -Value $script:OrigDriver
        }
        if ($script:OrigSetup) {
            Set-ItemProperty -Path $DriverRegKey -Name "Setup" -Value $script:OrigSetup
        }
        Write-Host "[  DRIVER ] Restored original HKLM registration"
    } else {
        Remove-Item -Path $DriverRegKey -Force -ErrorAction SilentlyContinue
        if (Test-Path $DriversRegKey) {
            Remove-ItemProperty -Path $DriversRegKey -Name $DriverName -ErrorAction SilentlyContinue
        }
        Write-Host "[  DRIVER ] Removed HKLM registration (no prior driver)"
    }
    $script:Registered = $false
}

try {
    Write-Host "=== Building mssql-odbc ($BuildType) ==="
    Push-Location $OdbcCrateDir
    if ($Release) {
        cargo build --release
    } else {
        cargo build
    }
    Pop-Location

    # Cargo builds into the workspace root's target/ by default, but honors
    # CARGO_TARGET_DIR (set by CI). Resolve via `cargo metadata` so the driver is
    # found regardless of where it landed.
    $TargetDir = $null
    Push-Location $OdbcCrateDir
    try {
        $meta = cargo metadata --format-version 1 --no-deps 2>$null | ConvertFrom-Json
        if ($meta -and $meta.target_directory) { $TargetDir = $meta.target_directory }
    } catch { }
    Pop-Location
    if (-not $TargetDir) { $TargetDir = Join-Path $WorkspaceDir "target" }

    $DriverPath = Join-Path $TargetDir "$BuildType\msodbcsql18.dll"
    if (-not (Test-Path $DriverPath)) {
        Write-Error "Driver not found at $DriverPath"
    }
    $DriverPath = Resolve-Path $DriverPath
    Write-Host "Driver: $DriverPath"

    Register-Driver $DriverPath

    Write-Host ""
    Write-Host "=== Configuring e2e tests (CMake) ==="
    Push-Location $ScriptDir
    cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug

    Write-Host ""
    Write-Host "=== Building e2e tests ==="
    cmake --build build --config Debug

    Write-Host ""
    Write-Host "=== Running e2e tests ==="
    Push-Location build
    $ctestArgs = @('--output-on-failure', '-C', 'Debug', '--output-junit', 'junit-mssql-odbc.xml')
    if ($Retries -gt 0) {
        # until-pass:N runs a failing test up to N times total, so N retries = N+1.
        $ctestArgs += @('--repeat', "until-pass:$($Retries + 1)")
        Write-Host "Retries enabled: each failing test reruns up to $Retries time(s)."
    }
    ctest @ctestArgs
    $ctestExit = $LASTEXITCODE
    Pop-Location
    Pop-Location

    if ($ctestExit -ne 0) {
        throw "e2e tests FAILED (ctest exit $ctestExit)"
    }

    Write-Host ""
    Write-Host "=== e2e tests passed ==="
}
finally {
    Restore-Registration
}
