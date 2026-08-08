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
# single-leg run, so every install failure path throws. A driver already
# registered at a different version is warned about and then upgraded to the
# pinned version, and the post-install check throws if the upgrade did not take.
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
    Write-Host "$name is already registered; checking its version before deciding whether to install."
    Get-ItemProperty -Path $key | Format-List | Out-String | Write-Host

    # A pre-installed driver on a hosted/persistent agent (the Windows 1ES image
    # ships 18.5.2.1) may be a version other than the pinned one, which changes
    # what the parity table compares against. Resolve the registered DLL: if it
    # already matches the pin, skip the install; otherwise warn and fall through to
    # upgrade it to the pinned version so the comparison always runs against a known
    # driver.
    $dll = (Get-ItemProperty -Path $key -Name 'Driver' -ErrorAction SilentlyContinue).Driver
    if ($dll -and (Test-Path $dll)) {
        $info = (Get-Item $dll).VersionInfo
        Write-Host "Registered '$name' -> $dll (FileVersion=$($info.FileVersion), ProductVersion=$($info.ProductVersion))"

        $want = Normalize-Version $Version
        $have = Normalize-Version $info.ProductVersion
        if ($have -and $have -eq $want) {
            Write-Host "Version check passed: registered driver matches the pinned $Version; skipping install."
            exit 0
        } elseif ($have) {
            Write-Warning "Registered '$name' is version $($info.ProductVersion) (normalized $have) but the pipeline pins $Version (normalized $want); upgrading the pre-installed driver to the pinned version."
        } else {
            Write-Warning "Could not read a ProductVersion from $dll; reinstalling the pinned $Version to be safe."
        }
    } else {
        Write-Warning "'$name' is registered but its Driver DLL path could not be resolved; reinstalling the pinned $Version."
    }
}

if (Get-Command winget -ErrorAction SilentlyContinue) {
    Write-Host "Installing msodbcsql $Version via winget..."
    # --force so winget upgrades/reinstalls even when a different version is already
    # present, rather than reporting "no applicable update" and leaving it in place.
    winget install --id Microsoft.msodbcsql.18 --version $Version --exact --force `
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
    # A same-UpgradeCode MSI performs a major upgrade over any pre-installed 18.x.
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

# Confirm the install/upgrade actually produced the pinned version. Now that we
# fall through and attempt an upgrade, a persistent mismatch is a real failure
# (the upgrade did not take) rather than an uncontrollable pre-existing driver.
$dll = (Get-ItemProperty -Path $key -Name 'Driver' -ErrorAction SilentlyContinue).Driver
if ($dll -and (Test-Path $dll)) {
    $info = (Get-Item $dll).VersionInfo
    $want = Normalize-Version $Version
    $have = Normalize-Version $info.ProductVersion
    Write-Host "Post-install '$name' -> $dll (ProductVersion=$($info.ProductVersion))"
    if ($have -and $have -ne $want) {
        throw "After install/upgrade, '$name' is version $($info.ProductVersion) (normalized $have) but the pipeline pins $Version (normalized $want); the upgrade did not take effect."
    }
}
Write-Host "Installed and registered '$name' ($Version)."
