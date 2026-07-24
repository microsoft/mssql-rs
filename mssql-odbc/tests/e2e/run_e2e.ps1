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
#                      [-CompareWithMsodbcsql] [-MsodbcsqlDll PATH]
#
# -Retries N reruns each failing test up to N extra times (ctest
# --repeat until-pass:N+1). A test that passes on any attempt counts as a
# pass; the suite only fails if a test still fails after all retries.
#
# With -CompareWithMsodbcsql, the script reruns the same suite against the
# Microsoft C++ driver and prints a parity table. Unlike Linux (which uses
# per-run odbcinst.ini files via ODBCSYSINI), the Windows Driver Manager
# reads the registry, so the two runs swap the registered driver in HKLM
# between them and the original registration is restored at the end.
#
# The reference driver defaults to whatever msodbcsql18.dll was registered
# before this script ran (typically C:\WINDOWS\system32\msodbcsql18.dll).
# Override it with -MsodbcsqlDll PATH. The script exits 0 only if BOTH runs
# pass.
#
# Examples:
#   .\run_e2e.ps1
#   .\run_e2e.ps1 -CompareWithMsodbcsql
#   .\run_e2e.ps1 -CompareWithMsodbcsql -MsodbcsqlDll 'C:\path\to\msodbcsql18.dll'

param(
    [switch]$Release,
    [int]$Retries = 0,
    [switch]$CompareWithMsodbcsql,
    [string]$MsodbcsqlDll = ""
)

$ErrorActionPreference = "Stop"

$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Definition
$OdbcCrateDir = Resolve-Path (Join-Path $ScriptDir "..\..")
$WorkspaceDir = Resolve-Path (Join-Path $OdbcCrateDir "..")
$BuildType   = if ($Release) { "release" } else { "debug" }

$DriverRegKey  = "HKLM:\Software\ODBC\ODBCINST.INI\ODBC Driver 18 for SQL Server"
$DriversRegKey = "HKLM:\Software\ODBC\ODBCINST.INI\ODBC Drivers"
$DriverName    = "ODBC Driver 18 for SQL Server"

# Track whether we modified the registry so cleanup knows what to do. Each value
# is snapshotted independently (did it exist, and its prior value) so a partial
# or custom pre-existing registration is restored exactly, not left pointing at
# the test DLL or a stray "Installed" entry.
$script:OrigDriver = $null
$script:OrigSetup  = $null
$script:HadExistingKey = $false
$script:HadDriverValue = $false
$script:HadSetupValue = $false
$script:HadDriversListEntry = $false
$script:OrigDriversListValue = $null
$script:Registered = $false

function Save-OriginalRegistration {
    if (Test-Path $DriverRegKey) {
        $script:HadExistingKey = $true
        $d = Get-ItemProperty -Path $DriverRegKey -Name "Driver" -ErrorAction SilentlyContinue
        if ($null -ne $d) { $script:HadDriverValue = $true; $script:OrigDriver = $d.Driver }
        $s = Get-ItemProperty -Path $DriverRegKey -Name "Setup" -ErrorAction SilentlyContinue
        if ($null -ne $s) { $script:HadSetupValue = $true; $script:OrigSetup = $s.Setup }
    }
    if (Test-Path $DriversRegKey) {
        $e = Get-ItemProperty -Path $DriversRegKey -Name $DriverName -ErrorAction SilentlyContinue
        if ($null -ne $e) {
            $script:HadDriversListEntry = $true
            $script:OrigDriversListValue = $e.$DriverName
        }
    }
}

