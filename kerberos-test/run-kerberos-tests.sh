#!/bin/bash
# Run Kerberos tests on a specific Linux distribution
# Usage: ./run-kerberos-tests.sh <profile> [--ci] [--archive]
#
# Profiles (matching CI matrix from validation-pipeline.yml):
#   Alpine: alpine318, alpine319, alpine320, alpine321
#   Debian: debian
#   Ubuntu: ubuntu22, ubuntu24
#   RHEL:   rhel9
#   Oracle: oracle9
#
# Options:
#   --ci       Run in CI mode (use cargo-nextest, output JUnit XML)
#   --archive  Use pre-built nextest archive (faster, requires archive file)
#
# Examples:
#   ./run-kerberos-tests.sh ubuntu22                    # Local testing (compile from source)
#   ./run-kerberos-tests.sh ubuntu22 --ci               # CI mode with JUnit output
#   ./run-kerberos-tests.sh ubuntu22 --ci --archive     # CI mode with pre-built archive (fastest)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Source credentials from .env file
if [ -f "$SCRIPT_DIR/.env" ]; then
    set -a
    source "$SCRIPT_DIR/.env"
    set +a
else
    echo "ERROR: .env file not found. Run ./generate-env.sh first"
    exit 1
fi

# Verify required environment variables
if [ -z "$KERBEROS_TEST_USER_PASSWORD" ]; then
    echo "ERROR: KERBEROS_TEST_USER_PASSWORD not set in .env"
    exit 1
fi
TEST_USER_PASSWORD="$KERBEROS_TEST_USER_PASSWORD"

PROFILE="${1:-ubuntu22}"
CI_MODE=false
ARCHIVE_MODE=false

# Check for flags
for arg in "$@"; do
    if [ "$arg" == "--ci" ]; then
        CI_MODE=true
    fi
    if [ "$arg" == "--archive" ]; then
        ARCHIVE_MODE=true
    fi
done

# Archive name (glibc for deb/rhel, musl for alpine)
case "$PROFILE" in
    alpine*) ARCHIVE_NAME="tdslib-nextest-musl.tar.zst" ;;
    *)       ARCHIVE_NAME="tdslib-nextest.tar.zst" ;;
esac

# RUSTFLAGS for musl targets: disable static linking to allow dlopen for GSSAPI
# Rust defaults to static linking on musl, but GSSAPI requires dynamic linking
# because the library is loaded at runtime via dlopen()
case "$PROFILE" in
    alpine*) MUSL_RUSTFLAGS='-C target-feature=-crt-static' ;;
    *)       MUSL_RUSTFLAGS='' ;;
esac

# Map profile to container name
case "$PROFILE" in
    alpine318) CONTAINER_NAME="kerberos-client-alpine318" ;;
    alpine319) CONTAINER_NAME="kerberos-client-alpine319" ;;
    alpine320) CONTAINER_NAME="kerberos-client-alpine320" ;;
    alpine321) CONTAINER_NAME="kerberos-client-alpine321" ;;
    debian)    CONTAINER_NAME="kerberos-client-debian" ;;
    ubuntu22)  CONTAINER_NAME="kerberos-client-ubuntu22" ;;
    ubuntu24)  CONTAINER_NAME="kerberos-client-ubuntu24" ;;
    rhel9)     CONTAINER_NAME="kerberos-client-rhel9" ;;
    oracle9)   CONTAINER_NAME="kerberos-client-oracle9" ;;
    --ci)      
        echo "Error: Profile required before --ci flag"
        exit 1
        ;;
    *)
        echo "Error: Unknown profile '$PROFILE'"
        echo ""
        echo "Valid profiles (matching CI matrix):"
        echo "  Alpine: alpine318, alpine319, alpine320, alpine321"
        echo "  Debian: debian"
        echo "  Ubuntu: ubuntu22, ubuntu24"
        echo "  RHEL:   rhel9"
        echo "  Oracle: oracle9"
        exit 1
        ;;
esac

echo "=============================================="
echo "Kerberos Test Runner - $PROFILE"
echo "Container: $CONTAINER_NAME"
echo "CI Mode: $CI_MODE"
echo "Archive Mode: $ARCHIVE_MODE"
if [ "$ARCHIVE_MODE" = true ]; then
    echo "Archive: $ARCHIVE_NAME"
fi

# Verify container is running
if ! docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    echo "Error: Container $CONTAINER_NAME is not running"
    echo ""
    echo "Start the environment with:"
    echo "  cd $SCRIPT_DIR"
    echo "  docker compose -f docker-compose-matrix.yml --profile $PROFILE up -d"
    exit 1
