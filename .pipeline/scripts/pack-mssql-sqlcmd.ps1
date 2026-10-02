<#
.SYNOPSIS
    Assemble the mssql-sqlcmd NuGet package contents and write its .nuspec.

.DESCRIPTION
    Collects the per-target static libraries staged by build-mssql-sqlcmd-native.*
    from ArtifactsDirectory (searched recursively, so it can be the root of
    downloaded pipeline artifacts), checks every required runtime is present,
    and lays out:

        <StagingDirectory>/include/mssql_sqlcmd.h
        <StagingDirectory>/runtimes/<rid>/native/<static library>
        <StagingDirectory>/runtimes/<rid>/native/native-static-libs.txt
        <StagingDirectory>/README.md
        <StagingDirectory>/LICENSE.txt
        <StagingDirectory>/mssql-sqlcmd.nuspec

    The package version is the crate version from mssql-sqlcmd/Cargo.toml. An
    official build produces it as is, since consumers pin an exact version;
    other builds get a -nightly or -dev prerelease suffix. The .nuspec path and
    the version are written to the pipeline variables mssqlSqlcmdNuspec and
    mssqlSqlcmdVersion.

.EXAMPLE
    .pipeline/scripts/pack-mssql-sqlcmd.ps1 -ArtifactsDirectory $(Pipeline.Workspace)/officialBuild `
        -StagingDirectory $(Build.StagingDirectory)/mssql-sqlcmd -IsOfficial True
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $ArtifactsDirectory,

    [Parameter(Mandatory = $true)]
    [string] $StagingDirectory,

    [string] $BuildReason = 'Manual',
    [string] $BuildId = '0',
    [string] $SourceVersion = '',
    [string] $IsOfficial = 'False',

    [string[]] $RequiredRids = @(
        'win-x64', 'win-x86', 'win-arm64',
        'linux-x64', 'linux-arm64', 'linux-musl-x64', 'linux-musl-arm64',
        'osx-x64', 'osx-arm64'
    )
)

$ErrorActionPreference = 'Stop'
# `pwsh -File` passes a comma-separated list as one string.
$RequiredRids = @($RequiredRids | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$crateDir = Join-Path $repoRoot 'mssql-sqlcmd'

# Version: the crate's, with a prerelease suffix unless this is an official build.
$cargoToml = Get-Content (Join-Path $crateDir 'Cargo.toml') -Raw
$packageSection = [regex]::Match($cargoToml, '(?ms)^\[package\]\s*$(?<body>.*?)(?=^\[|\z)')
$versionMatch = [regex]::Match($packageSection.Groups['body'].Value, '(?m)^\s*version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) { throw 'Could not read [package].version from mssql-sqlcmd/Cargo.toml' }
$crateVersion = $versionMatch.Groups[1].Value

$dateStamp = Get-Date -Format 'yyyyMMdd'
$isOfficialBuild = $IsOfficial -eq 'True'
# Consumers pin an exact version, so an official build always produces the crate
# version itself; a new release needs a version bump in Cargo.toml.
$packageVersion = if ($isOfficialBuild) {
    $crateVersion
} elseif ($BuildReason -eq 'Schedule') {
    "$crateVersion-nightly.$dateStamp"
} else {
    "$crateVersion-dev.$dateStamp.$BuildId"
}

# Fresh staging tree.
if (Test-Path $StagingDirectory) { Remove-Item $StagingDirectory -Recurse -Force }
New-Item -ItemType Directory -Force -Path $StagingDirectory | Out-Null

# Collect runtimes/<rid>/native from every artifact.
$runtimeDirs = Get-ChildItem -Path $ArtifactsDirectory -Recurse -Directory -Filter native |
    Where-Object { $_.Parent -and $_.Parent.Parent -and $_.Parent.Parent.Name -eq 'runtimes' }
foreach ($dir in $runtimeDirs) {
    $rid = $dir.Parent.Name
    $dest = Join-Path $StagingDirectory "runtimes\$rid\native"
    if (Test-Path $dest) { throw "Runtime $rid was staged by more than one artifact" }
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Copy-Item (Join-Path $dir.FullName '*') $dest -Force
}

# Every required runtime must carry its library and its link flags.
$missing = @()
foreach ($rid in $RequiredRids) {
    $native = Join-Path $StagingDirectory "runtimes\$rid\native"
    $library = if ($rid -like 'win-*') { 'mssql_sqlcmd.lib' } else { 'libmssql_sqlcmd.a' }
    foreach ($file in @($library, 'native-static-libs.txt')) {
        if (-not (Test-Path (Join-Path $native $file))) { $missing += "$rid/$file" }
    }
}
if ($missing) { throw "Missing from the build artifacts: $($missing -join ', ')" }

New-Item -ItemType Directory -Force -Path (Join-Path $StagingDirectory 'include') | Out-Null
Copy-Item (Join-Path $crateDir 'include\mssql_sqlcmd.h') (Join-Path $StagingDirectory 'include\mssql_sqlcmd.h')
Copy-Item (Join-Path $crateDir 'README.md') (Join-Path $StagingDirectory 'README.md')
Copy-Item (Join-Path $repoRoot 'LICENSE') (Join-Path $StagingDirectory 'LICENSE.txt')

$commit = if ($SourceVersion) { "<repository type=`"git`" url=`"https://github.com/microsoft/mssql-rs`" commit=`"$SourceVersion`" />" } else { '<repository type="git" url="https://github.com/microsoft/mssql-rs" />' }
$nuspec = @"
<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>mssql-sqlcmd</id>
    <version>$packageVersion</version>
    <authors>Microsoft</authors>
    <license type="file">LICENSE.txt</license>
    <readme>README.md</readme>
    <requireLicenseAcceptance>false</requireLicenseAcceptance>
    <description>Static libraries of the mssql-sqlcmd Rust crate, which native sqlcmd links through a C ABI (include/mssql_sqlcmd.h). One library per runtime under runtimes/&lt;rid&gt;/native, with the system libraries it needs in native-static-libs.txt.</description>
    $commit
  </metadata>
  <files>
    <file src="include\**" target="include" />
    <file src="runtimes\**" target="runtimes" />
    <file src="README.md" target="" />
    <file src="LICENSE.txt" target="" />
  </files>
</package>
"@
$nuspecPath = Join-Path $StagingDirectory 'mssql-sqlcmd.nuspec'
Set-Content -Path $nuspecPath -Value $nuspec -Encoding utf8

Write-Host "mssql-sqlcmd $packageVersion staged at $StagingDirectory"
Get-ChildItem (Join-Path $StagingDirectory 'runtimes') -Directory | ForEach-Object {
    $native = Join-Path $_.FullName 'native'
    $libs = (Get-Content (Join-Path $native 'native-static-libs.txt') -Raw).Trim()
    Write-Host ("  {0,-18} {1}" -f $_.Name, $libs)
}

Write-Host "##vso[task.setvariable variable=mssqlSqlcmdNuspec]$nuspecPath"
Write-Host "##vso[task.setvariable variable=mssqlSqlcmdVersion]$packageVersion"
