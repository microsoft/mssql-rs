<!--
Rendered as the mssql-mock-tds README on crates.io (auto-detected by Cargo).
Changes appear with the next crate release. Keep content user-facing and links
absolute. The docs.rs API reference is generated from Rust source separately.
-->

# mssql-mock-tds

A library and CLI for testing SQL Server clients without a SQL Server instance.
Returns configured TDS responses; it does not execute SQL or validate database
semantics. Use only in test environments, not as a database or authentication
service.

## Features

- **Connection setup:** PreLogin and Login7, with database, collation, and
  4096-byte packet-size ENVCHANGE responses.
- **TLS:** optional TLS for TDS 7.4 and strict TLS for TDS 8.0.
- **Authentication testing:** username/password and FedAuth login flows,
  including received-token inspection; credentials are not validated.
- **Custom results:** register query responses with column metadata and rows.
- **Data types:** `tinyint`, `smallint`, `int`, `bigint`, `nvarchar`, and chunked
  `nvarchar(max)`, `varchar(max)`, and `varbinary(max)` values, including NULLs.
- **Fault injection:** response delays, informational messages, errors, and
  raw data-classification tokens through the library API.
- **Redirection:** plain and enhanced login routing through the library API.
- **Test lifecycle:** concurrent connections, connection inspection, and
  shutdown signalling.

## CLI

```sh
cargo install mssql-mock-tds --locked
mssql-mock-tds --port 1433
```

The default listener is `127.0.0.1:1433`, without TLS. For TLS, supply a
certificate and key:

```sh
mssql-mock-tds --tls-mode optional --cert cert.pem --key key.pem
```

Use `--tls-mode strict` for TDS 8.0, or `--pfx identity.pfx` and
`--pfx-password` instead of PEM files. Windows requires PKCS#12 (`--pfx`).
Run `mssql-mock-tds --help` for all options.

### CLI options

| Option | Purpose | Default |
|---|---|---|
| `-H, --host <HOST>` | Listener address | `127.0.0.1` |
| `-p, --port <PORT>` | Listener port; `0` selects an available port | `1433` |
| `-m, --tls-mode <TLS_MODE>` | `none`, `optional` (TDS 7.4), or `strict` (TDS 8.0) | `none` |
| `-c, --cert <CERT>` | PEM certificate; requires `--key`, non-Windows only | Unset |
| `-k, --key <KEY>` | PEM private key; requires `--cert`, non-Windows only | Unset |
| `--pfx <PFX>` | PKCS#12 identity instead of PEM files; takes precedence if both are supplied | Unset |
| `--pfx-password <PFX_PASSWORD>` | PKCS#12 password | Empty string |
| `--redirect-host <REDIRECT_HOST>` | Login redirection target (without TLS only) | Unset |
| `--redirect-port <REDIRECT_PORT>` | Target port when `--redirect-host` is set | `1433` |
| `-v, --verbose...` | `-v` for debug logging, `-vv` for trace | Info |
| `-h, --help` | Show help | - |

TLS modes require either `--pfx` or both `--cert` and `--key`. The CLI ignores
redirection options in TLS modes. `RUST_LOG`, when set to a valid filter,
overrides the verbosity flags.

## Library

Add these dependencies:

```toml
[dependencies]
mssql-mock-tds = "0.3"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### Custom query responses

Bind an ephemeral port and register a response before starting the server:

```rust,no_run
use mssql_mock_tds::{
    ColumnDefinition, ColumnValue, MockTdsServer, QueryResponse, Row, SqlDataType,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockTdsServer::new("127.0.0.1:0").await?;
    server.query_registry().lock().await.register(
        "SELECT id FROM example",
        QueryResponse::new(
            vec![ColumnDefinition::new("id", SqlDataType::Int)],
            vec![Row::new(vec![ColumnValue::Int(42)])],
        ),
    );
    println!("Connect your test client to {}", server.local_addr());
    server.run().await?;
    Ok(())
}
```

The registry includes `SELECT 1` and `SELECT CAST(1 AS BIGINT), 2, 3` by default.
Registering the same query again replaces its response.

Use `QueryResponse::with_delay` with `TM_BEGIN_DELAY_KEY`, `TM_COMMIT_DELAY_KEY`,
or `TM_ROLLBACK_DELAY_KEY` to delay transaction responses. `ATTENTION_DELAY_KEY`
delays cancellation acknowledgements, including those interrupting a delayed
query, so tests can exercise unresponsive-peer cleanup.

### Supported data types

| SQL type | `SqlDataType` | `ColumnValue` payload |
|---|---|---|
| Integer types | `TinyInt`, `SmallInt`, `Int`, `BigInt` | Corresponding variant with `u8`, `i16`, `i32`, or `i64` |
| `nvarchar` | `NVarChar` | `NVarChar(String)` |
| `nvarchar(max)` | `NVarCharMax` | `NVarCharMax(Vec<Vec<u16>>)`; raw UTF-16 chunks |
| `varchar(max)` | `VarCharMax` | `VarCharMax(Vec<Vec<u8>>)`; bytes in the column's collation |
| `varbinary(max)` | `VarBinaryMax` | `VarBinaryMax(Vec<Vec<u8>>)`; opaque bytes |

Use `ColumnValue::Null` for integer NULLs and `NVarCharMaxNull` for
`nvarchar(max)` NULLs. Other string/binary NULL variants are not exposed.
Match row values to column definitions; the mock serializes fixtures rather
than validating their schema. PLP chunks must be nonempty; an empty chunk list
represents an empty value.

### Testing with TdsClient and controlled shutdown

For this example, also add `mssql-tds = "0.3"` under `[dev-dependencies]` and
place the test in your project's `tests/` directory. It uses the default
`SELECT 1` response and an unencrypted loopback connection:

```rust,no_run
use mssql_mock_tds::MockTdsServer;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection::tds_client::ResultSet;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::EncryptionSetting;
use tokio::sync::oneshot;

