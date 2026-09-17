# mssql-tds-cli

A command-line interface (CLI) tool for interacting with Microsoft SQL Server using the [mssql-tds](../mssql-tds) crate.

## Overview

`mssql-tds-cli` is a demonstration CLI built on top of the `mssql-tds` Rust crate, which provides a native implementation of the Tabular Data Stream (TDS) protocol. This tool allows you to connect to SQL Server instances, execute queries, and interact with databases directly from your terminal.

## Features

- Connect to Microsoft SQL Server using the TDS protocol
- Execute T-SQL queries and scripts
- View query results in the terminal
- Demonstrates the capabilities and usage of the `mssql-tds` library

## Usage

```
cargo run --bin mssql-tds-cli
```

Or, if installed:

```
mssql-tds-cli
```

**Note:**
The CLI does not parse command-line options or read a configuration file.
It connects to `tcp:localhost,1433` as `sa`, uses the `master` database, and
reads the password from `/tmp/password`.

## Why use this CLI?

- To test and debug the `mssql-tds` protocol implementation
- As a reference for building your own TDS-based tools in Rust
- For quick, scriptable access to SQL Server from the command line (with future enhancements)

## Project Structure

- `src/`: CLI source code
- `Cargo.toml`: Crate manifest
- Depends on: [`mssql-tds`](../mssql-tds)

## License

This project is licensed under the MIT License.
