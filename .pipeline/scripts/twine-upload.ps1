# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Uploads the collected wheels to the Azure Artifacts PyPI feed, resuming safely
  after a partial upload.

.DESCRIPTION
  Azure Artifacts' PyPI endpoint rejects --skip-existing (UnsupportedConfiguration)
  and rejects re-uploading a file that already exists. If an earlier attempt
  uploaded only part of the wheel set, a plain retry would stop at the first
  duplicate and leave the version's platform matrix incomplete.

  Before uploading, this script reads the feed's simple index for the package and
  compares each local wheel by filename and SHA-256:
    * already published with the same hash -> skipped
    * already published with a different hash -> fails (never overwrite a release)
    * not published -> uploaded
  The index lookup fails closed: if it cannot be read, nothing is uploaded.

  Credentials for the upload come from the .pypirc that TwineAuthenticate@1 writes.

.PARAMETER FeedName
  Twine repository name matching the .pypirc entry from TwineAuthenticate@1
  (typically $(pythonFeedName)).

.PARAMETER ConfigFile
  Path to the .pypirc written by TwineAuthenticate@1 (typically $(PYPIRC_PATH)).

.PARAMETER DistDir
  Directory containing the wheels to upload (typically $(distDir)).

.PARAMETER SimpleIndexUrl
  The feed's PEP 503 simple index root (typically $(pythonSimpleIndex)). The
  mssql-rs_Public feed allows anonymous reads.

.PARAMETER PackageName
  Distribution name to look up in the simple index.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$FeedName,
    [Parameter(Mandatory = $true)][string]$ConfigFile,
    [Parameter(Mandatory = $true)][string]$DistDir,
    [Parameter(Mandatory = $true)][string]$SimpleIndexUrl,
    [string]$PackageName = 'mssql-python-rs'
)

$ErrorActionPreference = 'Stop'

$wheels = @(Get-ChildItem $DistDir -Filter *.whl -File | Sort-Object Name)
if ($wheels.Count -eq 0) {
    throw "No wheels found to upload in $DistDir"
}

$normalizedName = [regex]::Replace($PackageName.ToLowerInvariant(), '[-_.]+', '-')
$packageUrl = $SimpleIndexUrl.TrimEnd('/') + "/$normalizedName/"
$response = Invoke-WebRequest -Uri $packageUrl -UseBasicParsing -SkipHttpErrorCheck
$published = @{}
if ($response.StatusCode -eq 404) {
    Write-Host "$PackageName has no files on the feed yet."
}
elseif ($response.StatusCode -ne 200) {
    throw "Could not read $packageUrl (HTTP $($response.StatusCode)); refusing to upload without knowing what is already published."
}
else {
    $html = if ($response.Content -is [byte[]]) {
        [System.Text.Encoding]::UTF8.GetString($response.Content)
    }
    else {
        [string]$response.Content
    }
    foreach ($match in [regex]::Matches($html, '<a\s[^>]*href="[^"#]*/([^/"#]+\.whl)#sha256=([0-9a-fA-F]{64})"')) {
        $published[$match.Groups[1].Value.ToLowerInvariant()] = $match.Groups[2].Value.ToLowerInvariant()
    }
}

$toUpload = @()
foreach ($wheel in $wheels) {
    $remoteHash = $published[$wheel.Name.ToLowerInvariant()]
    if (-not $remoteHash) {
        $toUpload += $wheel.FullName
        continue
    }
    $localHash = (Get-FileHash $wheel.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($localHash -ne $remoteHash) {
        throw "$($wheel.Name) is already on the feed with a different SHA-256 (feed $remoteHash, local $localHash). Published files are immutable; bump the version instead."
    }
    Write-Host "Already published, identical: $($wheel.Name)"
}

if ($toUpload.Count -eq 0) {
    Write-Host "All $($wheels.Count) wheel(s) are already published to '$FeedName'; nothing to upload."
    exit 0
}

Write-Host "Uploading $($toUpload.Count) of $($wheels.Count) wheel(s) to feed '$FeedName' ..."
twine upload -r $FeedName --config-file $ConfigFile @toUpload
if ($LASTEXITCODE -ne 0) {
    throw "twine upload failed with exit code $LASTEXITCODE"
}
