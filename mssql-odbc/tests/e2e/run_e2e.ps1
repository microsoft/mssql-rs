# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Build the Rust ODBC driver, register it in the Windows registry under its own
# driver name, and run C++ gtest e2e tests against it via the ODBC Driver Manager.
#
# The Rust driver registers as "ODBC Driver 18 for SQL Server (Rust)", so an
# installed msodbcsql18 ("ODBC Driver 18 for SQL Server") is left untouched and
# both can be used side by side. Each test leg selects its driver by name via
# ODBC_TEST_DRIVER.
#
# The test fixture itself does NOT handle driver registration — this script
# (or manual registry edits) is required. See run_e2e.sh for Linux/macOS.
#
# Requires: Administrator privileges (writes to HKLM).
# Usage: .\run_e2e.ps1 [-Release] [-Retries N] [-Coverage] [-CoverageOutput PATH]
#                      [-CompareWithMsodbcsql] [-MsodbcsqlDll PATH]
#
# -Retries N reruns each failing test up to N extra times (ctest
# --repeat until-pass:N+1). A test that passes on any attempt counts as a
# pass; the suite only fails if a test still fails after all retries.
#
# -Coverage builds the Rust driver with LLVM source-based coverage
# instrumentation so the driver code exercised by the C++ tests (which load the
# DLL through the Driver Manager as separate processes) is measured. A Cobertura
# report for mssql-tds + mssql-odbc is written to -CoverageOutput (default
# <repo>\target\cobertura-odbc-e2e.xml). Same mechanism as run_e2e.sh
# --coverage; everything runs through cargo-llvm-cov so the LLVM version that
# reads the .profraw matches the rustc that produced the instrumented DLL.
#
# With -CompareWithMsodbcsql, the script reruns the same suite against the
# Microsoft C++ driver and prints a parity table. Because the two drivers are
# registered under different names, no registry swap happens between the runs —
# only the driver name in the connection string changes.
#
# The reference driver defaults to the installed "ODBC Driver 18 for SQL Server"
# registration (typically C:\WINDOWS\system32\msodbcsql18.dll). Override it with
# -MsodbcsqlDll PATH, which temporarily repoints that registration and restores
# it at the end. The script exits 0 only if both runs pass AND every test reaches
# the same verdict in both legs.
#
# ODBC_TEST_CONNSTR overrides the whole connection string and would pin both
# legs to one driver, so comparison mode rejects it.
#
# When ODBC_TEST_SERVER is unset, a dev SQL Server on localhost:1433 is
# auto-detected (matching run_e2e.sh).
#
# Examples:
#   .\run_e2e.ps1
#   .\run_e2e.ps1 -CompareWithMsodbcsql
#   .\run_e2e.ps1 -CompareWithMsodbcsql -MsodbcsqlDll 'C:\path\to\msodbcsql18.dll'

param(
    [switch]$Release,
    [int]$Retries = 0,
    [switch]$Coverage,
    [string]$CoverageOutput = "",
    [switch]$CompareWithMsodbcsql,
    [string]$MsodbcsqlDll = "",
    # Optional ctest name-exclusion regex (ctest -E). Empty by default: the Windows
    # driver gaps tracked in AB#46973 (get_type_info_test, driver_connect_test) are
    # fixed, so the full suite runs on Windows. Pass -ExcludeTests '<regex>' to skip.
    [string]$ExcludeTests = ''
)

$ErrorActionPreference = "Stop"

$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Definition
$OdbcCrateDir = Resolve-Path (Join-Path $ScriptDir "..\..")
$WorkspaceDir = Resolve-Path (Join-Path $OdbcCrateDir "..")
$BuildType   = if ($Release) { "release" } else { "debug" }

# Default the Cobertura output into the workspace target/ dir so CI can publish
# it as an artifact. Resolved here (not as a param default) because it depends
# on $WorkspaceDir.
if ($Coverage -and -not $CoverageOutput) {
    $CoverageOutput = Join-Path $WorkspaceDir "target\cobertura-odbc-e2e.xml"
}

