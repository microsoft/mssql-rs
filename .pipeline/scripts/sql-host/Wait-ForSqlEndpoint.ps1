# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Wait-ForSqlEndpoint.ps1 — Windows counterpart of poll-for-endpoint.sh.
#
# Blocks until the cross-pool SQL host publishes its `sql-ready` sentinel
# artifact, downloads it, and exports the endpoint as DB_HOST / DB_PORT for
# downstream steps. See sql-host-template.yml for why the rendezvous uses
# pipeline artifacts in both directions instead of ADO `dependsOn`.
#
# Required env:
#   ARTIFACT_NAME         Name of the sql-ready sentinel artifact.
#   BUILD_BUILDID
#   SYSTEM_COLLECTIONURI
#   SYSTEM_TEAMPROJECT
#   SYSTEM_ACCESSTOKEN    Expose via env: in the YAML step.
#   AGENT_TEMPDIRECTORY
#
# Optional env:
#   POLL_INTERVAL_SECONDS            default 10
#   MAX_WAIT_SECONDS                 default 3600
#   SYSTEM_STAGEATTEMPT              used to detect a partial "Rerun failed jobs"
#   RERUN_HINT_AFTER_SECONDS         default 600
#   MIN_HOST_LIFE_REMAINING_SECONDS  default 300

$ErrorActionPreference = 'Stop'

foreach ($required in 'ARTIFACT_NAME', 'BUILD_BUILDID', 'SYSTEM_COLLECTIONURI', 'SYSTEM_TEAMPROJECT', 'SYSTEM_ACCESSTOKEN', 'AGENT_TEMPDIRECTORY') {
    if (-not [Environment]::GetEnvironmentVariable($required)) {
        throw "$required is required"
    }
}

$artifactName = $env:ARTIFACT_NAME
$pollInterval = if ($env:POLL_INTERVAL_SECONDS) { [int]$env:POLL_INTERVAL_SECONDS } else { 10 }
$maxWait = if ($env:MAX_WAIT_SECONDS) { [int]$env:MAX_WAIT_SECONDS } else { 3600 }
$stageAttempt = if ($env:SYSTEM_STAGEATTEMPT) { [int]$env:SYSTEM_STAGEATTEMPT } else { 1 }
$rerunHintAfter = if ($env:RERUN_HINT_AFTER_SECONDS) { [int]$env:RERUN_HINT_AFTER_SECONDS } else { 600 }
$minHostLife = if ($env:MIN_HOST_LIFE_REMAINING_SECONDS) { [int]$env:MIN_HOST_LIFE_REMAINING_SECONDS } else { 300 }

$headers = @{ Authorization = "Bearer $($env:SYSTEM_ACCESSTOKEN)" }
$collection = $env:SYSTEM_COLLECTIONURI.TrimEnd('/')
$projectEnc = [uri]::EscapeDataString($env:SYSTEM_TEAMPROJECT)
$url = "$collection/$projectEnc/_apis/build/builds/$($env:BUILD_BUILDID)/artifacts?api-version=7.1"

Write-Host '=== sql-host/Wait-ForSqlEndpoint.ps1 ==='
Write-Host "  ARTIFACT_NAME = $artifactName"
Write-Host "  Build         = $($env:BUILD_BUILDID)"
Write-Host "  Poll URL      = $url"
Write-Host "  Poll every    = ${pollInterval}s"
Write-Host "  Max wait      = ${maxWait}s"

# The SQL host is a sibling job released as soon as tests finish, so ADO's
# "Rerun failed jobs" never restarts it and no host publishes a sentinel for the
# new attempt. Surface that as actionable guidance rather than a timeout.
function Write-PartialRerunGuidance {
    Write-Host "##vso[task.logissue type=error]SQL host endpoint '$artifactName' never appeared for stage attempt $stageAttempt."
    Write-Host "##vso[task.logissue type=error]The on-demand SQL host runs as a sibling job in this stage and is released once tests finish. 'Rerun failed jobs' does NOT restart it, so this rerun has no SQL server to connect to."
    Write-Host "##vso[task.logissue type=error]Fix: use 'Rerun stage' (or re-queue the pipeline) instead of 'Rerun failed jobs' so the SQL host job runs again alongside the tests."
}

