# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Configure (and revert) SQL Server Extended Protection for Authentication (EPA).
#
# Extended Protection binds the integrated-auth (NTLM/Kerberos) exchange to the
# TLS channel (tls-unique channel binding, RFC 5929) to defeat authentication
# relay / TLS MITM. The setting lives in the instance's SuperSocketNetLib
# registry hive and is read by the SQL Server network layer at service startup,
# so a service RESTART is required for changes to take effect -- both to set it
# and to revert it.
#
# This mirrors the registry-write + Restart-Service pattern already used by
# Enable-SqlProtocols.ps1 and Generate-SqlCertificates.ps1. Written to be
# Windows PowerShell 5.1 compatible (invoked via PowerShell@2).
#
# Examples:
#   # Enforce EPA + Force Encryption (channel binding requires encryption):
#   .\Configure-ExtendedProtection.ps1 -Level Required -ForceEncryption Yes
#
#   # Revert after the test:
#   .\Configure-ExtendedProtection.ps1 -Level Off -ForceEncryption No

param(
    [string]$InstanceName = "MSSQLSERVER",

    [ValidateSet("Off", "Allowed", "Required")]
    [string]$Level = "Required",

    # Yes/No sets ForceEncryption; Unchanged leaves it as-is.
    [ValidateSet("Yes", "No", "Unchanged")]
    [string]$ForceEncryption = "Unchanged",

    # Optional NTLM service-binding SPN allow-list (semicolon-joined in registry).
    [string[]]$AcceptedNtlmSpns = @(),

    [bool]$RestartService = $true
)

$ErrorActionPreference = 'Stop'

$levelMap = @{ "Off" = 0; "Allowed" = 1; "Required" = 2 }

Write-Host "=== Configure SQL Server Extended Protection ===" -ForegroundColor Cyan
Write-Host "Instance:        $InstanceName" -ForegroundColor Yellow
Write-Host "ExtendedProt.:   $Level ($($levelMap[$Level]))" -ForegroundColor Yellow
if ($ForceEncryption -ne "Unchanged") {
    Write-Host "ForceEncryption: $ForceEncryption" -ForegroundColor Yellow
}

# --- Resolve the versioned instance registry key (e.g. MSSQL16.MSSQLSERVER) ---
$sqlRoot = "HKLM:\SOFTWARE\Microsoft\Microsoft SQL Server"
$versionKey = $null
foreach ($key in (Get-ChildItem $sqlRoot -ErrorAction SilentlyContinue)) {
    if ($key.PSChildName -match "MSSQL(\d+)\.$([regex]::Escape($InstanceName))$") {
        $versionKey = $key.PSChildName
        break
    }
}
if (-not $versionKey) {
    throw "Could not locate registry key for SQL instance '$InstanceName' under $sqlRoot"
}

$superSocket = "$sqlRoot\$versionKey\MSSQLServer\SuperSocketNetLib"
Write-Host "Registry hive:   $superSocket" -ForegroundColor White

if ($InstanceName -eq "MSSQLSERVER") {
    $serviceName = "MSSQLSERVER"
} else {
    $serviceName = "MSSQL`$$InstanceName"
}

# --- Capture the previous EP value (handy for logging / manual revert) ---
$previous = (Get-ItemProperty -Path $superSocket -Name "ExtendedProtection" -ErrorAction SilentlyContinue).ExtendedProtection
if ($null -eq $previous) { $prevDisplay = "<unset>" } else { $prevDisplay = "$previous" }
Write-Host "Previous ExtendedProtection: $prevDisplay" -ForegroundColor White

# --- Apply Extended Protection level ---
New-ItemProperty -Path $superSocket -Name "ExtendedProtection" -PropertyType DWord -Value $levelMap[$Level] -Force | Out-Null
Write-Host "Set ExtendedProtection = $($levelMap[$Level]) ($Level)" -ForegroundColor Green

# --- Optionally apply Force Encryption (channel binding is only enforced on
#     encrypted connections; Force Encryption guarantees all clients encrypt) ---
if ($ForceEncryption -ne "Unchanged") {
    if ($ForceEncryption -eq "Yes") { $fe = 1 } else { $fe = 0 }
    New-ItemProperty -Path $superSocket -Name "ForceEncryption" -PropertyType DWord -Value $fe -Force | Out-Null
    Write-Host "Set ForceEncryption = $fe" -ForegroundColor Green
}

# --- Optionally configure Accepted NTLM SPNs (service binding for NTLM) ---
if ($AcceptedNtlmSpns.Count -gt 0) {
    $joined = ($AcceptedNtlmSpns -join ';')
    New-ItemProperty -Path $superSocket -Name "AcceptedNTLMSPNs" -PropertyType String -Value $joined -Force | Out-Null
    Write-Host "Set AcceptedNTLMSPNs = $joined" -ForegroundColor Green
} elseif ($Level -eq "Off") {
    # Clean up any allow-list we may have written on a prior 'Required' pass.
    Remove-ItemProperty -Path $superSocket -Name "AcceptedNTLMSPNs" -ErrorAction SilentlyContinue
}

# --- Restart so the network layer re-reads the settings ---
if ($RestartService) {
    Write-Host "Restarting service '$serviceName' so settings take effect..." -ForegroundColor Yellow
    Restart-Service -Name $serviceName -Force

    $svc = Get-Service -Name $serviceName
    $timeout = 30; $elapsed = 0
    while ($svc.Status -ne 'Running' -and $elapsed -lt $timeout) {
        Start-Sleep -Seconds 1
        $elapsed++
        $svc = Get-Service -Name $serviceName
    }
    if ($svc.Status -ne 'Running') {
        throw "Service '$serviceName' did not return to Running after restart (status: $($svc.Status))"
    }
    Write-Host "Service '$serviceName' is running." -ForegroundColor Green
} else {
    Write-Host "RestartService=`$false - change will NOT take effect until '$serviceName' is restarted." -ForegroundColor Yellow
}

Write-Host "=== Extended Protection configuration complete ===" -ForegroundColor Cyan
