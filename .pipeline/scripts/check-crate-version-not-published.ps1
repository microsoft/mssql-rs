# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Preflight guard for the sandbox crate release flow.

.DESCRIPTION
  Fails (exit 1) when a crate version about to be published is already present
  in the Azure Artifacts Cargo registry, so the pipeline fails in seconds
  instead of building and only hitting cargo's duplicate-version rejection at
  `cargo publish`.

  The mssql-rs_Public feed allows anonymous reads, so the PEP-equivalent Cargo
  sparse index is queried without credentials. Each crate's index file is
  newline-delimited JSON, one object per published version.

  Best-effort: if the registry cannot be reached (auth/network), the script
  WARNs and exits 0. cargo's duplicate-version rejection at publish time remains
  the authoritative guard, so a transient lookup failure never blocks a release.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$IndexBaseUrl,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string[]]$Crates
)

$ErrorActionPreference = 'Stop'

function Get-CargoIndexPath([string]$name) {
    $n = $name.ToLower()
    switch ($n.Length) {
        1 { return "1/$n" }
        2 { return "2/$n" }
        3 { return "3/$($n[0])/$n" }
        default { return "$($n.Substring(0,2))/$($n.Substring(2,2))/$n" }
    }
}

$base = $IndexBaseUrl.TrimEnd('/')
$conflict = $false

foreach ($crate in $Crates) {
    $url = "$base/$(Get-CargoIndexPath $crate)"
    Write-Host "Release preflight: checking registry for $crate@$Version"
    Write-Host "  GET $url"

    $content = $null
    try {
        $resp = Invoke-WebRequest -Uri $url -TimeoutSec 60 -ErrorAction Stop
        $content = $resp.Content
    }
    catch {
        $code = $null
        if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode }
        if ($code -eq 404) {
            Write-Host "  OK: $crate has no published versions yet."
            continue
        }
        Write-Host "##vso[task.logissue type=warning]Registry lookup for $crate failed (HTTP $code); skipping preflight for this crate."
        continue
    }

    $versions = @()
    foreach ($line in ($content -split "`n")) {
        $line = $line.Trim()
        if (-not $line) { continue }
        try { $versions += ($line | ConvertFrom-Json).vers } catch { }
    }

    if ($versions -contains $Version) {
        Write-Host "##vso[task.logissue type=error]$crate@$Version is already published to the registry."
        $conflict = $true
    }
    else {
        Write-Host "  OK: $crate@$Version is not on the registry."
    }
}

if ($conflict) {
    Write-Error "One or more crate versions are already published. Bump the [package].version in the crate Cargo.toml before running a release. Azure Artifacts rejects re-publishing an existing version."
    exit 1
}

Write-Host "All crate versions are clear to publish."
