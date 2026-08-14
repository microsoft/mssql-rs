# PowerShell script for running cargo clippy on workspace and mssql-py-core
# Windows equivalent of bclippy.sh

$ErrorActionPreference = "Stop"

Write-Host "Running cargo clippy on mssql-tds production configuration..."
cargo clippy --frozen -p mssql-tds --lib -- -D warnings
if ($LASTEXITCODE -ne 0) {
    Write-Error "mssql-tds production clippy failed"
    exit $LASTEXITCODE
}

Write-Host "Running cargo clippy on workspace..."
cargo clippy --workspace --frozen --all-features --all-targets -- -D warnings
if ($LASTEXITCODE -ne 0) {
    Write-Error "Workspace clippy failed"
    exit $LASTEXITCODE
}

Write-Host "Running cargo clippy on mssql-py-core..."
Push-Location mssql-py-core
try {
    cargo fetch
    cargo clippy --frozen --all-features --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) {
        Write-Error "mssql-py-core clippy failed"
        exit $LASTEXITCODE
    }
}
finally {
    Pop-Location
}

Write-Host "✓ All clippy checks passed!"
