# mssql-sqlcmd

sqlcmd components in Rust. Native (ODBC) sqlcmd links this crate as a static
library and calls it through a C ABI; native sqlcmd still parses the command
line, connects and runs the batches.

## Layout

| Path | Purpose |
|---|---|
| `src/formatter/` | Output formatters. `json.rs` renders `--format json`. |
| `src/ffi.rs` | The C ABI native sqlcmd calls. |
| `include/mssql_sqlcmd.h` | C/C++ declarations for that ABI. |

## JSON output

Native sqlcmd hands each piece of output to a JSON document instead of printing
it — result sets and rows, row counts, server messages — and renders the
document once, at exit, with the connection details and exit code:

```json
{
  "sqlcmd": { "version": "18.5.1.1" },
  "connection": { "server": "localhost", "database": "master", "authentication": "SqlPassword", "encrypt": true },
  "exitCode": 0,
  "output": [
    { "type": "resultSet", "columns": ["id"], "rows": [["1"], [null]] },
    { "type": "rowsAffected", "count": 2 },
    { "type": "error", "number": 50000, "state": 1, "severity": 16, "message": "boom" }
  ]
}
```

Values are strings, as sqlcmd would print them; SQL `NULL` is JSON `null`.

## Building for native sqlcmd

```text
cargo build -p mssql-sqlcmd --release
```

produces `target/release/mssql_sqlcmd.lib` (Windows) or `libmssql_sqlcmd.a`.
The library has no dependencies beyond the Rust standard library; the system
libraries it needs at link time are listed by:

```text
cargo rustc -p mssql-sqlcmd --release --lib -- --print native-static-libs
```