# CI only changes how much diagnostic context is printed on failure. Every
# failure mode below is fatal locally too — a broken build must never report
# "passed" just because a developer ran it by hand.
$script:IsCI = [bool]($env:TF_BUILD -or $env:CI -or $env:GITHUB_ACTIONS)

# Native commands ignore $ErrorActionPreference, so a failing cargo/cmake would
# otherwise fall through to ctest and produce an empty-but-green run.
function Invoke-Checked([string]$What, [scriptblock]$Command) {
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$What failed (exit $LASTEXITCODE)"
    }
}

# Dump the logs that explain a configure/build/test failure. CI has no working
# copy to inspect afterwards, so the evidence has to be in the build log.
function Write-FailureDiagnostics([string]$Context) {
    if (-not $script:IsCI) { return }
    Write-Host ""
    Write-Host "=== CI diagnostics: $Context ==="
    $buildDir = Join-Path $ScriptDir "build"
    $logs = @(
        (Join-Path $buildDir "CMakeFiles\CMakeOutput.log"),
        (Join-Path $buildDir "CMakeFiles\CMakeError.log"),
        (Join-Path $buildDir "Testing\Temporary\LastTest.log")
    )
    foreach ($log in $logs) {
        if (Test-Path $log) {
            Write-Host ""
            Write-Host "--- tail of $log ---"
            Get-Content -Path $log -Tail 100 | ForEach-Object { Write-Host $_ }
        }
    }
    if (Test-Path $buildDir) {
        Write-Host ""
        Write-Host "--- test executables in $buildDir ---"
        $exes = @(Get-ChildItem -Path $buildDir -Filter '*_test.exe' -Recurse -ErrorAction SilentlyContinue)
        if ($exes.Count -eq 0) {
            Write-Host "(none found)"
        } else {
            $exes | ForEach-Object { Write-Host $_.FullName }
        }
    } else {
        Write-Host "build directory does not exist: $buildDir"
    }
}

$OdbcInstRoot     = "HKLM:\Software\ODBC\ODBCINST.INI"
$DriversRegKey    = "$OdbcInstRoot\ODBC Drivers"
$MsodbcsqlName    = "ODBC Driver 18 for SQL Server"
$RustDriverName   = "ODBC Driver 18 for SQL Server (Rust)"
$MsodbcsqlRegKey  = "$OdbcInstRoot\$MsodbcsqlName"
$RustDriverRegKey = "$OdbcInstRoot\$RustDriverName"

# The Rust driver registers under its own name, so an installed msodbcsql18 is
# never touched. Only our own key is snapshotted, in case a developer has a Rust
# driver permanently installed under the same name.
$script:OrigDriver = $null
$script:OrigSetup  = $null
$script:HadExistingKey = $false
$script:HadDriverValue = $false
$script:HadSetupValue = $false
$script:HadDriversListEntry = $false
$script:OrigDriversListValue = $null
$script:Registered = $false
# Set only when -MsodbcsqlDll overrides the installed reference registration.
# Driver, Setup, and the "ODBC Drivers" list entry are snapshotted independently
# so each is restored exactly as it was (they can pre-exist in any combination).
$script:MsodbcsqlOverridden    = $false
$script:HadMsodbcsqlKey        = $false
$script:HadMsodbcsqlDriver     = $false
$script:HadMsodbcsqlSetup      = $false
$script:OrigMsodbcsqlDriver    = $null
$script:OrigMsodbcsqlSetup     = $null
$script:HadMsodbcsqlListEntry  = $false
$script:OrigMsodbcsqlListValue = $null

function Save-OriginalRegistration {
    if (Test-Path $RustDriverRegKey) {
        $script:HadExistingKey = $true
        $d = Get-ItemProperty -Path $RustDriverRegKey -Name "Driver" -ErrorAction SilentlyContinue
        if ($null -ne $d) { $script:HadDriverValue = $true; $script:OrigDriver = $d.Driver }
        $s = Get-ItemProperty -Path $RustDriverRegKey -Name "Setup" -ErrorAction SilentlyContinue
        if ($null -ne $s) { $script:HadSetupValue = $true; $script:OrigSetup = $s.Setup }
    }
    if (Test-Path $DriversRegKey) {
        $e = Get-ItemProperty -Path $DriversRegKey -Name $RustDriverName -ErrorAction SilentlyContinue
        if ($null -ne $e) {
            $script:HadDriversListEntry = $true
            $script:OrigDriversListValue = $e.$RustDriverName
        }
    }
}

