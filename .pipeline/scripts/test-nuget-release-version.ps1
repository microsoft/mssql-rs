# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$')]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
$packageName = 'mssql-python-rs-wheels'
$feedIndex = 'https://pkgs.dev.azure.com/sqlclientdrivers/public/_packaging/mssql-rs_Public/nuget/v3/index.json'

# Resolve the package endpoint from the same public feed used by OneBranch.
# No publishing credential is needed for this read-only gate.
$response = Invoke-WebRequest -Uri $feedIndex -SkipHttpErrorCheck -TimeoutSec 30 -MaximumRedirection 0
if ($response.StatusCode -ne 200) {
    throw "NuGet service index returned HTTP $($response.StatusCode)"
}
$index = $response.Content | ConvertFrom-Json
$baseAddresses = @($index.resources | Where-Object { $_.'@type' -eq 'PackageBaseAddress/3.0.0' })
if ($baseAddresses.Count -ne 1) {
    throw 'Expected one NuGet PackageBaseAddress/3.0.0 resource.'
}
$baseAddress = [uri]$baseAddresses[0].'@id'
if (-not $baseAddress.IsAbsoluteUri -or $baseAddress.Scheme -ne 'https' -or
    $baseAddress.Host -ne 'pkgs.dev.azure.com') {
    throw 'Unexpected NuGet package base address.'
}
$uri = "$($baseAddress.AbsoluteUri.TrimEnd('/'))/$packageName/index.json"
$response = Invoke-WebRequest -Uri $uri -SkipHttpErrorCheck -TimeoutSec 30 -MaximumRedirection 0
if ($response.StatusCode -eq 404) {
    Write-Host "$packageName@$Version is not published."
    return
}
if ($response.StatusCode -ne 200) {
    throw "NuGet package index returned HTTP $($response.StatusCode)"
}
$package = $response.Content | ConvertFrom-Json
$versionPattern = '^[0-9]+\.[0-9]+\.[0-9]+(?:\.[0-9]+)?(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$'
if ($package.versions -isnot [array] -or $package.versions.Count -eq 0 -or
    @($package.versions | Where-Object { $_ -isnot [string] -or $_ -notmatch $versionPattern }).Count -ne 0) {
    throw 'NuGet package index has no valid versions array.'
}
$versions = @($package.versions | ForEach-Object { $_.Split('+')[0] })
if ($Version -in $versions -or "$Version.0" -in $versions) {
    throw "$packageName@$Version is already published and cannot be overwritten."
}
Write-Host "$packageName@$Version is not published."
