# mssql-tds

Async Rust implementation of the TDS (Tabular Data Stream) protocol for SQL Server
and Azure SQL Database.

## What it does

Low-level TDS client handling connection negotiation (prelogin, TLS, login7),
query execution, result set streaming, bulk copy, RPC calls, and transaction
management. Built on Tokio.

## DNS fallback caching

LOGIN7 advertises DNS caching (`0x0B`). Each TCP connection keeps its resolved
addresses until login completes with LOGINACK and a successful DONE. The last
FEATUREEXTACK then decides whether to publish those addresses in the process-wide
fallback cache: DNS bit 0 allows them; a missing or negative DNS acknowledgement
removes the host. A malformed FEATUREEXTACK revokes caching after that accepted
response is drained, then returns its parsing error.

Responses without FEATUREEXTACK leave an earlier allowance unchanged, including
gateway redirects. Rejected, truncated, and authentication-continuation responses
also leave it unchanged, so a failed retry cannot discard its own fallback.
Pending resolutions belong to their connection, not a shared host-limited map.
Concurrent accepted verdicts take effect in completion order and publish each
connection's own addresses. Cached IPv6 addresses retain their scope IDs.

Every connect still attempts DNS first. Fallback is used only if the resolver
returns an error before the connection deadline, including a resolver-reported
timeout. A resolver that remains pending until the caller's deadline expires
does **not** get extra time for a cached TCP attempt. Reserving an earlier DNS
sub-deadline for fallback is not implemented; caching does not guarantee that
every form of DNS outage can be recovered within the login timeout.

The deliberate differences from msodbcsql's eager cache are recorded in the
[ODBC parity registry](../mssql-odbc/docs/parity-deviations.md).

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
| `tls-schannel-direct-on-windows` | **yes** | Enables `tls-schannel-direct` |
| `tls-schannel-direct` | via `tls-schannel-direct-on-windows` | Windows: TLS through the in-tree Schannel engine instead of `native-tls`. No effect on other targets. |

Disable the defaults to drop platform-specific auth dependencies and use
`native-tls` on every platform:

```toml
mssql-tds = { version = "0.3", default-features = false }
```

## Quick start

```rust,no_run
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection::tds_client::ResultSet;
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

`execute` takes `()` for default options, or an `ExecuteOptions` to set a
per-command timeout, cancellation handle, Always Encrypted override, or
statement-error mode. It returns a `StatementResult` for the first result in
the batch; use `advance()` to move through the rest of a multi-statement batch.

## Cancelling an outstanding response

Use `client.cancel_query(timeout_secs).await` to abandon remaining server work
after execution has returned a result. Unlike `close_query`, which drains
normally, this sends ATTENTION and waits for acknowledgement before reuse,
including when a row or PLP value is partially read. The timeout bounds
cancellation settlement; zero uses the transport default. Failed settlement
returns an error and retires the connection.
Discarded rows are consumed without resolving Always Encrypted keys or decoding
column values, so an external key-store provider cannot delay cancellation.

## License

MIT
