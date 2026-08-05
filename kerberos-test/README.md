# Kerberos Test Environment for CI

This directory contains a containerized Kerberos authentication test environment for the CI pipeline. It uses **Samba AD DC** as the Active Directory Domain Controller.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Docker Network (172.20.0.0/24)                  │
│                                                                     │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────────┐  │
│  │   Samba AD DC   │  │   SQL Server    │  │      Client         │  │
│  │  (172.20.0.10)  │  │  (172.20.0.20)  │  │   (172.20.0.30)     │  │
│  │                 │  │                 │  │                     │  │
│  │  - Kerberos KDC │  │  - SSSD joined  │  │  - Rust toolchain   │  │
│  │  - LDAP         │  │  - Keytab auth  │  │  - Kerberos tools   │  │
│  │  - DNS          │  │                 │  │                     │  │
│  └─────────────────┘  └─────────────────┘  └─────────────────────┘  │
│         dc.example.local    sql.example.local     client            │
└─────────────────────────────────────────────────────────────────────┘
```

## Files

| File | Purpose |
|------|---------|
| `docker-compose-samba.yml` | Docker Compose for single-client testing |
| `docker-compose-matrix.yml` | Docker Compose for multi-distro matrix testing |
| `Dockerfile.client` | Default client container (based on .NET SDK) |
| `Dockerfile.client.matrix` | Parameterized client for all distros (uses build args) |
| `Dockerfile.samba-dc` | Samba AD Domain Controller (init script mounted from `scripts/`) |
| `Dockerfile.mssql-ad` | SQL Server with AD integration (init script mounted from `scripts/`) |
| `configure-kerberos.sh` | Script to configure Kerberos after containers start |
| `run-kerberos-tests.sh` | Run tests on a specific distro |
| `run-all-distros.sh` | Run tests on all distros in the matrix |
| `cleanup.sh` | Script to tear down the environment |
| `config/` | Kerberos configuration files |

## Domain Configuration

| Setting | Value |
|---------|-------|
| Domain Name | EXAMPLE.LOCAL |
| NetBIOS Name | EXAMPLE |
| Domain Controller | dc.example.local (172.20.0.10) |
| SQL Server | sql.example.local (172.20.0.20) |

## Credentials

Credentials are **generated at runtime** using the `generate-env.sh` script. This avoids committing secrets to the repository.

```bash
# Generate credentials (creates .env file)
./generate-env.sh

# View generated credentials
cat .env
```

The `.env` file is gitignored and contains:
- `KERBEROS_ADMIN_PASSWORD` - Domain Administrator password
- `KERBEROS_SA_PASSWORD` - SQL Server SA password
- `KERBEROS_TEST_USER_PASSWORD` - Test user password

**For backward compatibility**, scripts fall back to default passwords if `.env` is not found. However, credential scanners will flag these defaults, so always use `generate-env.sh` in CI.

| Account | Username | Environment Variable |
|---------|----------|---------------------|
| Domain Admin | Administrator | KERBEROS_ADMIN_PASSWORD |
| SQL Server SA | sa | KERBEROS_SA_PASSWORD |
| Test User | testuser | KERBEROS_TEST_USER_PASSWORD |

## CI Pipeline Usage

The CI pipeline (`.pipeline/templates/kerberos-test-template.yml`) uses this environment to:

1. Build and start containers with `docker compose -f docker-compose-samba.yml up -d`
2. Wait for Samba DC and SQL Server to be healthy
3. Run `configure-kerberos.sh` to set up authentication
4. Execute Rust GSSAPI/Kerberos tests inside the client container
5. Clean up with `cleanup.sh`

## Local Development

For local testing, you can manually run:

```bash
# Generate credentials first (creates .env file)
./generate-env.sh

# Start the environment
docker compose -f docker-compose-samba.yml up -d

# Wait for DC to be healthy
docker compose -f docker-compose-samba.yml ps

# Configure Kerberos
./configure-kerberos.sh

# Access the client container
docker exec -it kerberos-client bash

# Get a Kerberos ticket (password from .env)
source .env
echo "$KERBEROS_TEST_USER_PASSWORD" | kinit testuser@EXAMPLE.LOCAL
klist

# Clean up when done
./cleanup.sh
```
## Multi-Distro Matrix Testing

Test Kerberos authentication across multiple Linux distributions using the same
base images as the CI matrix (from `validation-pipeline.yml`).

All images come from `tdslibrs.azurecr.io/import/` - the same ACR used by CI.

### Test a Specific Distro

```bash
# Generate credentials first
./generate-env.sh

# Start infrastructure and one distro
docker compose -f docker-compose-matrix.yml --profile ubuntu22 up -d

# Configure Kerberos
./configure-kerberos.sh

# Run tests on that distro
./run-kerberos-tests.sh ubuntu22
```

### Available Profiles (matching CI matrix)

| Profile | Base Image | libc |
|---------|------------|------|
| `alpine318` | tdslibrs.azurecr.io/import/alpine:3.18 | musl |
| `alpine319` | tdslibrs.azurecr.io/import/alpine:3.19 | musl |
| `alpine320` | tdslibrs.azurecr.io/import/alpine:3.20 | musl |
| `alpine321` | tdslibrs.azurecr.io/import/alpine:3.21 | musl |
| `debian` | tdslibrs.azurecr.io/import/debian:bookworm | glibc |
| `ubuntu22` | tdslibrs.azurecr.io/import/ubuntu:22.04 | glibc |
| `ubuntu24` | tdslibrs.azurecr.io/import/ubuntu:24.04 | glibc |
| `rhel9` | tdslibrs.azurecr.io/import/redhat/ubi9:latest | glibc |
| `oracle9` | tdslibrs.azurecr.io/import/oraclelinux:9 | glibc |

### Profile Groups

```bash
# Test all Alpine versions
docker compose -f docker-compose-matrix.yml --profile alpine up -d

# Test all Ubuntu versions
docker compose -f docker-compose-matrix.yml --profile ubuntu up -d

# Test everything
docker compose -f docker-compose-matrix.yml --profile all up -d
```

### Run All Distros

```bash
# This script handles everything: build, start, configure, test all distros
./run-all-distros.sh

# Or test specific profiles
./run-all-distros.sh ubuntu22 alpine318 debian
```
## CI Mode (JUnit XML Output)

When running in Azure DevOps CI, use the `--ci` flag to:
- Use `cargo-nextest` instead of `cargo test`
- Output JUnit XML format for test result publishing
- Copy results to `test-results/<profile>/junit.xml`

```bash
# Run tests in CI mode
./run-kerberos-tests.sh ubuntu22 --ci

# JUnit XML will be at:
# kerberos-test/test-results/ubuntu22/junit.xml
```

The pipeline template should use `PublishTestResults@2` to upload results:

```yaml
- task: PublishTestResults@2
  condition: always()
  inputs:
    testResultsFormat: "JUnit"
    testResultsFiles: "kerberos-test/test-results/*/junit.xml"
    testRunTitle: "Kerberos Tests - $(profile)"
```
