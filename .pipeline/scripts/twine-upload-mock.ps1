# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Uploads the collected mock wheels to the feed.

.DESCRIPTION
  Azure Artifacts' PyPI endpoint rejects --skip-existing (UnsupportedConfiguration).
  Each run stamps a unique .dev<date><BuildId> version, so there is no collision to
  skip.

.PARAMETER FeedName
  Twine repository name matching the .pypirc entry created by TwineAuthenticate@1
  (typically $(pythonFeedName)).

.PARAMETER ConfigFile
  Path to the .pypirc written by TwineAuthenticate@1 (typically $(PYPIRC_PATH)).

.PARAMETER DistDir
  Directory containing the wheels to upload (typically $(distDir)).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$FeedName,
    [Parameter(Mandatory = $true)][string]$ConfigFile,
    [Parameter(Mandatory = $true)][string]$DistDir
)

$ErrorActionPreference = 'Stop'

Write-Host "Uploading SANDBOX wheels to $FeedName ..."
twine upload -r $FeedName --config-file $ConfigFile (Join-Path $DistDir '*.whl')
