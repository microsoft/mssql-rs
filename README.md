# mssql-rs

SQL Server connectivity components built on a Rust implementation of the
[Tabular Data Stream (TDS)](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-tds/)
protocol. This repository contains the protocol library, native drivers and
bindings, command-line tools, and test infrastructure.

## Components

Start with the component documentation for installation, usage, and limitations:

| Component | Intended use |
|---|---|
| [mssql-tds](mssql-tds/README.md) | Low-level async Rust API for SQL Server and Azure SQL Database |
| [mssql-odbc](mssql-odbc/README.md) | ODBC 3.x driver; see its implementation status and compatibility limitations |
| [mssql-python-rs](mssql-py-core/README.md) | Native Python runtime used by [mssql-python](https://github.com/microsoft/mssql-python), not a standalone DB-API client |
| [mssql-js](mssql-js/README.md) | Experimental Node.js bindings |
| [mssql-sqlcmd](mssql-sqlcmd/README.md) | SQLCMD-compatible command-line tool |

For testing and development, see the [Rust mock server](mssql-mock-tds/README.md),
[Python mock bindings](mssql-mock-tds-py/README.md),
[interactive demo](mssql-tds-cli/README.md), and
[TDS](mssql-tds-bench/README.md) / [ODBC](mssql-odbc-bench/README.md) benchmark harnesses.

## Getting Started

The following instructions build from source. Package users should follow the
component links above.

### Prerequisites

- [Rust](https://rustup.rs/) (version specified in `rust-toolchain.toml`)
- A platform C/C++ toolchain and linker. On Linux, also install OpenSSL development
  libraries and `pkg-config` (for example, `build-essential libssl-dev pkg-config`
  on Debian/Ubuntu).
- On x86_64 macOS, set `MACOSX_DEPLOYMENT_TARGET=10.15` or later for the default
  TLS features; see [TLS on macOS](mssql-tds/docs/connection-behavior.md#tls-on-macos).
- [cargo-nextest](https://nexte.st/) and
  [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) for `cargo btest`:
  `cargo install cargo-nextest cargo-llvm-cov --locked`, then
  `rustup component add llvm-tools-preview`.
- Docker (optional, for running a local SQL Server instance)

### Clone and Build

```bash
git clone https://github.com/microsoft/mssql-rs.git
cd mssql-rs
cargo build --workspace
```

The Cargo workspace excludes `mssql-py-core` and `mssql-mock-tds-py`; their
READMEs cover separate Python builds. Component-specific dependencies and test
setup are documented alongside each component.

### Local Checks

```bash
cargo bfmt       # Format check
cargo bclippy    # Lint (warnings are errors)
cargo btest      # Tests with coverage via nextest
```

These workspace aliases are defined in [.cargo/config.toml](.cargo/config.toml).
Use `scripts/bfmt.ps1` and `scripts/bclippy.ps1` on Windows, or their `.sh`
counterparts, to also check `mssql-py-core`. Integration tests require a configured
SQL Server; see the [TDS test setup](mssql-tds/tests/TRANSPORT_TESTS_README.md).

For a regular clone, `bash dev/setup-hooks.sh` installs the optional formatting
hook. It formats the workspace and `mssql-py-core`; it does not run lint or tests.
The installer expects a `.git` directory and does not support linked worktrees.

## Contributing

This project is not currently accepting external pull requests. See [CONTRIBUTING.md](CONTRIBUTING.md) for details.

Bug reports and feature requests are welcome through [GitHub Issues](https://github.com/microsoft/mssql-rs/issues).

## Support

See [SUPPORT.md](SUPPORT.md) for information on how to get help.

## Security

Please report security vulnerabilities through the Microsoft Security Response Center (MSRC). See [SECURITY.md](SECURITY.md) for details.

## Code of Conduct

This project has adopted the [Microsoft Open Source Code of Conduct](https://opensource.microsoft.com/codeofconduct/). See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for more information.

## License

This project is licensed under the [MIT License](LICENSE).
