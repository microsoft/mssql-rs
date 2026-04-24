# mssql-rs

A Rust implementation of the [Tabular Data Stream (TDS)](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-tds/) protocol used by Microsoft SQL Server. This library provides a foundational protocol layer to power SQL connectivity across multiple language bindings.

## Overview

The `mssql-tds` crate implements the TDS protocol from the ground up in Rust, providing a high-performance, memory-safe core that can be shared across driver ecosystems. The project is organized as a Cargo workspace:

| Crate | Purpose |
|---|---|
| `mssql-tds` | Core TDS protocol library |
| `mssql-js` | (Experimental) Node.js bindings via NAPI-RS |
| `mssql-tds-cli` | Interactive CLI client tool |
| `mssql-mock-tds` | Mock TDS server for testing |

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (version specified in `rust-toolchain.toml`)
- A C linker and OpenSSL development headers (e.g., `build-essential`, `libssl-dev` on Debian/Ubuntu)
- [cargo-nextest](https://nexte.st/) for running tests: `cargo install cargo-nextest --locked`
- Docker (optional, for running a local SQL Server instance)

### Clone and Build

```bash
git clone <repo-url>
cd mssql-rs
cargo build
```

### Git Hooks Setup

Install pre-commit hooks that run formatting and lint checks automatically:

```bash
./dev/setup-hooks.sh
```

## Local Build

Build the workspace:

```bash
cargo build
```

Before submitting changes, run the full check suite:

```bash
./scripts/bfmt.sh  or scripts/bfmt.ps1     # Format check
./scripts/bclippy.sh    # Lint (warnings are errors), for windows scripts/bclippy.ps1
cargo btest      # Test with cargo-nextest
```

These aliases are defined in `.cargo/config.toml`. The `mssql-py-core` crate is excluded from the workspace and requires separate fmt/clippy runs via the scripts in `scripts/`.

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
