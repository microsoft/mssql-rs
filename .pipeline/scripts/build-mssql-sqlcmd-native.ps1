<#
.SYNOPSIS
    Build the mssql-sqlcmd static library for the Windows targets and stage it in
    the layout of the mssql-sqlcmd NuGet package (see mssql-sqlcmd/README.md).

.DESCRIPTION
    For each target this produces:

        <OutputDirectory>/runtimes/<rid>/native/mssql_sqlcmd.lib
        <OutputDirectory>/runtimes/<rid>/native/native-static-libs.txt

    and, once, <OutputDirectory>/mssql-sqlcmd-version.txt: the crate version, as
    cargo metadata reports it, which the package is versioned from.

    native-static-libs.txt holds the system libraries the archive needs, exactly
    as rustc reports them. A static library needs no linker, so every target is
    built from one x64 agent with only its Rust standard library added.

.EXAMPLE
    .pipeline/scripts/build-mssql-sqlcmd-native.ps1 -OutputDirectory $env:TEMP\sqlcmd-native
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $OutputDirectory,

    # Rust target => NuGet runtime identifier.
    [hashtable] $Targets = [ordered]@{
        'x86_64-pc-windows-msvc'  = 'win-x64'
        'i686-pc-windows-msvc'    = 'win-x86'
        'aarch64-pc-windows-msvc' = 'win-arm64'
    }
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

Push-Location $repoRoot
try {
    # The crate version, for the package (pack-mssql-sqlcmd.ps1). The packaging
    # job has no Rust toolchain, so it is recorded here, where cargo runs.
    $metadata = & cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    # Cargo's resolved target directory honors CARGO_TARGET_DIR and [build] target-dir.
    $targetDir = $metadata.target_directory
    if (-not $targetDir) { throw 'cargo metadata reports no target_directory' }
    $crate = $metadata.packages | Where-Object name -eq 'mssql-sqlcmd'
    if (-not $crate) { throw 'cargo metadata lists no mssql-sqlcmd package' }
    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
    Set-Content -Path (Join-Path $OutputDirectory 'mssql-sqlcmd-version.txt') -Value $crate.version -Encoding ascii

    foreach ($target in $Targets.Keys) {
        $rid = $Targets[$target]

        & rustup target add $target | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "rustup target add $target failed" }

        $log = New-TemporaryFile
        try {
            & cargo rustc -p mssql-sqlcmd --release --lib --target $target `
                --crate-type staticlib -- --print native-static-libs 2> $log.FullName
            if ($LASTEXITCODE -ne 0) {
                Get-Content $log.FullName | Write-Host
                throw "cargo rustc failed for $target"
            }

            $note = Select-String -Path $log.FullName -Pattern '^note: native-static-libs: (.*)$' |
                Select-Object -Last 1
            if (-not $note) {
                Get-Content $log.FullName | Write-Host
                throw "rustc printed no native-static-libs line for $target"
            }
            $nativeLibs = $note.Matches[0].Groups[1].Value.Trim()
        }
        finally {
            Remove-Item $log.FullName -ErrorAction SilentlyContinue
        }

        $dest = Join-Path $OutputDirectory "runtimes\$rid\native"
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        Copy-Item (Join-Path $targetDir "$target\release\mssql_sqlcmd.lib") (Join-Path $dest 'mssql_sqlcmd.lib') -Force
        Set-Content -Path (Join-Path $dest 'native-static-libs.txt') -Value $nativeLibs -Encoding ascii

        $size = (Get-Item (Join-Path $dest 'mssql_sqlcmd.lib')).Length
        Write-Host "Staged $rid ($target): $size bytes; native libs: $nativeLibs"
    }
}
finally {
    Pop-Location
}
