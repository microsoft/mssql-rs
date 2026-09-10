# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$RepositoryDirectory,
    [Parameter(Mandatory = $true)][ValidatePattern('^[0-9a-fA-F]{40}$')][string]$CommitSha,
    [Parameter(Mandatory = $true)][ValidatePattern('^refs/heads/.+')][string]$SourceBranch,
    [switch]$IncludeWheelMetadata
)

$ErrorActionPreference = 'Stop'

git -C $RepositoryDirectory fetch origin $SourceBranch
if ($LASTEXITCODE -ne 0) {
    throw "Could not fetch selected build branch $SourceBranch in $RepositoryDirectory"
}
git -C $RepositoryDirectory cat-file -e "${CommitSha}^{commit}"
if ($LASTEXITCODE -ne 0) {
    throw "Selected build commit $CommitSha was not found after fetching $SourceBranch"
}

function Get-BuildSourceFile {
    param([string]$Path)

    $content = & git -C $RepositoryDirectory show "${CommitSha}:$Path"
    if ($LASTEXITCODE -ne 0) {
        throw "Could not read $Path from selected build commit $CommitSha"
    }
    return $content -join "`n"
}

function Get-TomlField {
    param([string]$Content, [string]$Section, [string]$Field)

    $sectionMatch = [regex]::Match(
        $Content, ('(?ms)^\[{0}\]\s*$(?<body>.*?)(?=^\[|\z)' -f [regex]::Escape($Section))
    )
    $fieldMatch = [regex]::Match(
        $sectionMatch.Groups['body'].Value, ('(?m)^\s*{0}\s*=\s*"([^"]+)"' -f [regex]::Escape($Field))
    )
    if (-not $fieldMatch.Success) {
        throw "Could not extract [$Section].$Field from selected build commit $CommitSha"
    }
    return $fieldMatch.Groups[1].Value
}

$cargoToml = Get-BuildSourceFile 'mssql-py-core/Cargo.toml'
$metadata = @{
    SourceCommit = $CommitSha
    Version = Get-TomlField $cargoToml 'package' 'version'
}
if ($IncludeWheelMetadata) {
    $pyprojectToml = Get-BuildSourceFile 'mssql-py-core/pyproject.toml'
    $metadata.DistributionName = Get-TomlField $pyprojectToml 'project' 'name'
    $metadata.PythonVersion = Get-TomlField $pyprojectToml 'project' 'version'
}

[pscustomobject]$metadata
