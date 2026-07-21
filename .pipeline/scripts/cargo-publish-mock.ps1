# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Publishes (or dry-runs) the mock crates to the feed.

.DESCRIPTION
  mssql-mock-tds path-depends on mssql-tds; cargo publish rewrites that to a
  registry version dependency and verifies by building, so mssql-tds@<version> must
  already exist in the feed. We therefore publish in dependency order: mssql-tds
  first, then mssql-mock-tds.

  With -DryRun only mssql-tds is dry-run built. A dry run of mssql-mock-tds would
  need mssql-tds@<version> to already exist in the feed (cargo verifies by building
  against the registry version), so it is intentionally skipped.

.PARAMETER Registry
  Cargo registry name defined in .cargo/config.ci.toml (typically $(cargoRegistry)).

.PARAMETER DryRun
  When set, runs cargo publish --dry-run instead of publishing.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Registry,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

if ($DryRun) {
    Write-Host "================ DRY RUN ================"
    Write-Host "publishCrate=false -> cargo publish --dry-run only."
    Write-Host "========================================"
    cargo publish -p mssql-tds --registry $Registry --dry-run --allow-dirty
    Write-Host "Skipping mssql-mock-tds dry run (depends on a published mssql-tds)."
    return
}

Write-Host "Publishing SANDBOX crates to $Registry (mssql-tds first)..."
cargo publish -p mssql-tds --registry $Registry --allow-dirty
# mssql-tds must be queryable in the feed index before mssql-mock-tds is
# verified/published. Sparse index propagation is usually quick, but allow a short
# settle window for the sandbox.
Start-Sleep -Seconds 30
cargo publish -p mssql-mock-tds --registry $Registry --allow-dirty
