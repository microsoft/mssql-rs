# mssql-rs

SQL Server connectivity components built on a Rust implementation of the
[Tabular Data Stream (TDS)](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-tds/)
protocol. This repository contains the protocol library, native drivers and
bindings, command-line tools, and test infrastructure.

## Components

Start with the component documentation for installation, usage, and limitations:

| Component | Purpose |
|---|---|
| [mssql-tds](mssql-tds/README.md) | Async Rust TDS protocol client for SQL Server and Azure SQL Database |
| [mssql-odbc](mssql-odbc/README.md) | Alpha ODBC 3.x driver exposing the native ODBC C API over `mssql-tds` |
| [mssql-py-core](mssql-py-core/README.md) | PyO3 extension providing Rust TDS operations for [mssql-python](https://github.com/microsoft/mssql-python); not a standalone DB-API client |
| [mssql-js](mssql-js/README.md) | Experimental Node.js bindings to `mssql-tds`, with a TypeScript wrapper |
| [mssql-sqlcmd](mssql-sqlcmd/README.md) | Rust static library exposing a C ABI for native sqlcmd integration; not a standalone executable |

For testing and development:

| Component | Purpose |
|---|---|
| [mssql-mock-tds](mssql-mock-tds/README.md) | Mock TDS server returning configured responses to test clients without SQL Server |
| [mssql-mock-tds-py](mssql-mock-tds-py/README.md) | Python bindings to the mock TDS server |
| [mssql-tds-cli](mssql-tds-cli/README.md) | Interactive T-SQL development demo using `mssql-tds` |
| [mssql-tds-bench](mssql-tds-bench/README.md) | Criterion harness comparing `mssql-tds` revisions against fixed workloads |
| [mssql-odbc-bench](mssql-odbc-bench/README.md) | C++ harness comparing ODBC drivers through the platform Driver Manager |

## Getting Started

The following instructions build from source. Package users should follow the
component links above.

### Prerequisites

- [Rust](https://rustup.rs/) (version specified in `rust-toolchain.toml`)
- A platform C/C++ toolchain and linker. On Linux, also install OpenSSL development
  libraries and `pkg-config` (for example, `build-essential libssl-dev pkg-config`
  on Debian/Ubuntu).
- macOS workspace builds also require OpenSSL development libraries for
  `mssql-mock-tds`, even though the TDS client uses platform TLS by default.
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