# Register the Rust driver under its own name, alongside any installed
# msodbcsql18. Both legs then run without swapping registrations.
function Register-RustDriver([string]$DriverPath) {
    # Arm cleanup before the first write: if any Set-ItemProperty/New-Item throws
    # partway through, Restore-Registration still runs and unwinds the partial key.
    $script:Registered = $true
    if (-not (Test-Path $RustDriverRegKey)) {
        New-Item -Path $RustDriverRegKey -Force | Out-Null
    }
    Set-ItemProperty -Path $RustDriverRegKey -Name "Driver" -Value $DriverPath
    Set-ItemProperty -Path $RustDriverRegKey -Name "Setup"  -Value $DriverPath

    if (-not (Test-Path $DriversRegKey)) {
        New-Item -Path $DriversRegKey -Force | Out-Null
    }
    Set-ItemProperty -Path $DriversRegKey -Name $RustDriverName -Value "Installed"

    Write-Host "[  DRIVER ] Registered '$RustDriverName' in HKLM: $DriverPath"
}

# Resolve the installed reference driver from its own registry key.
function Get-MsodbcsqlDriverPath {
    $d = Get-ItemProperty -Path $MsodbcsqlRegKey -Name "Driver" -ErrorAction SilentlyContinue
    if ($null -ne $d) { return $d.Driver }
    return $null
}

# Point the reference registration at an explicit DLL for the comparison leg,
# snapshotting the installed Driver, Setup, and "ODBC Drivers" list values
# independently so Restore-Registration can put each back exactly as it was.
function Set-MsodbcsqlOverride([string]$DriverPath) {
    # Arm cleanup before the first write, so a throw mid-registration still unwinds.
    $script:MsodbcsqlOverridden = $true
    if (Test-Path $MsodbcsqlRegKey) {
        $script:HadMsodbcsqlKey = $true
        $d = Get-ItemProperty -Path $MsodbcsqlRegKey -Name "Driver" -ErrorAction SilentlyContinue
        if ($null -ne $d) { $script:HadMsodbcsqlDriver = $true; $script:OrigMsodbcsqlDriver = $d.Driver }
        $s = Get-ItemProperty -Path $MsodbcsqlRegKey -Name "Setup" -ErrorAction SilentlyContinue
        if ($null -ne $s) { $script:HadMsodbcsqlSetup = $true; $script:OrigMsodbcsqlSetup = $s.Setup }
    } else {
        New-Item -Path $MsodbcsqlRegKey -Force | Out-Null
    }
    Set-ItemProperty -Path $MsodbcsqlRegKey -Name "Driver" -Value $DriverPath
    Set-ItemProperty -Path $MsodbcsqlRegKey -Name "Setup"  -Value $DriverPath

    if (Test-Path $DriversRegKey) {
        $e = Get-ItemProperty -Path $DriversRegKey -Name $MsodbcsqlName -ErrorAction SilentlyContinue
        if ($null -ne $e) {
            $script:HadMsodbcsqlListEntry = $true
            $script:OrigMsodbcsqlListValue = $e.$MsodbcsqlName
        }
    } else {
        New-Item -Path $DriversRegKey -Force | Out-Null
    }
    Set-ItemProperty -Path $DriversRegKey -Name $MsodbcsqlName -Value "Installed"
    Write-Host "[  DRIVER ] Overrode '$MsodbcsqlName' in HKLM: $DriverPath"
}