#[tokio::test]
async fn query_mock_server() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockTdsServer::new("127.0.0.1:0").await?;
    let addr = server.local_addr();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(server.run_with_shutdown(shutdown_rx));

    let result: mssql_tds::core::TdsResult<()> = async {
        let mut context = ClientContext::default();
        context.user_name = "test-user".into();
        context.password = "test-only".into();
        context.encryption_options.mode = EncryptionSetting::PreferOff;

        let provider = TdsConnectionProvider {};
        let datasource = format!("tcp:{},{}", addr.ip(), addr.port());
        let mut client = provider.create_client(context, &datasource, None).await?;
        client.execute("SELECT 1".into(), ()).await?;
        assert!(client.on_rows());
        assert!(client.next_row().await?.is_some());
        assert!(client.next_row().await?.is_none());
        client.close_query().await?;
        client.close_connection().await?;
        Ok(())
    }
    .await;

    shutdown_tx
        .send(())
        .map_err(|_| "mock server stopped early")?;
    task.await??;
    result?;
    Ok(())
}
```

The listener is bound before spawning, so no startup sleep is needed.
`run_with_shutdown` stops accepting connections; it does not join existing
connection tasks. Close test clients before signalling shutdown.

### TLS in library tests

Load a PKCS#12 identity on any supported platform, then choose the TLS constructor:

```rust,no_run
use mssql_mock_tds::{MockTdsServer, load_identity_from_file};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let password = std::env::var("TEST_PFX_PASSWORD")?;
    let identity = load_identity_from_file("identity.pfx", &password)?;
    let server = MockTdsServer::new_with_strict_tls("127.0.0.1:1433", identity).await?;
    server.run().await?;
    Ok(())
}
```

For TDS 7.4, use `new_with_tls(addr, Some(identity))`; passing `None` disables
TLS. Non-Windows tests can also use `create_test_identity(cert_pem, key_pem)`.
Configure the client to trust the test certificate and validate its hostname;
do not carry test-only trust bypasses into production.

### Delays and injected errors

Register a delayed error to exercise client timeout or error handling:

```rust,no_run
use mssql_mock_tds::{MockTdsServer, QueryResponse, TerminalError};
use std::time::Duration;

async fn configure(server: &MockTdsServer) {
    server.query_registry().lock().await.register(
        "SELECT blocked",
        QueryResponse::error_only(TerminalError::new(1222, 16, "Lock request timed out."))
            .with_delay(Duration::from_millis(250)),
    );
}
```

Use `with_info_tokens` for messages before rows, `with_leading_error` for an
error followed by a result set, and `register_with_trailing_info` for messages
after rows. `LOGIN_DELAY_KEY`, `RPC_DELAY_KEY`, and `TM_BEGIN_DELAY_KEY` delay
login, unmatched RPCs, and transaction-begin replies respectively.

## Current limitations

| Area | Behavior |
|---|---|
| SQL execution | No SQL engine or persistent data. Batch matching is case-insensitive and exact after parsing; no regex or wildcard matching. |
| Unregistered requests | Unmatched SQL batches and RPCs return an empty DONE response, not an unknown-query error. Register expected responses explicitly. |
| RPC | Matches registered text as uppercase UTF-16LE bytes within the raw request; send uppercase SQL on this path. No general parameter decoder, stored-procedure execution, or cursor engine. |
| Transactions | Begin/commit/rollback messages are acknowledged; isolation and data changes are not enforced. |
| Types and metadata | Only the types above are exposed; metadata is simplified, not a full SQL Server schema model. |
| Packet sizes | Responses are not fragmented. Keep each complete response within the advertised 4096-byte packet size, including the 8-byte header and all tokens; clients such as `TdsClient` reject larger packets. |
| Authentication | Login flows and token inspection are test aids, not credential verification. |
| TLS redirection | CLI redirection is ignored in TLS modes; public constructors do not combine TLS with redirection. |

## Development

From the repository root:

```sh
cargo nextest run --frozen -p mssql-mock-tds --lib
cargo nextest run --frozen -p mssql-tds --test test_mock_server
```

TLS tests in this integration target and `test_mock_server_tls` require
[test certificates](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/tests/test_certificates/README.md).
No live SQL Server is needed for the mock-server tests.

| Module | Responsibility |
|---|---|
| `server` | Connections, login flows, routing, and lifecycle |
| `protocol` | TDS packets and token encoding/decoding |
| `query_response` | Typed fixtures, query registry, and injection controls |
| `tls_helper`, `tds_tls_wrapper` | Certificate loading and TDS-wrapped TLS |

## Future enhancements

Potential extensions, not a committed release roadmap:

- Additional SQL types and NULL encodings, including decimal and date/time.
- SQL-aware query matching and general RPC parameter decoding.
- Transaction and cursor simulation beyond protocol acknowledgements.
- Multi-packet response encoding and TLS-enabled redirection.

See the [API reference](https://docs.rs/mssql-mock-tds) for response types,
TLS constructors, and test controls.

## License

MIT
