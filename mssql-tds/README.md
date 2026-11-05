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

These examples target version 0.4. If that version is not yet available on
crates.io, [use a source checkout](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/client-guide.md#using-a-source-checkout)
instead of the registry dependency below.

```toml
[dependencies]
mssql-tds = "0.4"
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

By default, connections use Strict encryption (TDS 8.0), which requires a
TDS-8-capable server and a trusted certificate matching the hostname.
For servers that require TDS 7.4, set
`context.encryption_options.mode = mssql_tds::core::EncryptionSetting::On`
before connecting; certificate validation remains enabled.

`execute` accepts `()` for defaults or `ExecuteOptions` for timeout,
cancellation, and other per-command settings. Use `advance()` for subsequent
results in a batch. See the [API reference](https://docs.rs/mssql-tds).

## Feature flags

| Feature | Default | Purpose |
|---|---|---|
| `integrated-auth` | On | Enables `sspi` (Windows) and `gssapi` (Unix Kerberos). |
| `tls-schannel-direct-on-windows` | On | Enables `tls-schannel-direct` for Windows Schannel TLS. |
| `tls-network-framework-on-macos` | On | Enables `tls-network-framework` for macOS Strict TCP connections. |

Set `default-features = false` to omit integrated authentication and use
`native-tls` on all platforms. Individual auth and TLS features can be enabled
separately. Linux builds require OpenSSL development libraries; Unix integrated
authentication loads GSSAPI at runtime.

The default Network.framework feature requires a macOS deployment target of
10.15 or later. For x86_64 macOS builds, set `MACOSX_DEPLOYMENT_TARGET=10.15`
(or later) for the entire build; lower targets fail at build time.
Apple Silicon's default deployment target already meets this requirement.
Disabling `tls-network-framework` removes this added minimum.

## Documentation

- [API reference](https://docs.rs/mssql-tds) for public types and methods.
- [Client guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/client-guide.md)
  for request timeouts, response cleanup, vectors, and LOB parameters.
- [Connection and decoding behavior](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md)
  for connection policy, TLS, DNS caching, encoding, and runtime details.

## License

MIT