function Restore-Registration {
    if ($script:MsodbcsqlOverridden) {
        if ($script:HadMsodbcsqlKey) {
            # Restore each value we may have overwritten, or remove it if it did
            # not exist before, mirroring the Rust driver restore below.
            if ($script:HadMsodbcsqlDriver) {
                Set-ItemProperty -Path $MsodbcsqlRegKey -Name "Driver" -Value $script:OrigMsodbcsqlDriver
            } else {
                Remove-ItemProperty -Path $MsodbcsqlRegKey -Name "Driver" -ErrorAction SilentlyContinue
            }
            if ($script:HadMsodbcsqlSetup) {
                Set-ItemProperty -Path $MsodbcsqlRegKey -Name "Setup" -Value $script:OrigMsodbcsqlSetup
            } else {
                Remove-ItemProperty -Path $MsodbcsqlRegKey -Name "Setup" -ErrorAction SilentlyContinue
            }
            Write-Host "[  DRIVER ] Restored original HKLM registration for '$MsodbcsqlName'"
        } else {
            Remove-Item -Path $MsodbcsqlRegKey -Force -ErrorAction SilentlyContinue
            Write-Host "[  DRIVER ] Removed HKLM registration for '$MsodbcsqlName' (no prior key)"
        }

        # Restore or remove the "ODBC Drivers" list entry independently of the key.
        if ($script:HadMsodbcsqlListEntry) {
            Set-ItemProperty -Path $DriversRegKey -Name $MsodbcsqlName -Value $script:OrigMsodbcsqlListValue
        } elseif (Test-Path $DriversRegKey) {
            Remove-ItemProperty -Path $DriversRegKey -Name $MsodbcsqlName -ErrorAction SilentlyContinue
        }

        $script:MsodbcsqlOverridden = $false
    }

    if (-not $script:Registered) { return }

    if ($script:HadExistingKey) {
        # Restore each value we may have overwritten, or remove it if it did not
        # exist before we ran (leaving a Driver/Setup value behind would point a
        # pre-existing key at the test DLL).
        if ($script:HadDriverValue) {
            Set-ItemProperty -Path $RustDriverRegKey -Name "Driver" -Value $script:OrigDriver
        } else {
            Remove-ItemProperty -Path $RustDriverRegKey -Name "Driver" -ErrorAction SilentlyContinue
        }
        if ($script:HadSetupValue) {
            Set-ItemProperty -Path $RustDriverRegKey -Name "Setup" -Value $script:OrigSetup
        } else {
            Remove-ItemProperty -Path $RustDriverRegKey -Name "Setup" -ErrorAction SilentlyContinue
        }
        Write-Host "[  DRIVER ] Restored original HKLM registration for '$RustDriverName'"
    } else {
        Remove-Item -Path $RustDriverRegKey -Force -ErrorAction SilentlyContinue
        Write-Host "[  DRIVER ] Removed HKLM registration for '$RustDriverName'"
    }

    # Restore or remove the "ODBC Drivers" list entry independently of the driver
    # key above, since the two can pre-exist in either combination.
    if ($script:HadDriversListEntry) {
        Set-ItemProperty -Path $DriversRegKey -Name $RustDriverName -Value $script:OrigDriversListValue
    } elseif (Test-Path $DriversRegKey) {
        Remove-ItemProperty -Path $DriversRegKey -Name $RustDriverName -ErrorAction SilentlyContinue
    }

    $script:Registered = $false
}

