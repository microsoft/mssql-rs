# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Stamps a prerelease version into the crate manifests.

.DESCRIPTION
  Stamps all published workspace crates and their mssql-tds dependencies using
  the same rules as the official build. The sandbox publishes only mssql-tds and
  mssql-mock-tds, but all workspace requirements must resolve after stamping.

.PARAMETER ReleaseVersion
  'True' publishes the base version as-is (e.g. 1.0.0). Anything else appends a
  -dev.<date>.<BuildId> segment.

.PARAMETER BuildId
  Azure DevOps build id, used in the dev segment.
#>
[CmdletBinding()]
param(
    [string]$ReleaseVersion = 'False',
    [Parameter(Mandatory = $true)][string]$BuildId
)

$ErrorActionPreference = 'Stop'

$isOfficial = if ($ReleaseVersion -eq 'True') { 'True' } else { 'False' }
& "$PSScriptRoot/stamp-crate-versions.ps1" `
    -BuildReason 'Manual' -BuildId $BuildId -IsOfficial $isOfficial
