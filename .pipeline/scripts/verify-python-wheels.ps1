param(
    [Parameter(Mandatory = $true)]
    [string]$WheelsDir,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedName,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedVersion,

    [switch]$RequireOdbc
)

$ErrorActionPreference = 'Stop'

function ConvertTo-CanonicalName {
    param([string]$Name)
    return [regex]::Replace($Name.ToLowerInvariant(), '[-_.]+', '-')
}

function Assert-PyPICompatiblePlatformTag {
    param([string]$WheelName)

    $platformTag = [System.IO.Path]::GetFileNameWithoutExtension($WheelName).Split('-')[-1]
    $accepted = @(
        '^win_amd64$',
        '^win_arm64$',
        '^macosx_\d+_\d+_universal2$',
        '^manylinux_\d+_\d+_(x86_64|aarch64)$',
        '^musllinux_\d+_\d+_(x86_64|aarch64)$'
    )
    foreach ($pattern in $accepted) {
        if ($platformTag -match $pattern) {
            return
        }
    }
    throw "$WheelName has platform tag '$platformTag' which PyPI rejects. Linux wheels must carry a manylinux or musllinux tag, not a bare linux tag."
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

function Get-ExpectedWheelNames {
    param([string]$Prefix, [string]$Version)

    $pythonTags = 'cp310', 'cp311', 'cp312', 'cp313', 'cp314'
    $platforms = @(
        'win_amd64',
        'manylinux_2_34_x86_64',
        'manylinux_2_34_aarch64',
        'manylinux_2_28_x86_64',
        'manylinux_2_28_aarch64',
        'musllinux_1_2_x86_64',
        'musllinux_1_2_aarch64',
        'macosx_15_0_universal2'
    )

    $expected = foreach ($pythonTag in $pythonTags) {
        foreach ($platform in $platforms) {
            "$Prefix-$Version-$pythonTag-$pythonTag-$platform.whl"
        }
    }
    foreach ($pythonTag in $pythonTags | Where-Object { $_ -ne 'cp310' }) {
        "$Prefix-$Version-$pythonTag-$pythonTag-win_arm64.whl"
    }
    return @($expected)
}

$canonicalExpectedName = ConvertTo-CanonicalName $ExpectedName
$filenamePrefix = $ExpectedName.Replace('-', '_')
$expectedFilenamePrefix = "$filenamePrefix-$ExpectedVersion-"
$expectedWheelNames = @(Get-ExpectedWheelNames $filenamePrefix $ExpectedVersion)
$wheels = @(Get-ChildItem -Path $WheelsDir -Filter '*.whl' -File)
foreach ($wheel in $wheels) {
    Assert-PyPICompatiblePlatformTag $wheel.Name
}
if ($wheels.Count -ne $expectedWheelNames.Count) {
    throw "Expected $($expectedWheelNames.Count) wheels, found $($wheels.Count)"
}
$actualWheelNames = @($wheels | ForEach-Object { $_.Name })
$missingWheels = @($expectedWheelNames | Where-Object { $actualWheelNames -cnotcontains $_ })
$unexpectedWheels = @($actualWheelNames | Where-Object { $expectedWheelNames -cnotcontains $_ })
if ($missingWheels.Count -gt 0 -or $unexpectedWheels.Count -gt 0) {
    throw "Wheel matrix mismatch. Missing: $($missingWheels -join ', '); Unexpected: $($unexpectedWheels -join ', ')"
}

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
        $requiresPythonMatch = [regex]::Match($metadata, '(?m)^Requires-Python:\s*(.+?)\r?$')
        if (-not $nameMatch.Success) {
            throw "$($wheel.Name): METADATA has no Name field"
        }
        if (-not $versionMatch.Success) {
            throw "$($wheel.Name): METADATA has no Version field"
        }
        if (-not $requiresPythonMatch.Success) {
            throw "$($wheel.Name): METADATA has no Requires-Python field"
        }

        $actualName = ConvertTo-CanonicalName $nameMatch.Groups[1].Value
        if ($actualName -ne $canonicalExpectedName) {
            throw "$($wheel.Name): metadata Name is '$($nameMatch.Groups[1].Value)', expected '$ExpectedName'"
        }
        if ($versionMatch.Groups[1].Value -ne $ExpectedVersion) {
            throw "$($wheel.Name): metadata Version is '$($versionMatch.Groups[1].Value)', expected '$ExpectedVersion'"
        }
        if ($requiresPythonMatch.Groups[1].Value -ne '>=3.10') {
            throw "$($wheel.Name): metadata Requires-Python is '$($requiresPythonMatch.Groups[1].Value)', expected '>=3.10'"
        }

        if ($RequireOdbc) {
            foreach ($expectedMember in Get-ExpectedOdbcMembers $wheel.Name) {
                if ($members -cnotcontains $expectedMember) {
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