# Run the (already-built) ctest suite, writing JUnit XML to $JunitName inside
# the build dir. Returns ctest's exit code without aborting the script.
function Invoke-CtestRun([string]$Label, [string]$JunitName, [string]$DriverName) {
    Write-Host ""
    Write-Host "=== Running e2e tests against $Label ==="
    Write-Host "ODBC_TEST_DRIVER=$DriverName"
    Push-Location (Join-Path $ScriptDir "build")
    $prevTarget = $env:ODBC_TEST_TARGET
    $prevDriver = $env:ODBC_TEST_DRIVER
    try {
        $ctestArgs = @('--output-on-failure', '-C', 'Debug', '--output-junit', $JunitName)
        if ($Retries -gt 0) {
            $ctestArgs += @('--repeat', "until-pass:$($Retries + 1)")
        }
        if ($ExcludeTests) {
            # ctest -E <regex> excludes tests by name (opt-in via -ExcludeTests).
            $ctestArgs += @('-E', $ExcludeTests)
        }
        # ODBC_TEST_TARGET tells tests which driver implementation this leg runs
        # against ("mssql-odbc" or "msodbcsql") so mssql-odbc-specific tests can
        # SKIP_IF_COMPARING_MSODBCSQL() on the reference-driver leg.
        # ODBC_TEST_DRIVER selects the driver by name in the connection string.
        $env:ODBC_TEST_TARGET = $Label
        $env:ODBC_TEST_DRIVER = $DriverName
        # Stream ctest output to the host so only the exit code is returned
        # from this function (an uncaptured pipeline would be returned too).
        ctest @ctestArgs | Out-Host
        return $LASTEXITCODE
    } finally {
        $env:ODBC_TEST_TARGET = $prevTarget
        $env:ODBC_TEST_DRIVER = $prevDriver
        Pop-Location
    }
}

