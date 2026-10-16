# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
  Stamps publishable crate manifests with the version for the current build.

.DESCRIPTION
  Official builds retain the source version. Non-official scheduled builds use a
  nightly version, while other non-official builds use a unique dev version.
  mssql-mock-tds is kept aligned with mssql-tds, including its dependency version.
  All inputs and planned edits are validated before logging versions or writing.
  See README.md for supported manifest syntax and version inputs.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$BuildReason,

    [Parameter(Mandatory = $true)]
    [string]$BuildId,

    [string]$IsOfficial = 'False',

    [string[]]$Crates = @('mssql-tds', 'mssql-mock-tds'),

    [switch]$WhatIf,

    [string]$RepositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..'))
)

$ErrorActionPreference = 'Stop'

function Assert-SemVer {
    param([Parameter(Mandatory = $true)][string]$Version)

    $number = '(?:0|[1-9][0-9]*)'
    $identifier = '(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)'
    $pattern = "\A$number\.$number\.$number(?:-$identifier(?:\.$identifier)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\z"
    if ($Version -cnotmatch $pattern) {
        throw 'Expected a valid single-line SemVer version.'
    }
    foreach ($component in ($Version -split '[-+]', 2)[0].Split('.')) {
        $value = [uint64]0
        if (-not [uint64]::TryParse($component, [ref]$value)) {
            throw 'SemVer core components must fit a Cargo unsigned 64-bit integer.'
        }
    }
}

function Assert-BareSectionKeys {
    param([Parameter(Mandatory = $true)][string]$Content, [Parameter(Mandatory = $true)][string]$Path)

    if ($Content -match '(?m)^[ \t]*(?:"(?:\\.|[^"\\\r\n])*"|''[^''\r\n]*'')[ \t]*[=.]') {
        throw "Quoted keys are unsupported in $Path"
    }
}

