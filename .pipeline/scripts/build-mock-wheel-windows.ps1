# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  SANDBOX / TEST-ONLY helper. Builds the mock TDS abi3 wheel natively on Windows.

.DESCRIPTION
  abi3 build: one wheel, no per-Python-version loop. --manifest-path is required
  because the Cargo package is still mssql-mock-tds-py while the distribution is
  mssql-mock-tds.

.PARAMETER OutputDir
  Directory the wheel is written to (typically $(ob_outputDirectory)).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputDir
)

$ErrorActionPreference = 'Stop'

python --version
pip install maturin

$wheels = Join-Path $OutputDir 'wheels'
New-Item -ItemType Directory -Force -Path $wheels | Out-Null

maturin build --release --auditwheel skip `
    --interpreter python `
    --manifest-path mssql-mock-tds-py\Cargo.toml `
    --out $wheels

Get-ChildItem $wheels | Format-Table Name, Length