# A ctest run that executed nothing still exits 0 and prints "No tests were
# found!!!" — historically that turned a broken CMake configure into a green
# build. Treat an empty JUnit as a hard failure.
function Assert-TestsExecuted([string]$JunitPath, [string]$Label) {
    $count = 0
    if (Test-Path $JunitPath) {
        try {
            [xml]$doc = Get-Content -Raw -Path $JunitPath
            $count = @($doc.SelectNodes("//testcase")).Count
        } catch {
            $count = 0
        }
    }
    if ($count -eq 0) {
        Write-FailureDiagnostics "no tests executed for '$Label'"
        throw "No tests were executed for '$Label'. The CMake project produced no ctest entries, or ctest failed to run them."
    }
    Write-Host "$Label leg executed $count test(s)."
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
# Returns the verdict counts so the caller can fail on any non-parity outcome.
function Write-ParityReport([string]$RustXml, [string]$MsXml) {
    $rust = Get-JunitResults $RustXml
    $ms   = Get-JunitResults $MsXml
    $names = @($rust.Keys + $ms.Keys | Sort-Object -Unique)

    # Verdicts describe only the observed outcome pairing, not a root cause: a
    # per-test PASS/FAIL divergence does not by itself establish which side is
    # wrong, and a shared failure does not prove the test is buggy. Flag both for
    # investigation rather than asserting blame.
    #
    # MIRROR: this classification is duplicated in verdict() in parity_report.py
    # (the Linux/macOS runner shells out to Python; Windows stays dependency-free
    # here). Keep the two in lockstep — any change to the ordering or the labels
    # here must be made there too.
    $verdict = {
        param($r, $m)
        # Classify MISSING first: a test present in only one leg is a divergence,
        # even when its lone result is SKIP (otherwise the skip shortcut below
        # would mask a one-sided run as an allowed skip).
        if ($r -eq "MISSING" -or $m -eq "MISSING") { return @("missing run - investigate", "divergence") }
        if ($r -eq "SKIP" -or $m -eq "SKIP") { return @("skipped (not compared)", "skip") }
        if ($r -eq "PASS" -and $m -eq "PASS") { return @("parity", "parity") }
        if ($r -eq "FAIL" -and $m -eq "FAIL") { return @("shared failure - investigate", "shared") }
        if ($r -ne $m) { return @("divergence - investigate", "divergence") }
        return @("unexpected - investigate", "divergence")
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
    return $counts
}

# Enable LLVM source-based instrumentation for the driver build.
# `cargo llvm-cov show-env` prints the env (RUSTFLAGS with -C instrument-coverage,
# the llvm-cov target dir, and an LLVM_PROFILE_FILE pattern keyed by %p/%m so
# distinct gtest processes and ctest retries never clobber each other's .profraw).
# PowerShell has no `eval`, so parse each KEY=VALUE line (stripping surrounding
# quotes) into the process env. The subsequent `cargo build` then produces an
# instrumented mssql-odbc.dll, and every ctest child process inherits
# LLVM_PROFILE_FILE from this environment. The llvm-cov target dir is also
# exported so the later `cargo metadata` resolves the INSTRUMENTED DLL.
function Enable-CoverageInstrumentation {
    Write-Host "=== Enabling coverage instrumentation for the Rust driver ==="
    cargo llvm-cov clean --workspace
    # stderr carries rustup/info chatter (e.g. "info: cargo-llvm-cov ...") — drop it.
    $envLines = & cargo llvm-cov show-env 2>$null
    foreach ($line in $envLines) {
        if ($line -match '^([A-Za-z_][A-Za-z0-9_]*)=(.*)$') {
            $key = $Matches[1]
            $val = $Matches[2].Trim()
            if ($val.Length -ge 2 -and $val.StartsWith('"') -and $val.EndsWith('"')) {
                $val = $val.Substring(1, $val.Length - 2)
            }
            Set-Item -Path "env:$key" -Value $val
        }
    }
    Write-Host "Coverage: LLVM_PROFILE_FILE=$($env:LLVM_PROFILE_FILE)"
}

# Turn the .profraw written by the ctest processes into a Cobertura report.
# `cargo llvm-cov report` wraps llvm-profdata merge + llvm-cov export, so the
# LLVM tooling matches the rustc that produced the instrumented DLL. Includes
# both mssql-tds and mssql-odbc because the cdylib statically links mssql-tds.
# Best-effort: the suite has already run, so a coverage tooling hiccup must not
# change the pass/fail result. Runs from the repo root so Cobertura filenames
# are repo-root-relative and union with the per-OS reports in the merge.
function New-CoverageReport([string]$OutputPath) {
    Write-Host ""
    Write-Host "=== Generating ODBC e2e coverage report ==="
    # Prove the instrumented driver actually ran under the Driver Manager: each
    # gtest process writes a .profraw keyed by LLVM_PROFILE_FILE. Zero .profraw
    # means coverage captured nothing (e.g. an uninstrumented DLL was loaded), so
    # warn loudly. Non-fatal: the functional result already stands.
    if ($env:LLVM_PROFILE_FILE) {
        $profrawDir = Split-Path -Parent $env:LLVM_PROFILE_FILE
        if ($profrawDir -and (Test-Path $profrawDir)) {
            $n = @(Get-ChildItem -Path $profrawDir -Filter '*.profraw' -ErrorAction SilentlyContinue).Count
            Write-Host "Coverage: found $n .profraw file(s) under $profrawDir"
            if ($n -eq 0) {
                Write-Warning "no .profraw produced; the driver ctest loaded may not be instrumented"
            }
        }
    }
    try {
        $outDir = Split-Path -Parent $OutputPath
        if ($outDir -and -not (Test-Path $outDir)) {
            New-Item -ItemType Directory -Path $outDir -Force | Out-Null
        }
    } catch {
        Write-Warning "failed to create ODBC e2e coverage output directory: $_"
        return
    }
    Push-Location $WorkspaceDir
    try {
        cargo llvm-cov report --package mssql-tds --package mssql-odbc `
            --cobertura --output-path $OutputPath
        if ($LASTEXITCODE -eq 0) {
            Write-Host "Coverage report written to $OutputPath"
        } else {
            Write-Warning "failed to generate ODBC e2e coverage report (exit $LASTEXITCODE)"
        }
    } catch {
        Write-Warning "failed to generate ODBC e2e coverage report: $_"
    } finally {
        Pop-Location
    }
}

# Mirror of setup_dev_sql_env in run_e2e.sh: point the tests at a dev SQL Server
# on localhost:1433 when the caller hasn't configured a server, so `.\run_e2e.ps1`
# works out of the box against dev\dev-launchsql.
function Initialize-DevSqlEnv {
    if ($env:ODBC_TEST_SERVER) { return }

    $reachable = $false
    $client = $null
    try {
        $client = New-Object System.Net.Sockets.TcpClient
        $reachable = $client.ConnectAsync('localhost', 1433).Wait(2000)
    } catch {
        $reachable = $false
    } finally {
        if ($client) { $client.Dispose() }
    }
    if (-not $reachable) { return }

    $env:ODBC_TEST_SERVER = 'localhost'
    if (-not $env:ODBC_TEST_UID) { $env:ODBC_TEST_UID = 'sa' }
    if (-not $env:ODBC_TEST_TRUST_CERT) { $env:ODBC_TEST_TRUST_CERT = 'Yes' }

    if (-not $env:ODBC_TEST_PWD) {
        if ($env:SQL_PASSWORD) {
            $env:ODBC_TEST_PWD = $env:SQL_PASSWORD
        } else {
            $dotenv = Join-Path $WorkspaceDir "mssql-tds\.env"
            if (Test-Path $dotenv) {
                $line = Get-Content -Path $dotenv | Where-Object { $_ -match '^SQL_PASSWORD=' } | Select-Object -First 1
                if ($line) {
                    $value = $line.Substring($line.IndexOf('=') + 1)
                    if ($value) { $env:ODBC_TEST_PWD = $value }
                }
            }
        }
    }

    if ($env:ODBC_TEST_PWD) {
        Write-Host "Auto-detected dev SQL Server at localhost:1433 ($($env:ODBC_TEST_UID) login)"
    } else {
        # Without a password the fixture falls back to Trusted_Connection, which
        # is a valid local setup — warn rather than fail.
        Write-Host "Warning: SQL Server detected on localhost:1433 but no password found; using integrated auth."
        Write-Host "  Set SQL_PASSWORD or ODBC_TEST_PWD, or run dev\dev-launchsql.ps1 first, to use SQL auth."
    }
}

# Ensure cmake is resolvable. CI Windows agents have Visual Studio (used to link
# the Rust MSVC build) which bundles CMake, but it isn't on PATH by default.
# Locate it via vswhere and prepend it, falling back to a standalone install.
function Initialize-CMake {
    if (Get-Command cmake -ErrorAction SilentlyContinue) {
        Write-Host "Using cmake: $((Get-Command cmake).Source)"
        return
    }

    $candidates = @('C:\Program Files\CMake\bin', (Join-Path ${env:ProgramFiles(x86)} 'CMake\bin'))

    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $vsRoot = & $vswhere -latest -products '*' -property installationPath 2>$null | Select-Object -First 1
        if ($vsRoot) {
            $candidates = @(Join-Path $vsRoot 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin') + $candidates
        }
    }

    foreach ($dir in $candidates) {
        if ($dir -and (Test-Path (Join-Path $dir 'cmake.exe'))) {
            $env:PATH = "$dir;$env:PATH"
            Write-Host "Added CMake to PATH from: $dir"
            break
        }
    }

    if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
        Write-Error "cmake not found. Install CMake 3.15+ or the 'C++ CMake tools for Windows' Visual Studio component."
    }
}

try {
    if ($Retries -gt 0) {
        Write-Host "Retries enabled: each failing test reruns up to $Retries time(s)."
    }

    if ($Coverage) {
        Enable-CoverageInstrumentation
    }

    Initialize-DevSqlEnv

    Write-Host "=== Building mssql-odbc ($BuildType) ==="
    Push-Location $OdbcCrateDir
    try {
        if ($Release) {
            Invoke-Checked "cargo build --release" { cargo build --release }
        } else {
            Invoke-Checked "cargo build" { cargo build }
        }
    } finally {
        Pop-Location
    }

    $DriverPath = & (Join-Path $OdbcCrateDir "scripts\finalize-artifact.ps1") -Profile $BuildType
    Write-Host "Rust driver: $DriverPath"

    if ($Coverage) {
        # RUSTFLAGS=-C instrument-coverage was exported before the build, so this
        # DLL is the instrumented one ctest will load; the post-run .profraw check
        # in New-CoverageReport proves it fired.
        Write-Host "Coverage: instrumented driver=$DriverPath"
        Write-Host "Coverage: LLVM_PROFILE_FILE=$($env:LLVM_PROFILE_FILE)"
    }

    # Snapshot our own registration (if a Rust driver is already installed under
    # the same name) so it can be restored on exit.
    Save-OriginalRegistration

    # Resolve the reference driver for comparison mode from its own registration.
    $RefDriverPath = $null
    if ($CompareWithMsodbcsql) {
        if ($env:ODBC_TEST_CONNSTR) {
            throw "ODBC_TEST_CONNSTR is set; it overrides the driver name and would pin both comparison legs to the same driver. Unset it to compare."
        }
        if ($env:ODBC_TEST_DSN) {
            throw "ODBC_TEST_DSN is set; a DSN pins the connection to one driver, so both comparison legs would use the same driver. Unset it to compare."
        }
        if ($MsodbcsqlDll) {
            $RefDriverPath = $MsodbcsqlDll
        } else {
            $RefDriverPath = Get-MsodbcsqlDriverPath
        }
        if (-not $RefDriverPath) {
            Write-Error "No '$MsodbcsqlName' registration found. Pass -MsodbcsqlDll PATH to point at the reference driver."
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
    Initialize-CMake
    Push-Location $ScriptDir
    try {
        try {
            Invoke-Checked "CMake configure" {
                cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug -DODBC_E2E_FORCE_UNICODE=ON
            }

            Write-Host ""
            Write-Host "=== Building e2e tests ==="
            Invoke-Checked "CMake build" { cmake --build build --config Debug }
        } catch {
            Write-FailureDiagnostics "CMake configure/build failed"
            throw
        }
    } finally {
        Pop-Location
    }

    $BuildDir  = Join-Path $ScriptDir "build"
    $RustJunit = Join-Path $BuildDir "junit-mssql-odbc.xml"
    $MsJunit   = Join-Path $BuildDir "junit-msodbcsql.xml"

    # Remove stale JUnit from previous runs so the parity report can never
    # read old results if ctest fails to execute (e.g. 0 tests run).
    Remove-Item -Path $RustJunit, $MsJunit -Force -ErrorAction SilentlyContinue

    # Run 1: the Rust driver, registered under its own name.
    Register-RustDriver $DriverPath
    $RustExit = Invoke-CtestRun "mssql-odbc" "junit-mssql-odbc.xml" $RustDriverName
    Assert-TestsExecuted $RustJunit "mssql-odbc"

    # Report on the instrumented mssql-odbc leg before the (uninstrumented)
    # msodbcsql reference leg runs, so the profraw reflects our driver only.
    if ($Coverage) {
        New-CoverageReport $CoverageOutput
    }

    if (-not $CompareWithMsodbcsql) {
        if ($RustExit -ne 0) {
            Write-FailureDiagnostics "e2e tests failed"
            throw "e2e tests FAILED (ctest exit $RustExit)"
        }
        Write-Host ""
        Write-Host "=== e2e tests passed ==="
        return
    }

    # Run 2: the reference msodbcsql driver, sharing the same built binaries.
    # No registry swap is needed — the two drivers are registered side by side,
    # so only the driver name in the connection string changes. When
    # -MsodbcsqlDll points somewhere other than the installed registration, that
    # DLL is registered under the reference name for the duration of this leg.
    if ($MsodbcsqlDll) {
        Set-MsodbcsqlOverride $RefDriverPath
    }
    $MsExit = Invoke-CtestRun "msodbcsql" "junit-msodbcsql.xml" $MsodbcsqlName
    Assert-TestsExecuted $MsJunit "msodbcsql"

    $parity = Write-ParityReport $RustJunit $MsJunit

    # Any non-parity outcome fails the run. ctest exit codes alone are not
    # enough: a test present in only one leg leaves both legs green while the
    # comparison is meaningless.
    $nonParity = $parity.divergence + $parity.shared
    if ($RustExit -ne 0 -or $MsExit -ne 0 -or $nonParity -gt 0) {
        Write-FailureDiagnostics "parity check failed"
        throw ("Parity check FAILED (mssql-odbc exit $RustExit, msodbcsql exit $MsExit; " +
               "$($parity.divergence) divergence(s), $($parity.shared) shared failure(s))")
    }

    Write-Host ""
    Write-Host "=== Both runs passed with full parity ==="
}
finally {
    Restore-Registration
}