fi

echo ""
echo "Step 1: Configure Kerberos on client..."
echo "----------------------------------------------"

# Create krb5.conf in the container
docker exec "$CONTAINER_NAME" bash -c 'cat > /etc/krb5.conf << EOF
[libdefaults]
    default_realm = EXAMPLE.LOCAL
    dns_lookup_realm = false
    dns_lookup_kdc = false
    rdns = false

[realms]
    EXAMPLE.LOCAL = {
        kdc = dc.example.local
        admin_server = dc.example.local
    }

[domain_realm]
    .example.local = EXAMPLE.LOCAL
    example.local = EXAMPLE.LOCAL
EOF'

echo "✓ Kerberos configuration created"

echo ""
echo "Step 2: Acquire Kerberos ticket..."
echo "----------------------------------------------"

docker exec "$CONTAINER_NAME" bash -c "echo '$TEST_USER_PASSWORD' | kinit testuser@EXAMPLE.LOCAL"
docker exec "$CONTAINER_NAME" klist

echo "✓ Kerberos ticket acquired"

# Create workspace directory
docker exec "$CONTAINER_NAME" mkdir -p /workspace

if [ "$ARCHIVE_MODE" = true ]; then
    # ============================================================
    # ARCHIVE MODE: Use pre-built nextest archive (fastest)
    # ============================================================
    echo ""
    echo "Step 3: Copy nextest archive to container..."
    echo "----------------------------------------------"
    
    # Archive should be in repo root (downloaded by CI)
    ARCHIVE_PATH="$REPO_ROOT/$ARCHIVE_NAME"
    if [ ! -f "$ARCHIVE_PATH" ]; then
        echo "ERROR: Archive not found: $ARCHIVE_PATH"
        echo "For CI: ensure archive is downloaded from build artifacts"
        echo "For local: run 'cargo nextest archive --archive-file $ARCHIVE_NAME' first"
        exit 1
    fi
    
    docker cp "$ARCHIVE_PATH" "$CONTAINER_NAME:/workspace/$ARCHIVE_NAME"
    echo "✓ Archive copied: $ARCHIVE_NAME"
    
    echo ""
    echo "Step 4: Install cargo-nextest..."
    echo "----------------------------------------------"
    docker exec "$CONTAINER_NAME" bash -c '
        cargo install cargo-nextest --version 0.9.99 --locked
    '
    echo "✓ cargo-nextest installed"
    
    echo ""
    echo "Step 5: Run Kerberos tests from archive..."
    echo "----------------------------------------------"
    
    # Run GSSAPI unit tests and Kerberos E2E tests from archive
    # Set KRB5CCNAME explicitly for Heimdal Kerberos (Alpine) compatibility
    # Note: For musl, RUSTFLAGS is already baked into the archive - see archive build step
    docker exec -e KERBEROS_TEST=1 -e KRB5CCNAME=FILE:/tmp/krb5cc_0 "$CONTAINER_NAME" bash -c "
        cd /workspace
        cargo nextest run \\
            --workspace-remap /workspace \\
            --archive-file $ARCHIVE_NAME \\
            -E 'test(test_gssapi) | test(kerberos)' \\
            --run-ignored all \\
            --profile ci \\
            --no-fail-fast
    "
    
    echo ""
    echo "Step 6: Copy JUnit XML results from container..."
    echo "----------------------------------------------"
    
    mkdir -p "$SCRIPT_DIR/test-results/$PROFILE"
    docker cp "$CONTAINER_NAME:/workspace/target/nextest/ci/junit.xml" "$SCRIPT_DIR/test-results/$PROFILE/junit.xml" 2>/dev/null || \
        echo "Warning: Could not copy junit.xml (tests may have failed)"
    
    if [ -f "$SCRIPT_DIR/test-results/$PROFILE/junit.xml" ]; then
        echo "✓ JUnit XML copied to: $SCRIPT_DIR/test-results/$PROFILE/junit.xml"
    fi
    
