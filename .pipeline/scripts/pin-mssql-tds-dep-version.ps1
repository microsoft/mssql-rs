# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Pins the mssql-tds path dependency to a version.

.DESCRIPTION
  The mssql-tds path dependency in mssql-mock-tds/Cargo.toml has no version, which
  cargo publish rejects ("all dependencies must have a version"). This adds the
  stamped version while keeping the path (cargo uses the version on publish and the
  path for local verify builds).

  A version alone is not enough: cargo records a dependency with no registry as
  coming from crates.io, where mssql-tds does not exist. Consumers of the feed then
  fail to resolve mssql-mock-tds with "no matching package named `mssql-tds` found,
  location searched: crates.io index". CI does not hit this because
  .cargo/config.ci.toml source-replaces crates.io with the feed. So we also pin the
  feed's own index.

.PARAMETER Version
  The stamped crate version to pin (typically $(crateVersion)).

.PARAMETER IndexUrl
  Sparse index base URL of the feed being published to (typically
  $(cargoSparseIndex)). Pass the plain URL, not the ~force-auth variant, so that
  anonymous readers of a public feed can resolve the dependency.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$IndexUrl
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($Version)) { Write-Error 'Version not set'; exit 1 }
if ([string]::IsNullOrWhiteSpace($IndexUrl)) { Write-Error 'IndexUrl not set'; exit 1 }

# ~force-auth makes the endpoint reject anonymous reads. This URL is baked into the
# published manifest, so passing it here would silently force every consumer to
# authenticate, and the breakage only shows up long after the publish succeeds. The
# other $(cargoSparseIndex) callers merely poll the index, so a wrong URL fails
# loudly in the same run and needs no equivalent check.
if ($IndexUrl -match '~force-auth') {
    Write-Error "IndexUrl must be the plain feed URL, not the ~force-auth variant: $IndexUrl"
    exit 1
}

$index = if ($IndexUrl -match '^sparse\+') { $IndexUrl } else { "sparse+$IndexUrl" }

$path = 'mssql-mock-tds/Cargo.toml'
$c = Get-Content $path -Raw
$c = $c -replace 'mssql-tds\s*=\s*\{\s*path\s*=\s*"\.\./mssql-tds"',
                 "mssql-tds = { path = `"../mssql-tds`", version = `"$Version`", registry-index = `"$index`""
Set-Content $path $c -NoNewline

Write-Host "Pinned mssql-tds dependency to version $Version (index: $index)"
Select-String -Path $path -Pattern 'mssql-tds\s*=' | ForEach-Object { Write-Host $_.Line }
