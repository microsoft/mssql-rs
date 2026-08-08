# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Installs the Microsoft ODBC Driver 18 for SQL Server (msodbcsql18) as the
# parity reference driver for the ODBC C++ e2e comparison run on Windows. It
# registers as "ODBC Driver 18 for SQL Server"; the Rust driver registers
# separately as "... (Rust)", so the two coexist and run_e2e.ps1 selects between
# them per leg.
#
# winget is the intended path, but it is not present on every Windows Server
# image, so fall back to the MSI winget itself would download. Skipping the
# install is deliberately not an option: it would silently downgrade the job to a
# single-leg run, so every failure path throws.
#
# The MSI URL and hash are pinned to the -Version default. Bumping -Version (via
# the msodbcsqlVersion pipeline variable) without also updating -MsiUrl/-MsiSha256
# leaves the winget path working while the MSI fallback fails loudly on a hash
# mismatch rather than installing an unverified binary.
[CmdletBinding()]
param(
    [string]$Version   = '18.6.2.1',
    [string]$MsiUrl    = 'https://download.microsoft.com/download/7bf9fad4-0f21-486d-a750-fc990ded5624/amd64/1033/msodbcsql.msi',
    [string]$MsiSha256 = '20314529110da3365a252164a657bdc837a18be5839105aa5f5acf0a8d2f4b82'
)

$ErrorActionPreference = 'Stop'

# Collapse zero-padded version segments (e.g. 18.06.0002.0001 -> 18.6.2.1) so a
# DLL's ProductVersion can be compared to the semantic -Version regardless of
# padding or ',' vs '.' separators.
function Normalize-Version([string]$v) {
    if (-not $v) { return '' }
    (($v -split '[.,]') | ForEach-Object {
        $n = 0
        if ([int]::TryParse($_.Trim(), [ref]$n)) { $n } else { $_.Trim() }
    }) -join '.'
}

$name = 'ODBC Driver 18 for SQL Server'
$key  = "HKLM:\Software\ODBC\ODBCINST.INI\$name"

if (Test-Path $key) {
    Write-Host "$name is already registered; verifying its version instead of installing."
    Get-ItemProperty -Path $key | Format-List | Out-String | Write-Host

    # A pre-installed driver on a self-hosted/persistent agent could be a version
    # other than the pinned one, which would silently change what the parity table
    # compares against. Resolve the registered DLL and fail loudly on a mismatch.
    $dll = (Get-ItemProperty -Path $key -Name 'Driver' -ErrorAction SilentlyContinue).Driver
    if ($dll -and (Test-Path $dll)) {
        $info = (Get-Item $dll).VersionInfo
        Write-Host "Registered '$name' -> $dll (FileVersion=$($info.FileVersion), ProductVersion=$($info.ProductVersion))"

        $want = Normalize-Version $Version
        $have = Normalize-Version $info.ProductVersion
        if (-not $have) {
            Write-Warning "Could not read a ProductVersion from $dll; skipping the version check."
        } elseif ($have -ne $want) {
            throw "Registered '$name' is version $($info.ProductVersion) (normalized $have) but the pipeline pins $Version (normalized $want). Uninstall the pre-installed driver, or update the msodbcsqlVersion variable and the pinned MSI in this script."
        } else {
            Write-Host "Version check passed: registered driver matches the pinned $Version."
        }
    } else {
        Write-Warning "'$name' is registered but its Driver DLL path could not be resolved; skipping the version check."
    }
    exit 0
}

if (Get-Command winget -ErrorAction SilentlyContinue) {
    Write-Host "Installing msodbcsql $Version via winget..."
    winget install --id Microsoft.msodbcsql.18 --version $Version --exact `
        --silent --accept-package-agreements --accept-source-agreements `
        --disable-interactivity
    if ($LASTEXITCODE -ne 0) {
        throw "winget install of msodbcsql $Version failed (exit $LASTEXITCODE)"
    }
} else {
    $msi = Join-Path $env:TEMP 'msodbcsql.msi'
    Write-Host "winget not available; downloading msodbcsql $Version MSI..."
    Invoke-WebRequest -Uri $MsiUrl -OutFile $msi -UseBasicParsing
    $actual = (Get-FileHash $msi -Algorithm SHA256).Hash
    if ($actual -ne $MsiSha256) {
        throw "msodbcsql MSI hash mismatch: expected $MsiSha256, got $actual"
    }
    $p = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @(
        '/i', "`"$msi`"", '/qn', '/norestart', 'IACCEPTMSODBCSQLLICENSETERMS=YES'
    )
    if ($p.ExitCode -ne 0) {
        throw "msiexec install of msodbcsql $Version failed (exit $($p.ExitCode))"
    }
}

if (-not (Test-Path $key)) {
    throw "Install reported success but '$name' is not registered under HKLM."
}
Write-Host "Installed and registered '$name'."
