# Cargo configuration

## For Developers (Default)

The checked-in `config.toml` uses crates.io without Azure DevOps authentication.
See the [repository setup](../README.md#getting-started) for build prerequisites.

```bash
git clone https://github.com/microsoft/mssql-rs.git
cd mssql-rs
cargo build
```

## For CI/Pipeline

Non-PR CI builds use `.cargo/config.ci.toml`, which configures:
- **Authenticated Azure Artifacts feeds** (`mssql-rs_Public` and `mssql-rs`)
- Source replacement: `crates.io` → `mssql-rs_Public` (ADO feed with crates.io as upstream)
- Authentication via `CargoAuthenticate@0` task (sets `CARGO_REGISTRIES_*_TOKEN` env vars)

The shared
[authentication template](../.pipeline/templates/cargo-authenticate-template.yml)
applies the CI config before `CargoAuthenticate@0`. Both steps are skipped for
pull requests, which use the public configuration. Do not copy an unconditional
authentication step into a PR job.

## Files

| File | Purpose |
|------|---------|
| `.cargo/config.toml` | **Default** - Uses crates.io (for developers) |
| `.cargo/config.ci.toml` | **CI override** - Uses authenticated ADO feeds |
| `.pipeline/scripts/apply-ci-cargo-config.sh` | Applies CI config (Linux/Mac) |
| `.pipeline/scripts/apply-ci-cargo-config.ps1` | Applies CI config (Windows) |

## Troubleshooting

For local authentication errors, compare `config.toml` with the checked-in
version: a previous CI setup may have replaced it with `config.ci.toml`.
Preserve local changes before restoring configuration. For non-PR CI failures,
check that config application precedes authentication.
