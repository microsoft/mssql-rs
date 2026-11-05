# mssql-tds-cli

An interactive development demo for the
[`mssql-tds`](https://github.com/microsoft/mssql-rs/tree/main/mssql-tds)
crate, not a replacement for sqlcmd. This crate is not published.

## Prerequisites

The demo's connection settings are fixed in `src/main.rs`:

- SQL Server at `tcp:localhost,1433`, with login `sa` and database `master`.
- The password read from `/tmp/password`, with surrounding whitespace trimmed.
  The file must exist before startup.
- Encryption enabled, with server-certificate validation bypassed. Use only
  against a local test server.

There are no command-line options for changing the server or credentials.

## Usage

From the repository root:

```sh
cargo run -p mssql-tds-cli
```

Enter a complete T-SQL batch on one line. The demo prints results using Rust
debug formatting. Type `exit`, send EOF, or press Ctrl+C to quit.

### Options

| Option | Behavior |
|---|---|
| `--multi-subnet-failover <true\|false>` | Override the client's multi-subnet failover setting; `false` disables parallel TCP connection attempts. |
| `-c, --config-file-path <PATH>` | Accepted but unused; no configuration file is read. |
| `-h, --help` | Show help. |
| `-V, --version` | Show version. |

For example, to disable parallel TCP connection attempts:

```sh
cargo run -p mssql-tds-cli -- --multi-subnet-failover false
```

## License

This project is licensed under the MIT License.
