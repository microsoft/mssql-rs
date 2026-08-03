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

.PARAMETER SparseIndexBaseUrl
  Cargo sparse index base URL (typically $(cargoSparseIndex)). When supplied with
  -TdsVersion, the script polls the index until mssql-tds@<version> is visible
  before publishing mssql-mock-tds, instead of sleeping a fixed interval.

.PARAMETER TdsVersion
  The stamped mssql-tds version to wait for on the index (typically $(crateVersion)).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Registry,
    [switch]$DryRun,
    [string]$SparseIndexBaseUrl = '',
    [string]$TdsVersion = ''
)

$ErrorActionPreference = 'Stop'

. "$PSScriptRoot/Get-CargoIndexPath.ps1"

# Polls the sparse index until $Crate@$Version is published (or attempts run out).
# Best-effort: a network/lookup blip never blocks the publish, cargo's own verify
# build against the registry version is the authoritative gate.
function Wait-CrateOnIndex {
    param(
        [Parameter(Mandatory = $true)][string]$IndexBaseUrl,
        [Parameter(Mandatory = $true)][string]$Crate,
        [Parameter(Mandatory = $true)][string]$Version,
        [int]$MaxAttempts = 20,
        [int]$DelaySeconds = 15
    )
    $url = "$($IndexBaseUrl.TrimEnd('/'))/$(Get-CargoIndexPath $Crate)"
    for ($i = 1; $i -le $MaxAttempts; $i++) {
        try {
            $resp = Invoke-WebRequest -Uri $url -TimeoutSec 30 -ErrorAction Stop
            foreach ($line in ($resp.Content -split "`n")) {
                $line = $line.Trim()
                if (-not $line) { continue }
                try { if (($line | ConvertFrom-Json).vers -eq $Version) {
                    Write-Host "  $Crate@$Version visible on index (attempt $i)."
                    return
                } } catch { }
            }
        }
        catch {
            # 404 = crate/version not indexed yet; keep polling.
        }
        Write-Host "  Waiting for $Crate@$Version on sparse index (attempt $i/$MaxAttempts)..."
        Start-Sleep -Seconds $DelaySeconds
    }
    Write-Host "##vso[task.logissue type=warning]$Crate@$Version not visible on index after $MaxAttempts attempts; proceeding (cargo will verify)."
}

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
# verified/published. Poll the sparse index so we wait exactly as long as
# propagation takes; fall back to a short settle window if we can't poll.
if ($SparseIndexBaseUrl -and $TdsVersion) {
    Wait-CrateOnIndex -IndexBaseUrl $SparseIndexBaseUrl -Crate 'mssql-tds' -Version $TdsVersion
}
else {
    Write-Host "Sparse index / version not supplied; using a fixed settle window."
    Start-Sleep -Seconds 30
}
cargo publish -p mssql-mock-tds --registry $Registry --allow-dirty