# Point the registered driver at $DriverPath without touching the saved
# original. Used to swap between the Rust and reference drivers across runs.
function Set-DriverRegistration([string]$DriverPath) {
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
        # Restore each value we may have overwritten, or remove it if it did not
        # exist before we ran (leaving a Driver/Setup value behind would point a
        # pre-existing key at the test DLL).
        if ($script:HadDriverValue) {
            Set-ItemProperty -Path $DriverRegKey -Name "Driver" -Value $script:OrigDriver
        } else {
            Remove-ItemProperty -Path $DriverRegKey -Name "Driver" -ErrorAction SilentlyContinue
        }
        if ($script:HadSetupValue) {
            Set-ItemProperty -Path $DriverRegKey -Name "Setup" -Value $script:OrigSetup
        } else {
            Remove-ItemProperty -Path $DriverRegKey -Name "Setup" -ErrorAction SilentlyContinue
        }
        Write-Host "[  DRIVER ] Restored original HKLM registration"
    } else {
        Remove-Item -Path $DriverRegKey -Force -ErrorAction SilentlyContinue
        Write-Host "[  DRIVER ] Removed HKLM registration (no prior driver)"
    }

    # Restore or remove the "ODBC Drivers" list entry independently of the driver
    # key above, since the two can pre-exist in either combination.
    if ($script:HadDriversListEntry) {
        Set-ItemProperty -Path $DriversRegKey -Name $DriverName -Value $script:OrigDriversListValue
    } elseif (Test-Path $DriversRegKey) {
        Remove-ItemProperty -Path $DriversRegKey -Name $DriverName -ErrorAction SilentlyContinue
    }

    $script:Registered = $false
}

# Run the (already-built) ctest suite, writing JUnit XML to $JunitName inside
# the build dir. Returns ctest's exit code without aborting the script.
function Invoke-CtestRun([string]$Label, [string]$JunitName) {
    Write-Host ""
    Write-Host "=== Running e2e tests against $Label ==="
    Push-Location (Join-Path $ScriptDir "build")
    $prevTarget = $env:ODBC_TEST_TARGET
    try {
        $ctestArgs = @('--output-on-failure', '-C', 'Debug', '--output-junit', $JunitName)
        if ($Retries -gt 0) {
            $ctestArgs += @('--repeat', "until-pass:$($Retries + 1)")
        }
        # ODBC_TEST_TARGET tells tests which driver implementation this leg runs
        # against ("mssql-odbc" or "msodbcsql") so mssql-odbc-specific tests can
        # SKIP_IF_COMPARING_MSODBCSQL() on the reference-driver leg.
        $env:ODBC_TEST_TARGET = $Label
        # Stream ctest output to the host so only the exit code is returned
        # from this function (an uncaptured pipeline would be returned too).
        ctest @ctestArgs | Out-Host
        return $LASTEXITCODE
    } finally {
        $env:ODBC_TEST_TARGET = $prevTarget
        Pop-Location
    }
}

# Parse a ctest JUnit XML into a hashtable of { test-name = 'PASS' | 'FAIL' }.
function Get-JunitResults([string]$Path) {
    $map = @{}
    if (-not (Test-Path $Path)) { return $map }
    try {
        [xml]$doc = Get-Content -Raw -Path $Path
    } catch {
        return $map
    }
    foreach ($tc in $doc.SelectNodes("//testcase")) {
        $name = $tc.GetAttribute("name")
        if (-not $name) { $name = "<unnamed>" }
        $status = "PASS"
        foreach ($child in $tc.ChildNodes) {
            if ($child.LocalName -eq "failure" -or $child.LocalName -eq "error") {
                $status = "FAIL"
                break
            }
            if ($child.LocalName -eq "skipped") {
                $status = "SKIP"
                # keep scanning: a failure child (if any) outranks a skip
            }
        }
        $map[$name] = $status
    }
    return $map
}

# Print a side-by-side parity table comparing the mssql-odbc and msodbcsql runs.
function Write-ParityReport([string]$RustXml, [string]$MsXml) {
    $rust = Get-JunitResults $RustXml
    $ms   = Get-JunitResults $MsXml
    $names = @($rust.Keys + $ms.Keys | Sort-Object -Unique)

    # Verdicts describe only the observed outcome pairing, not a root cause: a
    # per-test PASS/FAIL divergence does not by itself establish which side is
    # wrong, and a shared failure does not prove the test is buggy. Flag both for
    # investigation rather than asserting blame.
    $verdict = {
        param($r, $m)
        if ($r -eq "SKIP" -or $m -eq "SKIP") { return @("skipped (not compared)", "skip") }
        if ($r -eq "PASS" -and $m -eq "PASS") { return @("parity", "parity") }
        if ($r -eq "FAIL" -and $m -eq "FAIL") { return @("shared failure - investigate", "shared") }
        if ($r -ne $m) { return @("divergence - investigate", "divergence") }
        return @("missing run - investigate", "divergence")
    }

    $width = 4
    foreach ($n in $names) { if ($n.Length -gt $width) { $width = $n.Length } }

    Write-Host ""
    Write-Host "=== Parity report (mssql-odbc vs msodbcsql) ==="
    Write-Host ("{0}  {1,-10}  {2,-10}  Verdict" -f "Test".PadRight($width), "mssql-odbc", "msodbcsql")
    Write-Host ("{0}  {1}  {2}  {3}" -f ('-' * $width), ('-' * 10), ('-' * 10), ('-' * 30))

    $counts = @{ parity = 0; divergence = 0; shared = 0; skip = 0 }
    foreach ($n in $names) {
        $r = if ($rust.ContainsKey($n)) { $rust[$n] } else { "MISSING" }
        $m = if ($ms.ContainsKey($n)) { $ms[$n] } else { "MISSING" }
        $res = & $verdict $r $m
        $counts[$res[1]]++
        Write-Host ("{0}  {1,-10}  {2,-10}  {3}" -f $n.PadRight($width), $r, $m, $res[0])
    }
    Write-Host ""
    Write-Host ("Summary: {0} parity, {1} divergence(s), {2} shared failure(s), {3} skipped" -f $counts.parity, $counts.divergence, $counts.shared, $counts.skip)
}

