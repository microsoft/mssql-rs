param(
    [Parameter(Mandatory = $true)]
    [string]$WheelsDir,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedName,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedVersion,

    [Parameter(Mandatory = $true)]
    [int]$ExpectedCount,

    [switch]$RequireOdbc
)

$ErrorActionPreference = 'Stop'

function ConvertTo-CanonicalName {
    param([string]$Name)
    return [regex]::Replace($Name.ToLowerInvariant(), '[-_.]+', '-')
}

function Get-ExpectedOdbcMembers {
    param([string]$WheelName)

    switch -Wildcard ($WheelName) {
        '*-win_amd64.whl' {
            return @('mssql_py_core/libs/windows/x64/mssqlodbc.dll')
        }
        '*-win_arm64.whl' {
            return @('mssql_py_core/libs/windows/arm64/mssqlodbc.dll')
        }
        '*-musllinux_*_x86_64.whl' {
            return @('mssql_py_core/libs/linux/musl/x86_64/lib/mssqlodbc.so')
        }
        '*-musllinux_*_aarch64.whl' {
            return @('mssql_py_core/libs/linux/musl/arm64/lib/mssqlodbc.so')
        }
        '*-macosx_*_universal2.whl' {
            return @(
                'mssql_py_core/libs/macos/x86_64/lib/mssqlodbc.dylib',
                'mssql_py_core/libs/macos/arm64/lib/mssqlodbc.dylib'
            )
        }
        '*_x86_64.whl' {
            return @('mssql_py_core/libs/linux/glibc/x86_64/lib/mssqlodbc.so')
        }
        '*_aarch64.whl' {
            return @('mssql_py_core/libs/linux/glibc/arm64/lib/mssqlodbc.so')
        }
        default {
            throw "Unsupported wheel platform: $WheelName"
        }
    }
}

$wheels = @(Get-ChildItem -Path $WheelsDir -Filter '*.whl' -File)
if ($wheels.Count -ne $ExpectedCount) {
    throw "Expected $ExpectedCount wheels, found $($wheels.Count)"
}

$canonicalExpectedName = ConvertTo-CanonicalName $ExpectedName
$filenamePrefix = $ExpectedName.Replace('-', '_')
$expectedFilenamePrefix = "$filenamePrefix-$ExpectedVersion-"

foreach ($wheel in $wheels) {
    if (-not $wheel.Name.StartsWith($expectedFilenamePrefix)) {
        throw "$($wheel.Name): expected filename prefix $expectedFilenamePrefix"
    }

    $archive = [System.IO.Compression.ZipFile]::OpenRead($wheel.FullName)
    try {
        $members = @($archive.Entries | ForEach-Object { $_.FullName })
        $metadataEntries = @(
            $archive.Entries | Where-Object { $_.FullName.EndsWith('.dist-info/METADATA') }
        )
        if ($metadataEntries.Count -ne 1) {
            throw "$($wheel.Name): expected one METADATA file, found $($metadataEntries.Count)"
        }

        $reader = [System.IO.StreamReader]::new($metadataEntries[0].Open())
        try {
            $metadata = $reader.ReadToEnd()
        }
        finally {
            $reader.Dispose()
        }

        $nameMatch = [regex]::Match($metadata, '(?m)^Name:\s*(.+?)\r?$')
        $versionMatch = [regex]::Match($metadata, '(?m)^Version:\s*(.+?)\r?$')
        if (-not $nameMatch.Success) {
            throw "$($wheel.Name): METADATA has no Name field"
        }
        if (-not $versionMatch.Success) {
            throw "$($wheel.Name): METADATA has no Version field"
        }

        $actualName = ConvertTo-CanonicalName $nameMatch.Groups[1].Value
        if ($actualName -ne $canonicalExpectedName) {
            throw "$($wheel.Name): metadata Name is '$($nameMatch.Groups[1].Value)', expected '$ExpectedName'"
        }
        if ($versionMatch.Groups[1].Value -ne $ExpectedVersion) {
            throw "$($wheel.Name): metadata Version is '$($versionMatch.Groups[1].Value)', expected '$ExpectedVersion'"
        }

        if ($RequireOdbc) {
            foreach ($expectedMember in Get-ExpectedOdbcMembers $wheel.Name) {
                if ($members -notcontains $expectedMember) {
                    throw "$($wheel.Name): missing ODBC driver: $expectedMember"
                }
            }
        }
    }
    finally {
        $archive.Dispose()
    }
}

Write-Host "Validated $($wheels.Count) $ExpectedName wheels"