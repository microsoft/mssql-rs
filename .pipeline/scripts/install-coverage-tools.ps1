# Windows counterpart of install-coverage-tools.sh: installs the pinned
# cargo-llvm-cov and cargo-nextest from their published prebuilt binaries
# instead of compiling them with `cargo install`. Falls back to a source build
# if a download fails.
param(
    [string]$LlvmCovVersion = '0.6.16',
    [string]$NextestVersion = '0.9.99'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# Probing for an absent cargo subcommand is an expected non-zero exit, not a
# failure, and PowerShell 7.4+ turns those into terminating errors by default.
$PSNativeCommandUseErrorActionPreference = $false

$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$cargoBin = Join-Path $cargoHome 'bin'
New-Item -ItemType Directory -Force -Path $cargoBin | Out-Null

# Both assets are zips containing the bare executable at their root.
function Install-PrebuiltTool {
    param(
        [string]$Subcommand,
        [string]$Binary,
        [string]$Version,
        [string]$Url
    )

    $installed = ''
    try { $installed = (& cargo $Subcommand --version 2>$null) -join "`n" } catch { }
    if ($installed -match [regex]::Escape($Version)) {
        Write-Host "$Binary $Version already present, skipping"
        return
    }

    $temp = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid())
    try {
        New-Item -ItemType Directory -Force -Path $temp | Out-Null
        $archive = Join-Path $temp 'tool.zip'
        Invoke-WebRequest -Uri $Url -OutFile $archive -MaximumRetryCount 3 -RetryIntervalSec 2
        Expand-Archive -Path $archive -DestinationPath $temp -Force
        Copy-Item -Path (Join-Path $temp "$Binary.exe") -Destination $cargoBin -Force
        Write-Host "Installed prebuilt $Binary $Version"
    }
    catch {
        Write-Host "Prebuilt $Binary download failed ($($_.Exception.Message)), building from source"
        cargo install $Binary --version $Version --locked
        if ($LASTEXITCODE -ne 0) { throw "cargo install $Binary failed with exit code $LASTEXITCODE" }
    }
    finally {
        Remove-Item -Recurse -Force -Path $temp -ErrorAction SilentlyContinue
    }
}

Install-PrebuiltTool -Subcommand 'llvm-cov' -Binary 'cargo-llvm-cov' -Version $LlvmCovVersion `
    -Url "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v$LlvmCovVersion/cargo-llvm-cov-x86_64-pc-windows-msvc.zip"

Install-PrebuiltTool -Subcommand 'nextest' -Binary 'cargo-nextest' -Version $NextestVersion `
    -Url "https://get.nexte.st/$NextestVersion/windows"
