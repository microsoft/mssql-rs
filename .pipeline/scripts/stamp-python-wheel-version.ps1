# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Stamps the run's wheel version into [project].version of mssql-py-core's
  pyproject.toml (Windows jobs; Linux/macOS jobs use stamp-python-wheel-version.sh).

.DESCRIPTION
  maturin takes the wheel version from the static [project].version, so that is
  the only field stamped. Cargo.toml is left alone: the Rust crate version is
  independent of the Python distribution version and does not affect the wheel.

  Only the `version` line inside the [project] table is rewritten. The script
  fails if that table has no single `version = "..."` line, or if the value is
  not a PEP 440 version (for example an unexpanded `$(pyCoreWheelVersion)` macro).

.PARAMETER Version
  The version resolved once by compute-python-wheel-version.ps1.

.PARAMETER PyprojectPath
  Path to pyproject.toml (default mssql-py-core/pyproject.toml).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Version,
    [string]$PyprojectPath = 'mssql-py-core/pyproject.toml'
)

$ErrorActionPreference = 'Stop'

if ($Version -notmatch '^\d+(\.\d+)*((a|b|rc)\d+)?(\.post\d+)?(\.dev\d+)?$') {
    throw "Refusing to stamp '$Version' into $PyprojectPath`: not a PEP 440 version. An unexpanded `$(...)` macro means the compute step emitted no value."
}

$text = Get-Content $PyprojectPath -Raw
$section = [regex]::Match($text, '(?ms)^\[project\][ \t]*\r?\n(?<body>.*?)(?=^\[|\z)')
if (-not $section.Success) {
    throw "No [project] table in $PyprojectPath"
}
$body = $section.Groups['body']
$fieldPattern = '(?m)^(?<prefix>[ \t]*version[ \t]*=[ \t]*)"[^"]*"'
$fields = [regex]::Matches($body.Value, $fieldPattern)
if ($fields.Count -ne 1) {
    throw "Expected one version line in [project] of $PyprojectPath, found $($fields.Count)"
}

$field = $fields[0]
$start = $body.Index + $field.Index
$replacement = $field.Groups['prefix'].Value + '"' + $Version + '"'
$updated = $text.Substring(0, $start) + $replacement + $text.Substring($start + $field.Length)
Set-Content -Path $PyprojectPath -Value $updated -NoNewline

Write-Host "Stamped [project].version = `"$Version`" in $PyprojectPath"
