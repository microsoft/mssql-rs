# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Collects every built wheel into a single dist folder.

.DESCRIPTION
  Installs twine, flattens all *.whl artifacts downloaded from the build jobs into
  <StagingDir>\dist, and emits that path as the `distDir` pipeline variable for the
  upload step.

.PARAMETER StagingDir
  Build staging directory (typically $(Build.StagingDirectory)).

.PARAMETER SourceRoot
  Root to search recursively for wheels (typically $(Pipeline.Workspace)).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$StagingDir,
    [Parameter(Mandatory = $true)][string]$SourceRoot
)

$ErrorActionPreference = 'Stop'

python -m pip install --upgrade twine

$dist = Join-Path $StagingDir 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

Get-ChildItem $SourceRoot -Recurse -Filter *.whl | Copy-Item -Destination $dist

Write-Host "=== Collected wheels ==="
Get-ChildItem $dist -Filter *.whl | ForEach-Object {
    Write-Host "  $($_.Name)  ($([math]::Round($_.Length / 1KB, 1)) KB)"
}

Write-Host "##vso[task.setvariable variable=distDir]$dist"
