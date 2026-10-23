#!/usr/bin/env pwsh

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false

# Check if cargo nextest is installed
cargo nextest --version *> $null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Error: cargo-nextest is not installed."
    Write-Host "Please install it with: cargo install cargo-nextest"
    exit 1
}

# Generate certificates required for tests
& "$PSScriptRoot/../scripts/generate_mock_tds_server_certs.ps1"

# Run from the repository root so relative paths resolve from any directory.
Push-Location (Join-Path $PSScriptRoot "..")
try {
    # Run workspace Rust tests. mssql-tds is excluded here because its
    # connectivity-filtered suite runs below.
    Write-Host "Running workspace tests..."
    $WorkspaceExitCode = 0
    cargo nextest run `
        --workspace `
        --exclude mssql-tds `
        --all-targets `
        --no-fail-fast `
        --profile ci `
        --success-output immediate
    $WorkspaceExitCode = $LASTEXITCODE

    # Run mssql-py-core independently because it is outside the workspace.
    Write-Host "Running tests for mssql-py-core..."
    $PyCoreExitCode = 0
    Push-Location mssql-py-core
    try {
        # Cargo.lock is untracked, so fetch dependencies to generate it first.
        cargo fetch
        $PyCoreExitCode = $LASTEXITCODE

        if ($PyCoreExitCode -eq 0) {
            cargo nextest run `
                --all-targets `
                --no-fail-fast `
                --profile ci `
                --success-output immediate

            $PyCoreExitCode = $LASTEXITCODE
        }
    }
    finally {
        Pop-Location
    }

    # Run the mssql-tds integration tests with connectivity excluded.
    Write-Host "Running mssql-tds integration tests..."
    $MssqlTdsExitCode = 0
    cargo nextest run `
        -E "not (test(connectivity))" `
        --all-targets `
        -p mssql-tds `
        --no-fail-fast `
        --profile ci `
        --success-output immediate
    $MssqlTdsExitCode = $LASTEXITCODE
}
finally {
    Pop-Location
}

if ($WorkspaceExitCode -ne 0) {
    Write-Host "Workspace tests failed"
}

if ($PyCoreExitCode -ne 0) {
    Write-Host "mssql-py-core tests failed"
}

if ($MssqlTdsExitCode -ne 0) {
    Write-Host "mssql-tds integration tests failed"
}

if ($WorkspaceExitCode -ne 0) {
    exit $WorkspaceExitCode
}

if ($PyCoreExitCode -ne 0) {
    exit $PyCoreExitCode
}

exit $MssqlTdsExitCode
