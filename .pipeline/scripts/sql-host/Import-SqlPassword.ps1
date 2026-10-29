# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

$ErrorActionPreference = 'Stop'

if ($env:SHARED_SQL_PASSWORD -cnotmatch '\AAa1![0-9a-f]{48}\z' -or
    $env:SQL_CREDENTIAL_GENERATION -cnotmatch '\A[0-9a-f]{32}\z') {
    throw 'Shared SQL credential output is missing or invalid. Queue a fresh validation run.'
}

Write-Host "##vso[task.setvariable variable=SQL_PASSWORD;issecret=true]$($env:SHARED_SQL_PASSWORD)"
Write-Host '##vso[task.setvariable variable=SQL_PASSWORD_GENERATED]1'
