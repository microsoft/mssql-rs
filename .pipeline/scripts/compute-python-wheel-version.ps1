# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Resolves the mssql-python-rs wheel version ONCE for the whole run.

.DESCRIPTION
  Reads [project].version from mssql-py-core/pyproject.toml and emits the PEP 440
  version every build job stamps into the wheel, as the `pyCoreWheelVersion`
  pipeline OUTPUT variable. Computing it once (UTC date resolved here) keeps the
  parallel build jobs on an identical version even across a UTC-midnight boundary.

  Channel selection (PyPI feeds store a distribution by the version baked into the
  wheel, so each run needs a unique PEP 440 version):
    * Official release  -> committed version as-is      (e.g. 0.3.0)
    * Nightly schedule  -> base + .dev<date>           (e.g. 0.3.0.dev20260217)
    * CI / dev builds   -> base + .dev<date><buildId>  (e.g. 0.3.0.dev20260217123456)

  A value is always emitted. Azure Pipelines leaves `$(name)` macros unexpanded when
  the variable is empty, so an empty output would reach the build and Publish jobs
  as the literal text `$(pyCoreWheelVersion)`. For official builds the stamp
  rewrites [project].version with its committed value, which is a no-op.

.PARAMETER PyprojectPath
  Path to the distribution's pyproject.toml (default mssql-py-core/pyproject.toml).

.PARAMETER BuildReason
  $(Build.Reason) — 'Schedule' selects the nightly channel; everything else is dev.

.PARAMETER BuildId
  $(Build.BuildId), appended to the dev segment to keep CI builds unique.

.PARAMETER IsOfficial
  'True' selects the release channel (the committed version, no .dev segment).
#>
[CmdletBinding()]
param(
    [string]$PyprojectPath = 'mssql-py-core/pyproject.toml',
    [Parameter(Mandatory = $true)][string]$BuildReason,
    [Parameter(Mandatory = $true)][string]$BuildId,
    [string]$IsOfficial = 'False'
)

$ErrorActionPreference = 'Stop'

$py = Get-Content $PyprojectPath -Raw
$section = [regex]::Match($py, '(?ms)^\[project\]\s*$(?<body>.*?)(?=^\[|\z)')
$field = [regex]::Match($section.Groups['body'].Value, '(?m)^\s*version\s*=\s*"([^"]+)"')
if (-not $field.Success) {
    throw "Could not read [project].version from $PyprojectPath"
}
$base = $field.Groups[1].Value

if ($IsOfficial -eq 'True') {
    $ver = $base
}
elseif ($BuildReason -eq 'Schedule') {
    $ver = "$base.dev$([DateTime]::UtcNow.ToString('yyyyMMdd'))"
}
else {
    $ver = "$base.dev$([DateTime]::UtcNow.ToString('yyyyMMdd'))$BuildId"
}

Write-Host "Resolved wheel version (computed once): $ver"
Write-Host "Base: $base | Reason: $BuildReason | Official: $IsOfficial"
Write-Host "##vso[task.setvariable variable=pyCoreWheelVersion;isOutput=true]$ver"
