# Regression tests for the perf-lab harness's baseline source-swap helpers:
# version stamping, idempotent restore, and cleanup ordering.
#
# Run: pwsh mssql-tds-bench/perf-lab/tests/test-source-swap.ps1

$ErrorActionPreference = 'Stop'
$here    = Split-Path -Parent $MyInvocation.MyCommand.Path
$script:HarnessPath = Join-Path $here '..' 'run-benchmarks.ps1'
$failed  = 0

function Test-Result {
    param([string] $Name, $Actual, $Expected)
    if ("$Actual" -eq "$Expected") { Write-Host "  ok   - $Name" }
    else { Write-Host "  FAIL - $Name (expected [$Expected], got [$Actual])" -ForegroundColor Red; $script:failed = 1 }
}

# Load the helpers straight out of the harness so these tests exercise the real
# implementations and cannot drift from them.
$harness = Get-Content -Raw -LiteralPath $script:HarnessPath
foreach ($fn in 'Get-PackageVersion', 'Set-PackageVersion', 'Sync-BaselineVersion', 'Set-BaselineSource', 'Restore-CandidateSource') {
    $m = [regex]::Match($harness, "(?ms)^function $fn \{.*?^\}")
    if (-not $m.Success) { throw "could not extract $fn from $script:HarnessPath" }
    Invoke-Expression $m.Value
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("perfswap-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null

function Write-Manifest {
    param([string] $CrateDir, [string] $Version)
    New-Item -ItemType Directory -Force -Path $CrateDir | Out-Null
    @(
        '[package]'
        'name = "mssql-tds"'
        "version = `"$Version`""
        'edition = "2024"'
        ''
        '[dependencies]'
        'tokio = { version = "1.0" }'
    ) | Set-Content -LiteralPath (Join-Path $CrateDir 'Cargo.toml')
}

function New-Repo {
    param([string] $Root, [string] $CandidateVersion, [string] $BaselineVersion)
    if (Test-Path -LiteralPath $Root) { Remove-Item -Recurse -Force $Root }
    Write-Manifest (Join-Path $Root 'mssql-tds') $CandidateVersion
    'candidate' | Set-Content -LiteralPath (Join-Path $Root 'mssql-tds' 'MARKER')
    Write-Manifest (Join-Path $Root 'tree' 'mssql-tds') $BaselineVersion
    'baseline' | Set-Content -LiteralPath (Join-Path $Root 'tree' 'mssql-tds' 'MARKER')
    $script:CandidateSrc = Join-Path $Root 'mssql-tds'
    $script:StashedSrc   = Join-Path $Root '.mssql-tds-candidate'
    $script:BaselineTree = Join-Path $Root 'tree'
}

Write-Host 'manifest helpers'
$m = Join-Path $work 'm'
Write-Manifest $m '0.1.0'
$mf = Join-Path $m 'Cargo.toml'
Test-Result 'Get-PackageVersion reads the [package] version' (Get-PackageVersion $mf) '0.1.0'
Set-PackageVersion $mf '9.9.9'
Test-Result 'Set-PackageVersion rewrites the package version' (Get-PackageVersion $mf) '9.9.9'
Test-Result 'Set-PackageVersion leaves dependency versions alone' ((Get-Content $mf | Select-String -SimpleMatch 'version = "1.0"').Count) 1
Test-Result 'Set-PackageVersion preserves the line count' ((Get-Content $mf).Count) 7

Write-Host 'swap and restore'
New-Repo (Join-Path $work 'r1') '0.2.0' '0.1.0'
Set-BaselineSource | Out-Null
Test-Result 'swap installs the baseline source' (Get-Content (Join-Path $script:CandidateSrc 'MARKER')) 'baseline'
Test-Result 'swap stamps the candidate version onto the baseline' (Get-PackageVersion (Join-Path $script:CandidateSrc 'Cargo.toml')) '0.2.0'
Restore-CandidateSource
Test-Result 'restore brings the candidate back' (Get-Content (Join-Path $script:CandidateSrc 'MARKER')) 'candidate'
# Without the stash guard this deletes the just-restored source and then throws.
$secondRestoreOk = $true
try { Restore-CandidateSource } catch { $secondRestoreOk = $false }
Test-Result 'restore is idempotent (second call does not throw)' $secondRestoreOk $true
Test-Result 'restore is idempotent (candidate survives)' (Test-Path -LiteralPath (Join-Path $script:CandidateSrc 'MARKER')) $true

Write-Host 'unreadable baseline version'
New-Repo (Join-Path $work 'r3') '0.2.0' '0.1.0'
$baseManifest = Join-Path $script:BaselineTree 'mssql-tds' 'Cargo.toml'
Get-Content $baseManifest | Where-Object { $_ -notmatch '^version' } | Set-Content -LiteralPath $baseManifest
$threw = $false
try { Set-BaselineSource | Out-Null } catch { $threw = $true }
Test-Result 'swap throws when the baseline version is unreadable' $threw $true
Restore-CandidateSource
Test-Result 'candidate is recoverable after a failed swap' (Get-Content (Join-Path $script:CandidateSrc 'MARKER')) 'candidate'

# The swap is fallible, so it must run inside the try whose finally restores the
# candidate; otherwise a stamping throw strands the candidate in the stash directory.
Write-Host 'cleanup ordering'
$ordered = $harness -match '(?ms)try \{\s*\r?\n\s*Set-BaselineSource'
if ($ordered) { Write-Host '  ok   - Set-BaselineSource runs inside the protecting try/finally' }
else { Write-Host '  FAIL - Set-BaselineSource must run inside the try whose finally restores the candidate' -ForegroundColor Red; $failed = 1 }

Remove-Item -Recurse -Force $work
if ($failed -eq 0) { Write-Host 'All source-swap tests passed.' } else { Write-Host 'Source-swap tests FAILED.' -ForegroundColor Red }
exit $failed
