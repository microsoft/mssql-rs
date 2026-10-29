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

### Pre-built images

The DC, SQL+AD and per-distro client images are pre-built weekly by
`.pipeline/sync-container-images.yml` and published to
`ghcr.io/microsoft/mssql-rs/kerberos/*`. CI pulls them instead of rebuilding,
which skips the Rust toolchain install, the distro dev packages and the
Samba/SSSD stacks on every run.

**CI never builds these images.** If a pull fails the stage fails, so a missing,
unpublished or private image surfaces immediately instead of being masked by a
slow rebuild. Changes to `Dockerfile.samba-dc`, `Dockerfile.mssql-ad` or
`Dockerfile.client.matrix` therefore only take effect in CI once the sync
pipeline has republished the images.

Because `docker-compose-*.yml` declare both `image:` and `build:`, local
workflows can still pull or build:

```bash
docker compose -f docker-compose-matrix.yml --profile ubuntu22 pull   # use pre-built
docker compose -f docker-compose-matrix.yml --profile ubuntu22 build  # build locally
```

Override the source registry/tag with `KERBEROS_IMAGE_REGISTRY` and
`KERBEROS_IMAGE_TAG`.

The DC and SQL images run their init script from the mounted `./scripts`
directory rather than a copy baked into the image, so edits to
`scripts/init-*.sh` take effect without republishing the image.

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

### Matrix Host Access and Local Diagnostics

`docker-compose-matrix.yml` publishes services only on the container host's IPv4 loopback address (`127.0.0.1`), not its LAN interfaces. This reduces unintended network exposure; it does not replace authentication or host firewall controls. The single-client Compose configuration is unchanged.

| Service | Host endpoint | Container port |
|---------|---------------|----------------|
| DNS | `127.0.0.1:8053` (TCP and UDP) | 53 |
| Kerberos KDC | `127.0.0.1:8888` (TCP and UDP) | 88 |
| LDAP | `127.0.0.1:8389` (TCP) | 389 |
| LDAPS | `127.0.0.1:8636` (TCP) | 636 |
| SQL Server | `127.0.0.1:1433` (TCP) | 1433 |

Clients on `kerberos-net` still use `dc.example.local` (`172.20.0.10`) and `sql.example.local` (`172.20.0.20`) on their original container ports. Their DNS, profiles, and health checks do not depend on host port publication.

Run diagnostics on the container host (commands below assume this directory):

```bash
docker compose -f docker-compose-matrix.yml --profile ubuntu22 ps
docker compose -f docker-compose-matrix.yml port mssql 1433
dig @127.0.0.1 -p 8053 example.local
dig +tcp @127.0.0.1 -p 8053 example.local
docker compose -f docker-compose-matrix.yml --profile ubuntu22 exec client-ubuntu22 getent hosts dc.example.local sql.example.local
```

For host SQL diagnostics, use `127.0.0.1,1433` with the runtime-generated credentials. Keep Kerberos authentication tests inside the client containers using the domain hostnames, rather than a loopback address that does not match the SQL service principal.

#### Intentional Remote Access

Remote access requires an explicit, uncommitted local override and firewall rules limited to trusted sources. Prefer exposing only the service needed. For example, with Docker Compose **2.24.4 or later**, create `$HOME/kerberos-remote.yml` outside the checkout to expose SQL on a specific host IPv4 interface:

```yaml
services:
  mssql:
    ports: !override
      - "${KERBEROS_REMOTE_HOST:?Set KERBEROS_REMOTE_HOST to a trusted host IPv4 interface}:1433:1433"
```

Set `KERBEROS_REMOTE_HOST` to that interface's IPv4 address, then validate and apply the override explicitly:

```bash
docker compose -f docker-compose-matrix.yml -f "$HOME/kerberos-remote.yml" --profile ubuntu22 config --quiet
docker compose -f docker-compose-matrix.yml -f "$HOME/kerberos-remote.yml" --profile ubuntu22 up -d
```

`!override` replaces the port list; a plain additional `ports` list can merge with the original bindings instead. If DC access is also needed, add a `dc` service override with `ports: !override`, retaining all six mappings (`8053:53/tcp`, `8053:53/udp`, `8888:88/tcp`, `8888:88/udp`, `8389:389`, `8636:636`) and prefixing each with the chosen host IPv4 address. Do not change the checked-in defaults or bind to all interfaces as a workaround.

To restore loopback-only access, rerun the base Compose command without the override:

```bash
docker compose -f docker-compose-matrix.yml --profile ubuntu22 up -d
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
