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

$name = 'ODBC Driver 18 for SQL Server'
$key  = "HKLM:\Software\ODBC\ODBCINST.INI\$name"

if (Test-Path $key) {
    Write-Host "$name is already registered; skipping install."
    Get-ItemProperty -Path $key | Format-List | Out-String | Write-Host
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
