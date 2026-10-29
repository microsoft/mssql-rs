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

## Vectors

Connections request `VectorVersion::V2` by default, supporting float32 and float16
vectors when the server negotiates the corresponding feature version. Set
`ClientContext::vector_version` to `VectorVersion::V1` before connecting to opt out
of float16 support while retaining float32 support. A float16 RPC parameter,
including a typed NULL, requires negotiated V2; requesting V2 alone does not
guarantee server support.

`SqlVector::try_from_f32` creates float32 vectors (up to 1998 dimensions).
`SqlVector::try_from_f16` accepts `Vec<f32>` and tags it for float16 wire storage
(up to 3996 dimensions). Both types are held in memory as `f32` and exposed through
`as_f32()`; constructing a float16 vector does not immediately round its values.
Serialization narrows each element to IEEE 754 binary16 using `half::f16`, and
deserialization widens the received binary16 values to `f32`. Conversion is lossy:
round-to-nearest, ties-to-even can lose precision, tiny values can underflow, and
sufficiently large magnitudes can round to infinity. For example, `65505.0f32`
rounds to the finite value `65504.0`, not infinity. These conversion semantics do
not imply that SQL Server accepts non-finite vector elements.

Float16 live integration tests are ignored by default. Explicit opt-in requires nextest
`--run-ignored ignored-only` with the `test(vector16)` filter, `VECTOR16_TEST=1`, an explicit `DB_DATABASE`,
and `DB_HOST`/`DB_PORT` for a server with float16 support preprovisioned by an
administrator. The general integration helper defaults
`DB_DATABASE` to `master` when unset, but float16 tests reject that fallback.
When selected without `VECTOR16_TEST=1` they return early with a diagnostic; with opt-in they fail on bad
configuration. Tests never change database configuration or global flags.
See the [transport test guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/tests/TRANSPORT_TESTS_README.md#float16-vector-integration-tests)
for environment details and the `test(vector16)` nextest filter.

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
acknowledgement. Zero uses the transport default; failed cancellation settlement
retires the connection. Discarding rows during cancellation does not decode
values or resolve Always Encrypted keys.

## Runtime behavior

Healthy commands do not allocate a session-recovery future; recovery boxes it
only when needed. Connection setup boxes its action chain once per connection
to reduce caller stack use without a thread handoff.

String decoding follows column metadata, not BOM-shaped prefixes in values.
The [behavior guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md)
documents encoding and stack-use details.

## License

MIT
