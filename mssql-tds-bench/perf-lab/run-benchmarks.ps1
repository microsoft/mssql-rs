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
$Manifest   = Join-Path $RepoRoot 'mssql-tds-bench/Cargo.toml'
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

# --- Baseline run (mssql-tds = perf-baseline tag via a local worktree) ---
$BaselineTree = Join-Path ([System.IO.Path]::GetTempPath()) "perf-baseline-$([System.Guid]::NewGuid().ToString('N'))"
Write-Host ">>> Adding baseline worktree for tag '$BaselineTag' at $BaselineTree..."
& git worktree add --detach $BaselineTree "refs/tags/$BaselineTag"

Write-Host '>>> Swapping mssql-tds dependency to the baseline source...'
Copy-Item $Manifest "$Manifest.bak"
$baselineMssqlTds = (Join-Path $BaselineTree 'mssql-tds') -replace '\\', '/'
(Get-Content $Manifest -Raw).Replace(
    'mssql-tds = { path = "../mssql-tds" }',
    "mssql-tds = { path = `"$baselineMssqlTds`" }") | Set-Content $Manifest -NoNewline

Write-Host '>>> Baseline benchmarks...'
cargo bench -p mssql-tds-bench -- --save-baseline base

# Restore the committed manifest and remove the worktree.
Move-Item -Force "$Manifest.bak" $Manifest
& git worktree remove --force $BaselineTree

# --- Compare ---
Write-Host '>>> Comparing base -> candidate...'
critcmp base candidate | Tee-Object -FilePath (Join-Path $ResultsDir 'comparison.txt')

Copy-Item -Recurse -Force 'target/criterion' (Join-Path $ResultsDir 'criterion') -ErrorAction SilentlyContinue

Write-Host ">>> Done. Results in $ResultsDir"
