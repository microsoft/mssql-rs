# SQL Server Configuration Scripts

This directory contains scripts used by the Azure DevOps pipeline to configure and host SQL Server instances for tests.

## sql-host/ — On-demand SQL Server for ARM test stages

Boots a SQL Server docker container on an x64 1ES agent so ARM test jobs can
run against a real SQL Server without depending on a static (ACI) IP address.
The SQL host job and the ARM test jobs run concurrently and rendezvous through
pipeline-artifact sentinels; the SA password is derived deterministically from
the build context so no secret is transported between jobs.

- **start.sh** — Receives the SA password (derived by `derive-sql-password.sh`
  in the YAML templates), generates certs (adding the host's private VNet IPv4
  as an extra SAN via `EXTRA_IP_SAN`), starts the SQL container, and publishes
  the `sql-ready-<instanceId>` sentinel carrying the endpoint plus
  `ca.crt`/`mssql.pem`.
- **wait-for-teardown.sh** — Polls for the teardown sentinel artifacts (named
  by the raw sentinel names the test jobs publish) and releases the SQL host
  once every expected sentinel has been published.
- **teardown.sh** — Stops and removes the SQL container and network.

See `.pipeline/docs/arm-sql-host-design.md` for the full design.

## Scripts

### Generate-SqlCertificates.ps1
Generates and installs self-signed certificates for SQL Server TLS encryption.

**Parameters:**
- `InstanceName` (optional): SQL Server instance name (default: "MSSQLSERVER")

**Usage:**
```powershell
.\Generate-SqlCertificates.ps1
.\Generate-SqlCertificates.ps1 -InstanceName "SQLDEV"
```

**What it does:**
- Creates a self-signed SSL certificate for SQL Server
- Configures certificate permissions for the SQL service account
- Installs the certificate in the SQL Server registry configuration
- Copies the certificate to the trusted root store
- Restarts SQL Server service to apply changes

### Enable-SqlBrowser.ps1
Enables and starts the SQL Server Browser service.

**Parameters:**
- `ServiceName` (optional): Name of the SQL Browser service (default: "SQLBrowser")

**Usage:**
```powershell
.\Enable-SqlBrowser.ps1
.\Enable-SqlBrowser.ps1 -ServiceName "SQLBrowser"
```

**What it does:**
- Checks if SQL Browser service exists
- Sets startup type to Automatic
- Starts the service if not running
- Provides detailed status information

### Enable-SqlProtocols.ps1
Enables Named Pipes and Shared Memory protocols for SQL Server via registry modification.

**Parameters:**
- `InstanceName` (optional): SQL Server instance name (default: "MSSQLSERVER")
- `SqlVersion` (optional): SQL Server version prefix (default: "MSSQL17")
- `RestartService` (optional): Whether to restart SQL Server service (default: $true)

**Usage:**
```powershell
.\Enable-SqlProtocols.ps1
.\Enable-SqlProtocols.ps1 -InstanceName "SQLDEV" -SqlVersion "MSSQL17" -RestartService $true
```

**What it does:**
- Enables Named Pipes protocol via registry
- Enables Shared Memory protocol via registry
- Optionally restarts SQL Server service to apply changes
- Provides detailed configuration status

## Pipeline Integration

These scripts are referenced in the Azure DevOps pipeline template:

```yaml
- task: PowerShell@2
  displayName: 'Generate Certificate for TLS encryption'
  inputs:
    targetType: 'filePath'
    filePath: '.pipeline/scripts/Generate-SqlCertificates.ps1'
    arguments: '-InstanceName "MSSQLSERVER"'

- task: PowerShell@2
  displayName: 'Enable SQL Browser service'
  inputs:
    targetType: 'filePath'
    filePath: '.pipeline/scripts/Enable-SqlBrowser.ps1'
    arguments: '-ServiceName "SQLBrowser"'

- task: PowerShell@2
  displayName: 'Enable Named Pipes and Shared Memory protocols'
  inputs:
    targetType: 'filePath'
    filePath: '.pipeline/scripts/Enable-SqlProtocols.ps1'
    arguments: '-InstanceName "MSSQLSERVER" -SqlVersion "MSSQL17" -RestartService $true'
```

## Prerequisites

- PowerShell with Administrator privileges
- SQL Server installed on the target machine
- Access to Windows registry for protocol configuration

## Error Handling

Both scripts include comprehensive error handling and will:
- Display clear status messages
- Exit with non-zero code on critical failures
- Provide troubleshooting information for common issues