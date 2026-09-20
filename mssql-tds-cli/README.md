# mssql-tds-cli

`sqlcmd` — a Rust implementation of the SQL Server command line tool, built on
the `mssql-tds` crate. It speaks TDS directly, so there is no ODBC driver to
install.

It answers to two command lines at once:

- the option grammar of the shipped ODBC `sqlcmd` (`-S`, `-U`, `-Q`, `-i`, …)
- go-sqlcmd's subcommand CLI (`sqlcmd config`, `sqlcmd create mssql`, …) and its
  long-only options (`--vertical`, `--format`, `--authentication-method`, …)

## Build and run

```bash
cargo run -p mssql-tds-cli --bin sqlcmd -- -S localhost -U sa -P '<password>' -C -Q "SELECT 1"
```

## Compatibility

Where the two reference tools disagree — row-count wording, some column widths,
float and GUID rendering — `--compat` picks whose behaviour to follow:

```bash
sqlcmd --compat go  -Q "SELECT 1"     # go-sqlcmd behaviour
sqlcmd --compat odbc -Q "SELECT 1"    # ODBC sqlcmd behaviour (default)
```

`SQLCMDCOMPAT` sets the same thing from the environment; the flag wins.

To ship a build that follows go-sqlcmd without anyone passing the flag:

```bash
cargo build -p mssql-tds-cli --features compat-go
```

`--compat` and `SQLCMDCOMPAT` still override that at run time.

## Library

The crate also builds as a library with a small C ABI (`sqlcmd_modern_claims`,
`sqlcmd_modern_main`), so the same implementation can be linked into the native
ODBC `sqlcmd` and handle the modern command lines there.
