# mssql-oledb

Windows-only OLE DB provider work backed by `mssql-tds`.

The initial slice adds connection-string handling and a synchronous session API
for connecting with SQL authentication, executing SQL, and fetching typed rows
incrementally from forward-only result sets. The crate is not yet a usable OLE
DB provider: COM class activation, OLE DB interfaces, property sets, rowsets and
accessor binding, and Windows integration tests remain to be implemented.

Connection properties currently supported by the execution core are `Data
Source` (or `Server`), `Initial Catalog` (or `Database`), `User ID`, `Password`,
`Encrypt`, and `TrustServerCertificate`. Integrated authentication and
parameterized commands are not supported in this initial slice.

## Validate

```bash
cargo test -p mssql-oledb --lib
cargo check -p mssql-oledb --target x86_64-pc-windows-msvc
```
