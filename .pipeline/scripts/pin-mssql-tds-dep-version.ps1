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

# Cargo identifies a source by the literal URL string, so the plain and ~force-auth
# spellings of this feed are two unrelated sources to it. Whichever one we bake here
# is where consumers resolve mssql-tds from, and a wrong choice only surfaces long
# after the publish succeeds -- unlike the other $(cargoSparseIndex) callers, which
# merely poll the index and fail loudly in the same run.
#
# The plain URL is not free: an internal consumer whose alias points at ~force-auth
# and who also depends on mssql-tds directly gets two copies in the graph, and a
# confusing "expected `mssql_tds::X`, found `mssql_tds::X`" where a type crosses
# between them. They can fix that by pointing their own alias at the plain URL.
# Baking ~force-auth has no such escape hatch: every anonymous read of this public
# feed would fail to resolve, which is the breakage this pinning exists to prevent.
if ($IndexUrl -match '~force-auth') {
    Write-Error "IndexUrl must be the plain feed URL, not the ~force-auth variant: $IndexUrl"
    exit 1
}

$index = if ($IndexUrl -match '^sparse\+') { $IndexUrl } else { "sparse+$IndexUrl" }

$path = 'mssql-mock-tds/Cargo.toml'
$c = Get-Content $path -Raw
$c = $c -replace 'mssql-tds\s*=\s*\{\s*path\s*=\s*"\.\./mssql-tds"',
                 "mssql-tds = { path = `"../mssql-tds`", version = `"$Version`", registry-index = `"$index`""

# -replace changes nothing when the dependency line drifts from the shape above
# (keys reordered, workspace = true), so without this the script would exit 0 having
# done nothing and the run would fail much later, in cargo publish -p mssql-mock-tds
# -- after cargo-publish-mock.ps1 has already put mssql-tds@$Version on the feed.
# Feed versions are immutable, so that burns the version and the retry cannot
# succeed. Counting rather than merely testing for presence also catches a second
# run appending a duplicate set of keys, which TOML rejects.
$dep = [regex]::Match($c, '(?m)^\s*mssql-tds\s*=\s*\{[^}]*\}')
if (-not $dep.Success) {
    Write-Error "No mssql-tds dependency line found in $path"
    exit 1
}
foreach ($key in 'version', 'registry-index') {
    $n = ([regex]::Matches($dep.Value, "(?<![\w-])$key\s*=")).Count
    if ($n -ne 1) {
        Write-Error "Expected exactly one '$key' in the mssql-tds dependency of ${path}, found ${n}: $($dep.Value.Trim())"
        exit 1
    }
}

Set-Content $path $c -NoNewline

Write-Host "Pinned mssql-tds dependency to version $Version (index: $index)"
Select-String -Path $path -Pattern 'mssql-tds\s*=' | ForEach-Object { Write-Host $_.Line }
