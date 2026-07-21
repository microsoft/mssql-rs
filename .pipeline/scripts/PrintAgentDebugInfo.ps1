Write-Host "Current PATH:"
Write-Host $env:Path
Write-Host ""

$pythonDir = "C:\ProgramData\Tools\Python\3.12.10\x64"
if (Test-Path $pythonDir) {
    Write-Host "Directory $pythonDir exists"
    Write-Host "Recursive directory tree:"
    Get-ChildItem -Path $pythonDir -Recurse -Force | ForEach-Object {
        $relativePath = $_.FullName.Substring($pythonDir.Length)
        if ($_.PSIsContainer) {
            Write-Host "[DIR]  $relativePath"
        } else {
            Write-Host "[FILE] $relativePath ($($_.Length) bytes)"
        }
    }
} else {
    Write-Host "Directory $pythonDir does NOT exist"
}

Write-Host ""
Write-Host "=== Physical Disks (BusType shows NVMe / SATA / SAS) ==="
try {
    Get-PhysicalDisk | Select-Object DeviceId, FriendlyName, MediaType, BusType,
        @{N='SizeGB';E={[math]::Round($_.Size/1GB,1)}} |
        Format-Table -AutoSize | Out-String | Write-Host
} catch { Write-Host "Get-PhysicalDisk failed: $_" }

Write-Host "=== Disks ==="
try {
    Get-Disk | Select-Object Number, FriendlyName, BusType,
        @{N='SizeGB';E={[math]::Round($_.Size/1GB,1)}}, PartitionStyle, OperationalStatus |
        Format-Table -AutoSize | Out-String | Write-Host
} catch { Write-Host "Get-Disk failed: $_" }

Write-Host "=== Volumes (drive letters + free space) ==="
try {
    Get-Volume | Where-Object DriveLetter | Select-Object DriveLetter, FileSystemLabel, FileSystem,
        @{N='SizeGB';E={[math]::Round($_.Size/1GB,1)}},
        @{N='FreeGB';E={[math]::Round($_.SizeRemaining/1GB,1)}} |
        Format-Table -AutoSize | Out-String | Write-Host
} catch { Write-Host "Get-Volume failed: $_" }
