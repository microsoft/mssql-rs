# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# run-benchmarks.ps1 — perf-lab testScript for mssql-tds (Windows).
#
# Windows equivalent of run-benchmarks.sh. The shared Perf.Test.Job template
# copies the repository to the VM and launches this script from the repo root,
# with SQL_SERVER and SQL_PASSWORD injected as environment variables.
#
# Builds the mssql-tds-bench harness TWICE from the SAME (candidate) working
# tree, swapping ONLY the mssql-tds dependency (working tree vs a local
# `git worktree` of the perf-baseline tag), then compares with critcmp.

$ErrorActionPreference = 'Stop'

# Baseline pointer — hard-coded; the tag is moved manually in the git repo.
$BaselineTag = 'perf-baseline'

$RepoRoot   = (Get-Location).Path
$ResultsDir = Join-Path $RepoRoot 'results'
New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

# --- Connection (SQL_SERVER / SQL_PASSWORD injected by run-remote) ---
if (-not $env:SQL_SERVER)   { throw 'SQL_SERVER not set' }
if (-not $env:SQL_PASSWORD) { throw 'SQL_PASSWORD not set' }
$env:DB_HOST = $env:SQL_SERVER
if (-not $env:DB_PORT)                 { $env:DB_PORT = '1433' }
if (-not $env:DB_USERNAME)             { $env:DB_USERNAME = 'sa' }
if (-not $env:TRUST_SERVER_CERTIFICATE){ $env:TRUST_SERVER_CERTIFICATE = 'true' }

# --- Toolchain ---
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host '>>> Installing Rust toolchain via rustup...'
    Invoke-WebRequest 'https://win.rustup.rs' -OutFile 'rustup-init.exe'
    & ./rustup-init.exe -y --default-toolchain stable
}
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

# --- git (needed for the baseline worktree) ---
# The Windows Server perf image (RUST-Win22-Sql25-1P) normally ships git, but
# install it if absent: winget first, then Chocolatey as a fallback.
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Host '>>> git not found; installing...'
    if (Get-Command winget -ErrorAction SilentlyContinue) {
        winget install --id Git.Git -e --source winget `
            --accept-package-agreements --accept-source-agreements
    } else {
        if (-not (Get-Command choco -ErrorAction SilentlyContinue)) {
            Write-Host '>>> Installing Chocolatey...'
            Set-ExecutionPolicy Bypass -Scope Process -Force
            [System.Net.ServicePointManager]::SecurityProtocol = `
                [System.Net.ServicePointManager]::SecurityProtocol -bor 3072
            Invoke-Expression ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
        }
        choco install git -y --no-progress
    }
    # Refresh PATH so the freshly installed git resolves in this session.
    $env:PATH = [System.Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
                [System.Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        throw 'git installation failed'
    }
}

if (-not (Get-Command critcmp -ErrorAction SilentlyContinue)) {
    Write-Host '>>> Installing critcmp...'
    cargo install critcmp --version 0.1.8 --locked
}

# --- Verify the baseline tag is present in the shipped .git ---
& git rev-parse "refs/tags/$BaselineTag^{commit}" *> $null
if ($LASTEXITCODE -ne 0) {
    throw "Baseline tag '$BaselineTag' not found in the shipped repository. Ensure the checkout fetches tags."
}

# --- Candidate run (mssql-tds = working tree) ---
Write-Host '>>> Candidate benchmarks...'
cargo bench -p mssql-tds-bench -- --save-baseline candidate

# --- Baseline run (mssql-tds source swapped to the perf-baseline tag) ---
# Materialize the baseline mssql-tds via a local worktree, then replace the
# workspace's mssql-tds source in place. The harness (mssql-tds-bench) and its
# `path = "../mssql-tds"` dependency are unchanged, so mssql-tds is the only
# variable. Swapping the source keeps a single mssql-tds in the workspace and
# avoids a Cargo lockfile package collision.
$BaselineTree = Join-Path ([System.IO.Path]::GetTempPath()) "perf-baseline-$([System.Guid]::NewGuid().ToString('N'))"
Write-Host ">>> Adding baseline worktree for tag '$BaselineTag' at $BaselineTree..."
& git worktree add --detach $BaselineTree "refs/tags/$BaselineTag"

Write-Host '>>> Swapping mssql-tds source to the baseline...'
$CandidateSrc = Join-Path $RepoRoot 'mssql-tds'
$StashedSrc   = Join-Path $RepoRoot '.mssql-tds-candidate'
Move-Item $CandidateSrc $StashedSrc
Copy-Item -Recurse (Join-Path $BaselineTree 'mssql-tds') $CandidateSrc

Write-Host '>>> Baseline benchmarks...'
cargo bench -p mssql-tds-bench -- --save-baseline base

# Restore the candidate source and remove the worktree.
Remove-Item -Recurse -Force $CandidateSrc
Move-Item $StashedSrc $CandidateSrc
& git worktree remove --force $BaselineTree

# --- Compare ---
Write-Host '>>> Comparing base -> candidate...'
critcmp base candidate | Tee-Object -FilePath (Join-Path $ResultsDir 'comparison.txt')

Copy-Item -Recurse -Force 'target/criterion' (Join-Path $ResultsDir 'criterion') -ErrorAction SilentlyContinue

Write-Host ">>> Done. Results in $ResultsDir"
