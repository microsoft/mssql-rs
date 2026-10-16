#!/usr/bin/env pwsh

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false

# Run workspace Rust tests.
Write-Host "Running workspace tests..."
$WorkspaceExitCode = 0
cargo nextest run `
    --workspace `
    --no-fail-fast `
    --profile ci `
    --success-output immediate
$WorkspaceExitCode = $LASTEXITCODE

# Run mssql-py-core independently because it is outside the workspace.
Write-Host "Running tests for mssql-py-core..."
$PyCoreExitCode = 0
Push-Location mssql-py-core
try {
    cargo nextest run `
        --all-targets `
        --no-fail-fast `
        --profile ci `
        --success-output immediate

    $PyCoreExitCode = $LASTEXITCODE
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