$start = Get-Date
$deadline = $start.AddSeconds($maxWait)
$downloadUrl = $null
$rerunHinted = $false

while ($true) {
    $now = Get-Date
    if ($now -ge $deadline) {
        if ($stageAttempt -gt 1) {
            Write-PartialRerunGuidance
        } else {
            Write-Host "##vso[task.logissue type=error]artifact $artifactName did not appear within ${maxWait}s (the SQL host job may have failed to start)."
        }
        throw "artifact $artifactName did not appear within ${maxWait}s."
    }

    try {
        $response = Invoke-RestMethod -Uri $url -Headers $headers -Method Get
        $match = $response.value | Where-Object { $_.name -eq $artifactName } | Select-Object -First 1
        if ($match) {
            $downloadUrl = $match.resource.downloadUrl
            Write-Host "Endpoint sentinel $artifactName found."
            break
        }
    } catch {
        Write-Warning "artifact list HTTP error: $_"
    }

    $elapsed = ($now - $start).TotalSeconds
    if (-not $rerunHinted -and $stageAttempt -gt 1 -and $elapsed -ge $rerunHintAfter) {
        Write-Host "##vso[task.logissue type=warning]No SQL host endpoint after $([int]($elapsed / 60)) min on stage attempt $stageAttempt. If you used 'Rerun failed jobs', the SQL host job was NOT restarted and this will time out — cancel and use 'Rerun stage' instead."
        $rerunHinted = $true
    }
    Write-Host "$($now.ToUniversalTime().ToString('HH:mm:ss')) waiting for $artifactName; $([int]($deadline - $now).TotalSeconds)s left."
    Start-Sleep -Seconds $pollInterval
}

$downloadDir = Join-Path $env:AGENT_TEMPDIRECTORY $artifactName
$extractDir = Join-Path $downloadDir 'extracted'
Remove-Item -Recurse -Force $downloadDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $downloadDir -Force | Out-Null
$zipPath = Join-Path $downloadDir 'artifact.zip'

$downloaded = $false
foreach ($attempt in 1..5) {
    try {
        Invoke-WebRequest -Uri $downloadUrl -Headers $headers -OutFile $zipPath -UseBasicParsing
        $downloaded = $true
        break
    } catch {
        Write-Warning "artifact download attempt $attempt failed: $_"
        Start-Sleep -Seconds ($attempt * 5)
    }
}
if (-not $downloaded) {
    throw "failed to download $artifactName after 5 attempts."
}

Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

$endpointFile = Get-ChildItem -Path $extractDir -Filter 'endpoint.txt' -Recurse -File | Select-Object -First 1
if (-not $endpointFile) {
    Get-ChildItem -Path $extractDir -Recurse -File | ForEach-Object { Write-Host $_.FullName }
    throw "endpoint.txt not found inside $artifactName artifact."
}

# endpoint.txt format: "HOST PORT DEADLINE_EPOCH" on a single line.
$fields = (Get-Content -Path $endpointFile.FullName -TotalCount 1) -split '\s+' | Where-Object { $_ }
if ($fields.Count -lt 2) {
    throw "endpoint.txt is malformed: '$(Get-Content -Path $endpointFile.FullName -Raw)'"
}
$dbHost = $fields[0]
$dbPort = $fields[1]

if ($fields.Count -ge 3) {
    $remaining = [int]$fields[2] - [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    if ($remaining -lt $minHostLife) {
        Write-Host "  This usually means the SQL host hit MAX_LIFETIME_MINUTES while this test job was queued."
        throw "SQL host endpoint expires in ${remaining}s (need >= ${minHostLife}s)."
    }
    Write-Host "SQL host has ${remaining}s of life remaining (over the ${minHostLife}s threshold)."
}

Write-Host "Resolved SQL endpoint: host=$dbHost port=$dbPort"
Write-Host "##vso[task.setvariable variable=DB_HOST]$dbHost"
Write-Host "##vso[task.setvariable variable=DB_PORT]$dbPort"
