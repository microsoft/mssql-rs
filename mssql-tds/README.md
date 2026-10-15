# mssql-tds

Async Rust implementation of the TDS (Tabular Data Stream) protocol for SQL Server
and Azure SQL Database.

## What it does

Low-level TDS client handling connection negotiation (prelogin, TLS, login7),
query execution, result set streaming, bulk copy, RPC calls, and transaction
management. Built on Tokio.

## Execution stack usage

Command execution keeps session-recovery state in a separately boxed future,
allocated only when a dead connection needs recovery. Healthy commands do not
pay for that allocation or carry the login state inline. This reduces caller
stack use for synchronous adapters such as ODBC on musl's small worker stacks,
without moving execution to another thread or changing cancellation and timeout
handling.

Connection establishment boxes the action-chain future once per connection.
This keeps transport attempts, TLS/login, and client construction out of the
caller's future, leaving room for the Driver Manager and native TLS on small
application stacks. The connection is still polled on the calling thread with
the same cancellation and login deadline; no worker-thread handoff is introduced.

## SQL string decoding

`SqlString::decode` and `to_utf8_string` use the encoding supplied by TDS metadata,
not a byte-order mark (BOM) inferred from the value. Leading BOM-shaped bytes are
data: UTF-16LE `FF FE` remains U+FEFF, `FE FF` remains U+FFFE without switching to
big-endian decoding, and Windows-1252 `EF BB BF` remains U+00EF U+00BB U+00BF.

This behavior also applies to decoded column strings in the `mssql-js` row writer
and the `mssql-py-core` cursor, not just ODBC. Applications using those bindings
can therefore receive leading characters that were previously stripped or decoded
under the wrong encoding. Malformed-sequence handling is unchanged; preserving
raw unpaired UTF-16 units in ODBC `SQL_C_WCHAR` buffers is a separate behavior.

## Feature flags

| Flag | Default | Description |
|------|---------|-------------|
| `integrated-auth` | **yes** | Enables both `sspi` and `gssapi` |
| `sspi` | via `integrated-auth` | Windows SSPI (Kerberos/NTLM) |
| `gssapi` | via `integrated-auth` | Unix GSSAPI (Kerberos) via runtime `dlopen` |

Disable the default to drop platform-specific auth dependencies:

```toml
mssql-tds = { version = "0.1", default-features = false }
```

## Quick start

```rust,no_run
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection::tds_client::ResultSetClient;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::TdsResult;

#[tokio::main]
async fn main() -> TdsResult<()> {
    let mut context = ClientContext::default();
    context.user_name = std::env::var("DB_USER").unwrap_or("<user>".into());
    context.password = std::env::var("DB_PASSWORD").unwrap_or("<password>".into());
    context.database = "master".into();

    let provider = TdsConnectionProvider {};
    let mut client = provider
        .create_client(context, "tcp:localhost,1433", None)
        .await?;

    client
        .execute("SELECT 1 AS value".into(), None, None)
        .await?;

    if let Some(rs) = client.get_current_resultset() {
        while let Some(row) = rs.next_row().await? {
            println!("{row:?}");
        }
    }

    client.close_query().await?;
    Ok(())
}
```

## License

MIT
