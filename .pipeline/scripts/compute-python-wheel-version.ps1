# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Resolves the mssql-python-rs wheel version ONCE for the whole run.

.DESCRIPTION
  Reads the base version from mssql-py-core/pyproject.toml and emits the PEP 440
  version every build job must stamp into the wheel, as the `pyCoreWheelVersion`
  pipeline OUTPUT variable. Computing it once (UTC date resolved here) keeps the
  parallel build jobs on an identical version even across a UTC-midnight boundary.

  Channel selection (PyPI feeds store a distribution by the version baked into the
  wheel, so each run needs a unique PEP 440 version):
    * Official release  -> empty (wheel keeps the committed manifest version)
    * Nightly schedule  -> base + .dev<date>           (e.g. 0.1.0.dev20260217)
    * CI / dev builds   -> base + .dev<date><buildId>  (e.g. 0.1.0.dev20260217123456)

.PARAMETER PyprojectPath
  Path to the distribution's pyproject.toml (default mssql-py-core/pyproject.toml).

.PARAMETER BuildReason
  $(Build.Reason) — 'Schedule' selects the nightly channel; everything else is dev.

.PARAMETER BuildId
  $(Build.BuildId), appended to the dev segment to keep CI builds unique.

.PARAMETER IsOfficial
  'True' leaves the version empty so the build keeps the committed manifest version
  (the release channel publishes the exact version in source).
#>
[CmdletBinding()]
param(
    [string]$PyprojectPath = 'mssql-py-core/pyproject.toml',
    [Parameter(Mandatory = $true)][string]$BuildReason,
    [Parameter(Mandatory = $true)][string]$BuildId,
    [string]$IsOfficial = 'False'
)

$ErrorActionPreference = 'Stop'

if ($IsOfficial -eq 'True') {
    Write-Host 'Official build: leaving the wheel at its committed manifest version (no stamp).'
    Write-Host "##vso[task.setvariable variable=pyCoreWheelVersion;isOutput=true]"
    exit 0
}

$py = Get-Content $PyprojectPath -Raw
if ($py -match '(?ms)^\[project\].*?^version\s*=\s*"([^"]+)"') { $base = $Matches[1] }
else { Write-Error "Could not read [project].version from $PyprojectPath"; exit 1 }

$date = [DateTime]::UtcNow.ToString('yyyyMMdd')
if ($BuildReason -eq 'Schedule') {
    $ver = "$base.dev$date"          # nightly: one build per day
}
else {
    $ver = "$base.dev$date$BuildId"  # CI/dev: unique per build
}

Write-Host "Resolved wheel version (computed once): $ver"
Write-Host "Base: $base | Reason: $BuildReason"
Write-Host "##vso[task.setvariable variable=pyCoreWheelVersion;isOutput=true]$ver"
