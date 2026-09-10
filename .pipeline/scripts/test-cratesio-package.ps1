# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Verifies that a crate version is absent from or available on crates.io.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$CrateName,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)]
    [ValidateSet('Absent', 'Available')]
    [string]$ExpectedState,
    [ValidateRange(1, 100)][int]$MaxAttempts = 40,
    [ValidateRange(1, 300)][int]$DelaySeconds = 15
)

$ErrorActionPreference = 'Stop'
$uri = "https://crates.io/api/v1/crates/$CrateName/$Version"
$headers = @{
    'User-Agent' = 'mssql-rs-release-pipeline (https://github.com/microsoft/mssql-rs)'
}

for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
    try {
        $response = Invoke-WebRequest -Uri $uri -Headers $headers -SkipHttpErrorCheck -TimeoutSec 30
    }
    catch {
        if ($ExpectedState -eq 'Absent' -or $attempt -eq $MaxAttempts) {
            throw "Failed to query crates.io for ${CrateName}@${Version}: $($_.Exception.Message)"
        }
        Write-Host "Transient crates.io request failure: $($_.Exception.Message)"
        Start-Sleep -Seconds $DelaySeconds
        continue
    }

    $statusCode = [int]$response.StatusCode
    if ($ExpectedState -eq 'Absent') {
        if ($statusCode -eq 404) {
            Write-Host "$CrateName@$Version is not published."
            return
        }
        if ($statusCode -eq 200) {
            throw "$CrateName@$Version is already published and cannot be overwritten."
        }
        throw "crates.io returned HTTP $statusCode for $uri"
    }

    if ($statusCode -eq 200) {
        Write-Host "$CrateName@$Version is available on crates.io."
        return
    }
    if ($statusCode -ne 404 -and $statusCode -ne 429 -and
        ($statusCode -lt 500 -or $statusCode -gt 599)) {
        throw "crates.io returned HTTP $statusCode for $uri"
    }

    if ($attempt -lt $MaxAttempts) {
        if ($statusCode -eq 404) {
            Write-Host "Waiting for $CrateName@$Version on crates.io ($attempt/$MaxAttempts)..."
        }
        else {
            Write-Host "Transient HTTP $statusCode from crates.io; retrying ($attempt/$MaxAttempts)..."
        }
        Start-Sleep -Seconds $DelaySeconds
    }
}

throw "$CrateName@$Version was not available on crates.io after $MaxAttempts attempts."