function Assert-SupportedManifestStructure {
    param([Parameter(Mandatory = $true)][string]$Content, [Parameter(Mandatory = $true)][string]$Path)

    # Consume ordinary strings/comments before detecting multiline delimiters.
    # Whole headers are skipped only outside arrays, before section extraction.
    $tokens = [regex]::Matches($Content, '#[^\r\n]*|(?<multiline>"""|'''''')|"(?:\\.|[^"\\\r\n])*"|''[^''\r\n]*''|(?<header>(?m:^[ \t]*\[)[^\r\n]*)|(?<open>\[)|(?<close>\])')
    $arrayDepth = 0
    foreach ($token in $tokens) {
        if ($token.Groups['multiline'].Success) {
            throw "Multiline strings are unsupported in $Path"
        }
        elseif ($token.Groups['header'].Success) {
            if ($arrayDepth -gt 0) { throw "Nested arrays are unsupported in $Path" }
        }
        elseif ($token.Groups['open'].Success) {
            if ($arrayDepth -gt 0) { throw "Nested arrays are unsupported in $Path" }
            $arrayDepth++
        }
        elseif ($token.Groups['close'].Success) {
            $arrayDepth--
            if ($arrayDepth -lt 0) { throw "Unbalanced array delimiters in $Path" }
        }
    }
    if ($arrayDepth -ne 0) { throw "Unterminated array in $Path" }
}

function Get-PackageVersionMatch {
    param([Parameter(Mandatory = $true)][string]$Content, [Parameter(Mandatory = $true)][string]$Path)

    Assert-SupportedManifestStructure -Content $Content -Path $Path
    $sections = [regex]::Matches($Content, '(?ms)^[ \t]*\[[ \t]*package[ \t]*\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|\z).*?(?=^[ \t]*\[|\z)')
    $firstTable = [regex]::Match($Content, '(?m)^[ \t]*\[')
    $rootContent = if ($firstTable.Success) { $Content.Substring(0, $firstTable.Index) } else { $Content }
    if (-not [string]::IsNullOrWhiteSpace($rootContent)) {
        Assert-BareSectionKeys -Content $rootContent -Path $Path
    }
    # Quoted/dotted package keys and multiline strings need a TOML parser; do
    # not let them introduce a second declaration that a line matcher misses.
    if ($sections.Count -ne 1 -or
        $rootContent -match '(?m)^[ \t]*(?:package|dependencies|dev-dependencies|build-dependencies|target)[ \t]*[.=]' -or
        $Content -match '(?m)^[ \t]*(?:\[\[?[ \t]*["'']|\[\[[ \t]*package[ \t]*\]|\[\[?[ \t]*package[ \t]*\.[ \t]*(?:version\b|["'']))') {
        throw "Expected one bare [package] table in $Path"
    }

    $section = $sections[0]
    Assert-BareSectionKeys -Content $section.Value -Path $Path
    $declarations = [regex]::Matches($section.Value, '(?m)^[ \t]*version\b')
    $versions = [regex]::Matches($section.Value, '(?m)^[ \t]*version[ \t]*=[ \t]*"(?<version>[^"\r\n]*)"[ \t]*(?:#[^\r\n]*)?\r?$')
    if ($declarations.Count -ne 1 -or $versions.Count -ne 1) {
        throw "Expected one literal package version in $Path"
    }
    $version = $versions[0].Groups['version']
    Assert-SemVer -Version $version.Value
    if ($version.Value -match '-(?:dev|nightly)\.[0-9]{8}(?:\.|$)') {
        throw "Source manifest is already stamped with a build version in $Path"
    }
    return [pscustomobject]@{
        Version = $version.Value
        Index = $section.Index + $version.Index
        Length = $version.Length
    }
}

function Set-PackageVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Content,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)]$VersionMatch
    )

    return $Content.Substring(0, $VersionMatch.Index) + $Version +
        $Content.Substring($VersionMatch.Index + $VersionMatch.Length)
}

function Set-MssqlTdsDependencyVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Content,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $quotedString = '(?:"(?:\\.|[^"\\\r\n])*"|''[^''\r\n]*'')'
    $selector = "(?:$quotedString|[A-Za-z0-9_-]+)"
    $targetPrefix = "target[ \t]*\.[ \t]*$selector[ \t]*\.[ \t]*"
    if ($Content -match '(?m)^[ \t]*\[\[[ \t]*(?:dependencies|dev-dependencies|build-dependencies|target)[ \t]*(?:\.|\])') {
        throw "Dependency and target array-of-table headers are unsupported in $Path"
    }
    # The target selector may be quoted, but the following table kind must be
    # bare so escaped spellings cannot hide a dependency table or subtable.
    if ($Content -match ('(?m)^[ \t]*\[[ \t]*' + $targetPrefix + '["'']')) {
        throw "Quoted target table kinds are unsupported in $Path"
    }
    $targetParents = [regex]::Matches($Content, "(?ms)^[ \t]*\[[ \t]*target(?:[ \t]*\.[ \t]*$selector)?[ \t]*\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|\z)(?<body>.*?)(?=^[ \t]*\[|\z)")
    foreach ($targetParent in $targetParents) {
        if ($targetParent.Groups['body'].Value -match '(?m)^[ \t]*[^# \t\r\n]') {
            throw "Target parent tables must use explicit dependency table headers in $Path"
        }
    }
    $dependencyBodies = [regex]::Matches($Content, "(?ms)^[ \t]*\[[ \t]*(?:$targetPrefix)?(?:dependencies|dev-dependencies|build-dependencies)(?:[ \t]*\.[ \t]*[A-Za-z0-9_-]+)*[ \t]*\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|\z)(?<body>.*?)(?=^[ \t]*\[|\z)")
    foreach ($dependencyBody in $dependencyBodies) {
        # Consume strings and comments whole: package-like text inside a path or
        # example must not be interpreted as the identity of a renamed crate.
        $tokens = [regex]::Matches($dependencyBody.Groups['body'].Value, "#[^\r\n]*|(?<quotedKey>$quotedString)(?=[ \t]*[=.])|(?<package>package)[ \t]*=[ \t]*(?<name>$quotedString)|$quotedString|[A-Za-z0-9_-]+")
        foreach ($token in $tokens) {
            if ($token.Groups['quotedKey'].Success) {
                throw "Quoted dependency field keys are unsupported in $Path"
            }
            if ($token.Groups['package'].Success) {
                $name = $token.Groups['name'].Value
                $name = $name.Substring(1, $name.Length - 2)
                if ($name.Contains('\')) {
                    throw "Escaped dependency package names are unsupported in $Path"
                }
                if ($name -ceq 'mssql-tds') {
                    throw "Renamed mssql-tds dependencies are unsupported in $Path"
                }
            }
        }
    }
    $sections = [regex]::Matches($Content, '(?ms)^[ \t]*\[[ \t]*dependencies[ \t]*\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|\z).*?(?=^[ \t]*\[|\z)')
    $dependencySections = [regex]::Matches($Content, "(?ms)^[ \t]*\[[ \t]*(?:$targetPrefix)?(?:dependencies|dev-dependencies|build-dependencies)[ \t]*\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|\z).*?(?=^[ \t]*\[|\z)")
    $declarationCount = 0
    foreach ($dependencySection in $dependencySections) {
        Assert-BareSectionKeys -Content $dependencySection.Value -Path $Path
        $declarationCount += [regex]::Matches($dependencySection.Value, '(?m)^[ \t]*(?:mssql-tds|["'']mssql-tds["''])[ \t]*[=.]').Count
    }
    if ($declarationCount -ne 1 -or $sections.Count -ne 1 -or
        $Content -match ('(?m)^[ \t]*\[[ \t]*(?:' + $targetPrefix + ')?(?:dependencies|dev-dependencies|build-dependencies)[ \t]*\.[ \t]*(?:["'']|mssql-tds(?:[ \t.\]]))')) {
        throw "Expected one inline mssql-tds dependency in [dependencies] in $Path"
    }

    $section = $sections[0]
    $dependencies = [regex]::Matches($section.Value, '(?m)^[ \t]*mssql-tds[ \t]*=[ \t]*\{(?<fields>(?:' + $quotedString + '|[^{}"''\r\n])*)\}[ \t]*(?:#[^\r\n]*)?\r?$')
    if ($dependencies.Count -ne 1) {
        throw "Expected one single-line mssql-tds inline dependency in $Path"
    }
    $fields = $dependencies[0].Groups['fields']
    $quote = [char]0
    $depth = 0
    $start = 0
    $versionIndex = -1
    $versionLength = 0
    $keys = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    # Only commas outside strings and arrays delimit inline-table fields.
    for ($i = 0; $i -le $fields.Length; $i++) {
        if ($i -lt $fields.Length) {
            $character = $fields.Value[$i]
            if ($quote -ne [char]0) {
                if ($quote -eq '"' -and $character -eq '\') {
                    throw "Escaped basic-string dependency values are unsupported in $Path"
                }
                if ($character -eq $quote) { $quote = [char]0 }
                continue
            }
            if ($character -eq '"' -or $character -eq "'") {
                $quote = $character
                continue
            }
            if ($character -eq '[') { $depth++; continue }
            if ($character -eq ']') {
                $depth--
                if ($depth -lt 0) { throw "Unbalanced inline dependency array in $Path" }
                continue
            }
            if ($character -ne ',' -or $depth -ne 0) { continue }
        }
        if ($quote -ne [char]0 -or $depth -ne 0) {
            throw "Unterminated inline dependency value in $Path"
        }
        $field = [regex]::Match($fields.Value.Substring($start, $i - $start), '\A[ \t]*(?<key>[A-Za-z0-9_-]+)[ \t]*=[ \t]*(?<value>.+?)[ \t]*\z')
        if (-not $field.Success -or -not $keys.Add($field.Groups['key'].Value)) {
            throw "Expected unique bare inline dependency keys in $Path"
        }
        if ($field.Groups['key'].Value -ceq 'package') {
            throw "The mssql-tds dependency key must not override its package in $Path"
        }
        if ($field.Groups['key'].Value -ceq 'version') {
            $value = $field.Groups['value']
            if ($value.Value -cnotmatch '\A"[^"]+"\z') {
                throw "Expected a double-quoted mssql-tds dependency version in $Path"
            }
            $sourceVersion = $value.Value.Substring(1, $value.Length - 2)
            Assert-SemVer -Version $sourceVersion
            $versionIndex = $start + $value.Index + 1
            $versionLength = $sourceVersion.Length
        }
        $start = $i + 1
    }
    if ($versionIndex -lt 0) {
        throw "Expected one literal mssql-tds dependency version with bare keys in $Path"
    }
    $index = $section.Index + $fields.Index + $versionIndex
    return $Content.Substring(0, $index) + $Version +
        $Content.Substring($index + $versionLength)
}

if ($BuildReason -cnotmatch '\A[A-Za-z][A-Za-z0-9]*\z') {
    throw 'BuildReason must be an ASCII alphanumeric reason beginning with a letter.'
}
if ($BuildId -cnotmatch '\A[1-9][0-9]*\z') {
    throw 'BuildId must be a positive decimal integer without leading zeroes.'
}
$isOfficialBuild = $false
if ($IsOfficial -notmatch '\A(?:True|False)\z' -or -not [bool]::TryParse($IsOfficial, [ref]$isOfficialBuild)) {
    throw 'IsOfficial must be True or False.'
}
if ([string]::IsNullOrWhiteSpace($RepositoryRoot) -or $RepositoryRoot -match '[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]|##(?:vso\[|\[)') {
    throw 'RepositoryRoot must be a nonempty path without control characters or logging commands.'
}
if ($Crates.Count -eq 0) {
    throw 'Crates must contain at least one crate directory.'
}
$seen = @{}
$manifests = foreach ($crate in $Crates) {
    if ($crate -cnotmatch '\A[A-Za-z0-9][A-Za-z0-9_-]*\z' -or $seen.ContainsKey($crate)) {
        throw 'Crates must contain unique bare crate directory names.'
    }
    $seen[$crate] = $true
    $path = Join-Path (Join-Path $RepositoryRoot $crate) 'Cargo.toml'
    $content = Get-Content -LiteralPath $path -Raw
    $versionMatch = Get-PackageVersionMatch -Content $content -Path $path
    [pscustomobject]@{ Path = $path; Crate = $crate; Content = $content; VersionMatch = $versionMatch }
}
$baseVersion = @($manifests)[0].VersionMatch.Version
$dateStamp = Get-Date -Format 'yyyyMMdd'

if ($isOfficialBuild) {
    $crateVersion = $baseVersion
}
else {
    if ($baseVersion.Contains('+')) {
        throw 'Non-official builds do not support source build metadata.'
    }
    if ($BuildReason -eq 'Schedule') {
        $crateVersion = "$baseVersion-nightly.$dateStamp"
    }
    else {
        $crateVersion = "$baseVersion-dev.$dateStamp.$BuildId"
    }
}
Assert-SemVer -Version $crateVersion

$edits = foreach ($manifest in $manifests) {
    $updated = Set-PackageVersion -Content $manifest.Content -Version $crateVersion -VersionMatch $manifest.VersionMatch
    if ($manifest.Crate -eq 'mssql-mock-tds') {
        $updated = Set-MssqlTdsDependencyVersion -Content $updated -Version $crateVersion -Path $manifest.Path
    }
    [pscustomobject]@{ Path = $manifest.Path; Content = $manifest.Content; Updated = $updated }
}

Write-Host "Build reason:  $BuildReason"
Write-Host "Base version:  $baseVersion"
Write-Host "Crate version: $crateVersion"

foreach ($edit in $edits) {
    $path = $edit.Path
    if ($WhatIf) {
        Write-Host "[WhatIf] Would patch $path -> version = `"$crateVersion`""
    }
    elseif ($edit.Content -ne $edit.Updated) {
        Set-Content -LiteralPath $path -Value $edit.Updated -NoNewline
        Write-Host "Patched $path -> version = `"$crateVersion`""
    }
    else {
        Write-Host "$path already uses version `"$crateVersion`""
    }
}

if (-not $WhatIf) {
    Write-Host "##vso[task.setvariable variable=crateVersion]$crateVersion"
    Write-Host "##vso[task.setvariable variable=crateVersion;isOutput=true]$crateVersion"
}

Write-Host "Done. Crate version: $crateVersion"