try {
    if ($Retries -gt 0) {
        Write-Host "Retries enabled: each failing test reruns up to $Retries time(s)."
    }

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
    $DriverPath = (Resolve-Path $DriverPath).Path
    Write-Host "Rust driver: $DriverPath"

    # Capture the existing registration before we overwrite it. In comparison
    # mode this is also the default reference (msodbcsql) driver.
    Save-OriginalRegistration

    # Resolve the reference driver for comparison mode.
    $RefDriverPath = $null
    if ($CompareWithMsodbcsql) {
        if ($MsodbcsqlDll) {
            $RefDriverPath = $MsodbcsqlDll
        } elseif ($script:OrigDriver) {
            $RefDriverPath = $script:OrigDriver
        } else {
            Write-Error "No existing msodbcsql18.dll registration found. Pass -MsodbcsqlDll PATH to point at the reference driver."
        }
        if (-not (Test-Path $RefDriverPath)) {
            Write-Error "Reference driver not found: $RefDriverPath"
        }
        $RefDriverPath = (Resolve-Path $RefDriverPath).Path
        if ($RefDriverPath -eq $DriverPath) {
            Write-Error "Reference driver is the same as the Rust driver ($RefDriverPath). Pass a different -MsodbcsqlDll."
        }
        Write-Host "Reference driver (msodbcsql): $RefDriverPath"
    }

    Write-Host ""
    Write-Host "=== Configuring e2e tests (CMake) ==="
    Push-Location $ScriptDir
    cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug -DODBC_E2E_FORCE_UNICODE=ON

    Write-Host ""
    Write-Host "=== Building e2e tests ==="
    cmake --build build --config Debug
    Pop-Location

    $BuildDir  = Join-Path $ScriptDir "build"
    $RustJunit = Join-Path $BuildDir "junit-mssql-odbc.xml"
    $MsJunit   = Join-Path $BuildDir "junit-msodbcsql.xml"

    # Remove stale JUnit from previous runs so the parity report can never
    # read old results if ctest fails to execute (e.g. 0 tests run).
    Remove-Item -Path $RustJunit, $MsJunit -Force -ErrorAction SilentlyContinue

    # Run 1: the Rust driver.
    Set-DriverRegistration $DriverPath
    $RustExit = Invoke-CtestRun "mssql-odbc" "junit-mssql-odbc.xml"

    if (-not $CompareWithMsodbcsql) {
        if ($RustExit -ne 0) {
            throw "e2e tests FAILED (ctest exit $RustExit)"
        }
        Write-Host ""
        Write-Host "=== e2e tests passed ==="
        return
    }

    # Run 2: the reference msodbcsql driver, sharing the same built binaries.
    Set-DriverRegistration $RefDriverPath
    $MsExit = Invoke-CtestRun "msodbcsql" "junit-msodbcsql.xml"

    Write-ParityReport $RustJunit $MsJunit

    if ($RustExit -eq 0 -and $MsExit -eq 0) {
        Write-Host ""
        Write-Host "=== Both runs passed ==="
    } else {
        throw "Parity check FAILED (mssql-odbc exit $RustExit, msodbcsql exit $MsExit)"
    }
}
finally {
    Restore-Registration
}
