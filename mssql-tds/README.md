# mssql-tds

Async Rust implementation of the TDS (Tabular Data Stream) protocol for SQL Server
and Azure SQL Database.

## What it does

Low-level TDS client handling connection negotiation (prelogin, TLS, login7),
query execution, result set streaming, bulk copy, RPC calls, and transaction
management. Built on Tokio.

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
See [tests/TRANSPORT_TESTS_README.md](tests/TRANSPORT_TESTS_README.md#float16-vector-integration-tests)
for environment details and the `test(vector16)` nextest filter.

## License

MIT
