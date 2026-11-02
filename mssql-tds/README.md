<!--
Rendered as the mssql-tds README on crates.io via `readme` in Cargo.toml.
Changes appear with the next crate release. Keep content user-facing and links
absolute. The docs.rs API reference is generated from Rust source separately.
-->

# mssql-tds

Low-level, async TDS client for SQL Server and Azure SQL Database, built on
Tokio. Supports query and result-set streaming, RPC, bulk copy, transactions,
TLS, and integrated authentication.

## Quick start

```toml
[dependencies]
mssql-tds = "0.3"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Set `DB_USER` and `DB_PASSWORD` for your SQL Server, then:

```rust,no_run
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection::tds_client::ResultSet;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut context = ClientContext::default();
    context.user_name = std::env::var("DB_USER")?;
    context.password = std::env::var("DB_PASSWORD")?;
    context.database = "master".into();

    let provider = TdsConnectionProvider {};
    let mut client = provider
        .create_client(context, "tcp:localhost,1433", None)
        .await?;

    client.execute("SELECT 1 AS value".into(), ()).await?;
    if client.on_rows() {
        while let Some(row) = client.next_row().await? {
            println!("{row:?}");
        }
    }
    client.close_query().await?;
    Ok(())
}
```

The server certificate must be trusted and match the hostname.
`execute` accepts `()` for defaults or `ExecuteOptions` for timeout,
cancellation, and other per-command settings. Use `advance()` for subsequent
results in a batch. See the [API reference](https://docs.rs/mssql-tds).

## Connection policy and migration

See the [connection behavior guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md)
for connection defaults, MultiSubnetFailover settings, transport limitations,
migration, timeouts, and DNS caching.

## Feature flags

| Feature | Default | Purpose |
|---|---|---|
| `integrated-auth` | On | Enables `sspi` (Windows) and `gssapi` (Unix Kerberos). |
| `tls-schannel-direct-on-windows` | On | Enables `tls-schannel-direct`: the in-tree Windows Schannel TLS engine. |

Set `default-features = false` to omit integrated authentication and use
`native-tls` on all platforms. Individual auth and TLS features can be enabled
separately. Linux builds require OpenSSL development libraries; Unix integrated
authentication loads GSSAPI at runtime.

## Response lifecycle

Drain a response with `close_query()` before reuse, or call
`cancel_query(timeout_secs)` to abandon it and wait for cancellation
acknowledgement. `abandon_query(timeout_secs)` abandons it the way an ODBC
statement close does: unread messages and statement errors are discarded, and
ATTENTION is skipped when the response has already fully arrived. Zero uses the
transport default; failed cancellation settlement retires the connection.
Discarding rows during cancellation does not decode values or resolve Always
Encrypted keys.

## Runtime behavior

Healthy commands do not allocate a session-recovery future; recovery boxes it
only when needed. Connection setup boxes its action chain once per connection
to reduce caller stack use without a thread handoff.

String decoding follows column metadata, not BOM-shaped prefixes in values.
The [behavior guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md)
documents encoding and stack-use details.

## Legacy LOB parameters

`SqlType::Text`, `SqlType::NText`, and `SqlType::Image` RPC parameters accept
materialized, empty, and NULL values. A streamed value can keep its legacy
declaration through `RpcParameter::with_streamed_declaration` while sending a
MAX/PLP payload, without buffering the whole value.

## License

MIT
