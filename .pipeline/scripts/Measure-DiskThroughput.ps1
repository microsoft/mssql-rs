# Measure raw disk throughput where the agent work folder lives (Windows).
# Uses Microsoft diskspd with cache bypass (-Sh) so results reflect the disk,
# not the OS file cache. Reports sequential + random throughput and IOPS.
[CmdletBinding()]
param(
    [string]$Label = "unknown-pool",
    [string]$TargetDir = "",
    [int]$FileSizeGB = 4,
    [int]$DurationSec = 20
)

$ErrorActionPreference = 'Stop'
function Section($t) { Write-Host "`n==== $t ====" -ForegroundColor Cyan }

# --- Resolve the work folder (where build I/O actually happens) ---
if (-not $TargetDir) { $TargetDir = $env:PIPELINE_WORKSPACE }
if (-not $TargetDir) { $TargetDir = $env:AGENT_BUILDDIRECTORY }
if (-not $TargetDir) { $TargetDir = $env:AGENT_WORKFOLDER }
if (-not $TargetDir) { $TargetDir = (Get-Location).Path }
$TargetDir = (Resolve-Path $TargetDir).Path
$drive = (Split-Path -Qualifier $TargetDir)
Write-Host "Label            : $Label"
Write-Host "Work/target dir  : $TargetDir"
Write-Host "Target drive     : $drive"

# --- Report the physical disk backing that drive ---
Section "Disk topology"
try {
    Get-PhysicalDisk |
        Select-Object DeviceId, FriendlyName, MediaType, BusType,
            @{n='SizeGB';e={[math]::Round($_.Size/1GB)}} |
        Format-Table -AutoSize | Out-String | Write-Host
} catch { Write-Host "Get-PhysicalDisk failed: $($_.Exception.Message)" }
try {
    Get-Volume |
        Select-Object DriveLetter, FileSystemType,
            @{n='SizeGB';e={[math]::Round($_.Size/1GB)}},
            @{n='FreeGB';e={[math]::Round($_.SizeRemaining/1GB)}} |
        Format-Table -AutoSize | Out-String | Write-Host
} catch {}
try {
    $letter = $drive.TrimEnd(':')
    $part = Get-Partition -DriveLetter $letter -ErrorAction Stop
    $pdisk = Get-Disk -Number $part.DiskNumber | Get-PhysicalDisk
    Write-Host ("Target volume {0} -> disk #{1}: {2} BusType={3} MediaType={4}" -f `
        $drive, $part.DiskNumber, $pdisk.FriendlyName, $pdisk.BusType, $pdisk.MediaType)
} catch { Write-Host "volume->disk mapping failed: $($_.Exception.Message)" }

# --- Download diskspd ---
Section "Downloading diskspd"
$toolDir = Join-Path $env:AGENT_TEMPDIRECTORY 'diskspd'
if (-not $toolDir) { $toolDir = Join-Path $TargetDir 'diskspd' }
New-Item -ItemType Directory -Force -Path $toolDir | Out-Null
$zip = Join-Path $toolDir 'DiskSpd.zip'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
Invoke-WebRequest -Uri 'https://github.com/microsoft/diskspd/releases/download/v2.2/DiskSpd.zip' `
    -OutFile $zip -UseBasicParsing
Expand-Archive -Path $zip -DestinationPath $toolDir -Force
$diskspd = Get-ChildItem -Path $toolDir -Recurse -Filter 'diskspd.exe' |
    Where-Object { $_.FullName -match '\\amd64\\' } | Select-Object -First 1
if (-not $diskspd) {
    $diskspd = Get-ChildItem -Path $toolDir -Recurse -Filter 'diskspd.exe' | Select-Object -First 1
}
if (-not $diskspd) { throw "diskspd.exe not found after extraction" }
Write-Host "diskspd: $($diskspd.FullName)"

# --- Run scenarios ---
$testFile = Join-Path $TargetDir 'diskspd_probe.dat'
$results = New-Object System.Collections.Generic.List[object]

function Invoke-Scenario($name, [string[]]$scArgs) {
    Section "Scenario: $name"
    $all = @($scArgs + @('-L', '-W3', "-d$DurationSec", "-c${FileSizeGB}G", $testFile))
    Write-Host "diskspd $($all -join ' ')"
    $out = & $diskspd.FullName @all 2>&1 | Out-String
    Write-Host $out
    $mib = $null; $iops = $null
    foreach ($line in ($out -split "`r?`n")) {
        if ($line -match '^total:\s+\d+\s*\|\s*\d+\s*\|\s*([\d.]+)\s*\|\s*([\d.]+)') {
            $mib = [double]$Matches[1]; $iops = [double]$Matches[2]
            break
        }
    }
    $results.Add([pscustomobject]@{ Scenario = $name; 'MB/s' = $mib; 'IOPS' = $iops })
}

# block size / pattern / queue depth / threads chosen to mirror build-like I/O
Invoke-Scenario 'seq-write-1M'   @('-w100','-b1M','-o8','-t4','-Sh')
Invoke-Scenario 'seq-read-1M'    @('-w0','-b1M','-o8','-t4','-Sh')
Invoke-Scenario 'rand-read-4K'   @('-w0','-r','-b4K','-o8','-t4','-Sh')
Invoke-Scenario 'rand-write-4K'  @('-w100','-r','-b4K','-o8','-t4','-Sh')
Invoke-Scenario 'rand-rw-64K-30w' @('-w30','-r','-b64K','-o8','-t4','-Sh')

# --- Cleanup ---
Remove-Item -Path $testFile -Force -ErrorAction SilentlyContinue

# --- Summary ---
Section "SUMMARY [$Label]"
$results | Format-Table -AutoSize | Out-String | Write-Host
Write-Host "##[section]DISK-THROUGHPUT-RESULT label=$Label"
foreach ($r in $results) {
    Write-Host ("RESULT|{0}|{1}|{2:N2}|{3:N2}" -f $Label, $r.Scenario, $r.'MB/s', $r.'IOPS')
}