else
    # ============================================================
    # SOURCE MODE: Copy source and compile (slower, for local dev)
    # ============================================================
    echo ""
    echo "Step 3: Copy source code to container..."
    echo "----------------------------------------------"
    
    # Copy source code (excluding target, .git, and other build artifacts)
    cd "$REPO_ROOT"
    tar --exclude='target' \
        --exclude='.git' \
        --exclude='node_modules' \
        --exclude='__pycache__' \
        --exclude='*.pyc' \
        --exclude='mssql-js' \
        --exclude='mssql-mock-tds-py' \
        --exclude='mssql-py-core' \
        -cf - \
        Cargo.toml \
        Cargo.lock \
        .config \
        mssql-tds \
        mssql-tds-cli \
        mssql-mock-tds \
        | docker exec -i "$CONTAINER_NAME" tar -xf - -C /workspace
    
    # Create simplified workspace Cargo.toml (excludes mssql-js which isn't copied).
    # Must include [workspace.lints] since member crates use lints.workspace = true.
    docker exec -i "$CONTAINER_NAME" bash -c "cat > /workspace/Cargo.toml" << 'OUTER_EOF'
[workspace]
members = [
    "mssql-tds",
    "mssql-tds-cli",
    "mssql-mock-tds",
]
resolver = "2"

[workspace.lints.rust]
unexpected_cfgs = { level = "warn", check-cfg = ['cfg(fuzzing)', 'cfg(feature, values("sspi", "gssapi", "integrated-auth"))'] }
OUTER_EOF
    
    echo "✓ Source code copied"
    
    # In CI mode, install cargo-nextest
    if [ "$CI_MODE" = true ]; then
        echo ""
        echo "Step 4: Install cargo-nextest (CI mode)..."
        echo "----------------------------------------------"
        docker exec "$CONTAINER_NAME" bash -c '
            cargo install cargo-nextest --version 0.9.99 --locked
        '
        echo "✓ cargo-nextest installed"
    fi
    
    echo ""
    echo "Step 5: Run GSSAPI unit tests..."
    echo "----------------------------------------------"
    
    # Set KRB5CCNAME explicitly for Heimdal Kerberos (Alpine) compatibility
    # This ensures test processes can find the credential cache
    # Set RUSTFLAGS for musl targets to enable dynamic linking for GSSAPI dlopen
    if [ "$CI_MODE" = true ]; then
        # CI mode: use nextest with JUnit output
        docker exec -e KERBEROS_TEST=1 -e KRB5CCNAME=FILE:/tmp/krb5cc_0 -e RUSTFLAGS="$MUSL_RUSTFLAGS" "$CONTAINER_NAME" bash -c '
            cd /workspace
            cargo nextest run -p mssql-tds --features gssapi -E "test(test_gssapi)" --profile ci --no-fail-fast
        '
    else
        # Local mode: use cargo test
        docker exec -e KERBEROS_TEST=1 -e KRB5CCNAME=FILE:/tmp/krb5cc_0 -e RUSTFLAGS="$MUSL_RUSTFLAGS" "$CONTAINER_NAME" bash -c '
            cd /workspace
            cargo test -p mssql-tds --features gssapi test_gssapi -- --nocapture
        '
    fi
    
    echo ""
    echo "Step 6: Run Kerberos E2E tests..."
    echo "----------------------------------------------"
    
    if [ "$CI_MODE" = true ]; then
        # CI mode: use nextest with JUnit output
        # Run ignored tests that require Kerberos environment
        docker exec -e KERBEROS_TEST=1 -e KRB5CCNAME=FILE:/tmp/krb5cc_0 -e RUSTFLAGS="$MUSL_RUSTFLAGS" "$CONTAINER_NAME" bash -c '
            cd /workspace
            cargo nextest run -p mssql-tds --features gssapi -E "test(kerberos)" --run-ignored ignored-only --profile ci --no-fail-fast
        '
        
        echo ""
        echo "Step 7: Copy JUnit XML results from container..."
        echo "----------------------------------------------"
        
        mkdir -p "$SCRIPT_DIR/test-results/$PROFILE"
        docker cp "$CONTAINER_NAME:/workspace/target/nextest/ci/junit.xml" "$SCRIPT_DIR/test-results/$PROFILE/junit.xml" 2>/dev/null || \
            echo "Warning: Could not copy junit.xml (tests may have failed)"
        
        if [ -f "$SCRIPT_DIR/test-results/$PROFILE/junit.xml" ]; then
            echo "✓ JUnit XML copied to: $SCRIPT_DIR/test-results/$PROFILE/junit.xml"
        fi
    else
        # Local mode: use cargo test
        docker exec -e KERBEROS_TEST=1 -e KRB5CCNAME=FILE:/tmp/krb5cc_0 -e RUSTFLAGS="$MUSL_RUSTFLAGS" "$CONTAINER_NAME" bash -c '
            cd /workspace
            cargo test -p mssql-tds --features gssapi kerberos -- --nocapture --ignored
        '
    fi
fi

echo ""
echo "=============================================="
echo "✅ All Kerberos tests passed on $PROFILE!"
echo "=============================================="
