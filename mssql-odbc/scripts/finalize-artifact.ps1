# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

param(
    [ValidateSet("debug", "release")]
    [string]$Profile = "debug"
)

$ErrorActionPreference = "Stop"
$OdbcCrateDir = Split-Path -Parent $PSScriptRoot

Push-Location $OdbcCrateDir
try {
    $Metadata = cargo metadata --format-version 1 --no-deps 2>$null | ConvertFrom-Json
    if (-not $Metadata.target_directory) {
        throw "cargo metadata did not return a target directory"
    }
} finally {
    Pop-Location
}

$SourcePath = Join-Path $Metadata.target_directory "$Profile\mssqlodbc.dll"
$ProductPath = Join-Path $Metadata.target_directory "$Profile\mssql-odbc.dll"
if (-not (Test-Path $SourcePath -PathType Leaf)) {
    throw "Cargo artifact not found at $SourcePath"
}

Copy-Item -Force $SourcePath $ProductPath
(Resolve-Path $ProductPath).Path