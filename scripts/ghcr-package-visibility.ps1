<#
.SYNOPSIS
    Audit (and help make public) the GHCR packages mirrored by sync-container-images.yml.

.DESCRIPTION
    GitHub does NOT expose a REST/GraphQL/gh-CLI endpoint to change container package
    visibility (see https://github.com/orgs/community/discussions/42937). Visibility can
    only be flipped from the web UI, per package. This script therefore:

      1. Reads each package's current visibility via the Packages REST API.
      2. Lists every package that is not yet 'public'.
      3. Prints the direct "Package settings" URL for each one (Danger Zone ->
         Change visibility -> Public), and optionally opens them in your browser.

    For anonymous pulls from the PR pipeline, every package must be Public. Note the
    microsoft org may restrict public packages to org owners; if the UI blocks the change,
    an org owner / admin must allow public packages or perform the flip.

.PARAMETER Token
    A GitHub PAT (classic) with at least read:packages. Defaults to $env:GHCR_TOKEN.
    Only used for the read-only visibility audit; not required to open the settings pages.

.PARAMETER Open
    Open each not-yet-public package's settings page in the default browser.

.EXAMPLE
    $env:GHCR_TOKEN = '<pat>'; ./scripts/ghcr-package-visibility.ps1

.EXAMPLE
    ./scripts/ghcr-package-visibility.ps1 -Open
#>
[CmdletBinding()]
param(
    [string]$Token = $env:GHCR_TOKEN,
    [string]$Org = 'microsoft',
    [string]$Repo = 'mssql-rs',
    [switch]$Open
)

$ErrorActionPreference = 'Stop'

# Package names are <repo>/<acr-path> (the org is the registry owner, the repo is part
# of the package name because images are pushed to ghcr.io/<org>/<repo>/<path>).
$paths = @(
    'python-build/manylinux_2_34_x86_64_rust'
    'python-build/manylinux_2_34_aarch64_rust'
    'python-build/musllinux_1_2_x86_64_rust'
    'python-build/musllinux_1_2_aarch64_rust'
    'build/ubuntu'
    'build/alpine'
    'import/debian'
    'import/alpine'
    'import/ubuntu'
    'import/redhat/ubi9'
    'import/oraclelinux'
)

$headers = $null
if ($Token) {
    $headers = @{
        Authorization          = "Bearer $Token"
        Accept                 = 'application/vnd.github+json'
        'X-GitHub-Api-Version' = '2022-11-28'
    }
}
else {
    Write-Warning 'No token provided (set $env:GHCR_TOKEN or pass -Token). Skipping the visibility audit; settings URLs will still be printed.'
}

$needsAction = New-Object System.Collections.Generic.List[string]

Write-Host ''
Write-Host ('{0,-12} {1}' -f 'VISIBILITY', 'PACKAGE')
Write-Host ('{0,-12} {1}' -f '----------', '-------')

foreach ($path in $paths) {
    $name = "$Repo/$path"
    $visibility = 'unknown'
    if ($headers) {
        $enc = [uri]::EscapeDataString($name)
        try {
            $pkg = Invoke-RestMethod -Headers $headers -Uri "https://api.github.com/orgs/$Org/packages/container/$enc"
            $visibility = $pkg.visibility
        }
        catch {
            $code = $_.Exception.Response.StatusCode.value__
            $visibility = "err:$code"
        }
    }
    Write-Host ('{0,-12} {1}' -f $visibility, $name)
    if ($visibility -ne 'public') {
        $needsAction.Add($name)
    }
}

Write-Host ''
if ($needsAction.Count -eq 0 -and $headers) {
    Write-Host 'All packages are already public. Nothing to do.' -ForegroundColor Green
    return
}

Write-Host 'Make these PUBLIC via the web UI (Package settings -> Danger Zone -> Change visibility -> Public):' -ForegroundColor Yellow
foreach ($name in $needsAction) {
    # Browser settings URL keeps the slashes unescaped.
    $url = "https://github.com/orgs/$Org/packages/container/$name/settings"
    Write-Host "  $url"
    if ($Open) {
        Start-Process $url
    }
}

Write-Host ''
Write-Host 'Reminder: there is no API/CLI to change package visibility; this must be done in the UI.'
