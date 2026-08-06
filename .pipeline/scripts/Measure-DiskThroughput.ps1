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
    $busType = [string]$pdisk.BusType
    $mediaType = [string]$pdisk.MediaType
    Write-Host ("Target volume {0} -> disk #{1}: {2} BusType={3} MediaType={4}" -f `
        $drive, $part.DiskNumber, $pdisk.FriendlyName, $busType, $mediaType)
} catch { Write-Host "volume->disk mapping failed: $($_.Exception.Message)" }

# --- Agent CPU / RAM / Azure SKU ---
Section "Agent info"
try {
    $cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
    $cs  = Get-CimInstance Win32_ComputerSystem
    Write-Host ("CPU        : {0}" -f $cpu.Name.Trim())
    Write-Host ("Cores      : {0} physical / {1} logical" -f $cpu.NumberOfCores, $cpu.NumberOfLogicalProcessors)
    Write-Host ("RAM        : {0:N1} GB" -f ($cs.TotalPhysicalMemory / 1GB))
} catch { Write-Host "cpu info failed: $($_.Exception.Message)" }
try {
    $imds = Invoke-RestMethod -Headers @{Metadata='true'} -TimeoutSec 5 `
        -Uri 'http://169.254.169.254/metadata/instance/compute?api-version=2021-02-01'
    Write-Host ("Azure VM   : size={0} location={1} zone={2}" -f $imds.vmSize, $imds.location, $imds.zone)
} catch { Write-Host "IMDS query failed: $($_.Exception.Message)" }

# --- Rust / cargo cache warmth (fresh ephemeral images start cold) ---
Section "Rust / cargo cache warmth"
foreach ($exe in 'rustc', 'cargo') {
    $cmd = Get-Command $exe -ErrorAction SilentlyContinue
    if ($cmd) { Write-Host ("{0}: {1}" -f $exe, (& $exe --version)) }
    else { Write-Host "${exe}: not on PATH" }
}
$cargoHome = $env:CARGO_HOME
if (-not $cargoHome) { $cargoHome = Join-Path $env:USERPROFILE '.cargo' }
$registry = Join-Path $cargoHome 'registry'
if (Test-Path $registry) {
    $files = Get-ChildItem -Path $registry -Recurse -File -ErrorAction SilentlyContinue
    $szMB = (($files | Measure-Object Length -Sum).Sum) / 1MB
    Write-Host ("cargo registry cache: {0} files, {1:N1} MB at {2}" -f $files.Count, $szMB, $registry)
} else {
    Write-Host "cargo registry cache: NONE at $registry (cold)"
}

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
    # diskspd writes warnings to stderr; don't let that become a terminating error.
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $out = & $diskspd.FullName @all 2>&1 | Out-String
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev
    Write-Host $out
    if ($code -ne 0) {
        Write-Host "##vso[task.logissue type=warning]diskspd exited $code for scenario $name"
    }
    $mib = $null; $iops = $null
    foreach ($line in ($out -split "`r?`n")) {
        if ($line -match '^total:\s+\d+\s*\|\s*\d+\s*\|\s*([\d.]+)\s*\|\s*([\d.]+)') {
            $mib = [double]$Matches[1]; $iops = [double]$Matches[2]
            break
        }
    }
    $results.Add([pscustomobject]@{ Scenario = $name; 'MB/s' = $mib; 'IOPS' = $iops })
}

# block size / pattern / queue depth / threads chosen to mirror build-like I/O.
# Sequential runs use a single thread (-t1) so the pattern stays truly sequential.
Invoke-Scenario 'seq-write-1M'   @('-w100','-b1M','-o8','-t1','-Sh')
Invoke-Scenario 'seq-read-1M'    @('-w0','-b1M','-o8','-t1','-Sh')
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
    Write-Host ("RESULT|{0}|{1}|{2:F2}|{3:F2}" -f $Label, $r.Scenario, $r.'MB/s', $r.'IOPS')
}

# --- Publish a per-job markdown summary to the build Summary tab ---
$md = New-Object System.Text.StringBuilder
[void]$md.AppendLine("### Disk throughput - $Label")
[void]$md.AppendLine("")
[void]$md.AppendLine("Work dir ``$TargetDir`` on drive $drive (BusType=$busType, MediaType=$mediaType), cache-bypass (diskspd -Sh).")
[void]$md.AppendLine("")
[void]$md.AppendLine("| Scenario | MB/s | IOPS |")
[void]$md.AppendLine("|---|---:|---:|")
foreach ($r in $results) {
    [void]$md.AppendLine(("| {0} | {1:F2} | {2:F2} |" -f $r.Scenario, $r.'MB/s', $r.'IOPS'))
}
$tempDir = $env:AGENT_TEMPDIRECTORY; if (-not $tempDir) { $tempDir = $TargetDir }
$summaryPath = Join-Path $tempDir 'disk-summary.md'
[System.IO.File]::WriteAllText($summaryPath, $md.ToString())
Write-Host "##vso[task.uploadsummary]$summaryPath"

# --- Emit machine-readable results for the aggregation job ---
$staging = $env:BUILD_ARTIFACTSTAGINGDIRECTORY
if ($staging) {
    New-Item -ItemType Directory -Force -Path $staging | Out-Null
    $lines = foreach ($r in $results) {
        "RESULT|{0}|{1}|{2:F2}|{3:F2}" -f $Label, $r.Scenario, $r.'MB/s', $r.'IOPS'
    }
    Set-Content -Path (Join-Path $staging 'results.txt') -Value $lines
}
