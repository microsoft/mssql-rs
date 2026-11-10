# mssql-rs

Microsoft Rust Driver for SQL Server with a Tiberius-style API, built on [`mssql-tds`](https://crates.io/crates/mssql-tds).

This crate is based on [saurabh500/mssql-tiberius-bridge](https://github.com/saurabh500/mssql-tiberius-bridge/tree/d805ab9295ea8080f4c62e4bcebab0f6b4aad998), distributed under the MIT license.

## Why?

[tiberius](https://github.com/prisma/tiberius) is the most popular Rust TDS driver, but it's community-maintained. Microsoft's `mssql-tds` is the official, supported implementation — but it has a different API surface.

**mssql-rs** gives you the tiberius API you know on top of the mssql-tds engine:

- `row.get::<T, _>("column_name")` — named and indexed column access
- `stream.into_first_result()` — collect results into `Vec<Row>`
- `stream.into_row_stream()` — `Stream<Item = Result<Row>>` over a buffered `QueryResult` (rows pre-buffered)
- `client.query_streamed(sql, params)` / `simple_query_streamed(sql)` — true wire-level row streaming for memory-bounded large result sets
- `client.ping()` — cached connection-health check without SQL or network I/O
- `client.reset_session()` — native TDS session reset with `READ COMMITTED` isolation
- `conn.query(sql, &[&param])` — positional `@P1, @P2` parameters
- `Config::new().host().port().trust_cert()` — fluent builder
- `Config::trust_cert_ca("server.pem")` — pin the server's leaf certificate
- `AuthMethod::aad_token(jwt)` — Microsoft Entra ID / AAD federated auth
- deadpool connection pooling with native session reset and validation before reuse

## Quick Start

Requires Rust 1.95 or newer and uses edition 2024.

Add the driver and Tokio to your Cargo dependencies:

```sh
cargo add mssql-rs
cargo add tokio --features macros,rt-multi-thread
```

```rust
use mssql_rs::{Config, AuthMethod, Client};

#[tokio::main]
async fn main() -> mssql_rs::Result<()> {
    let mut cfg = Config::new();
    cfg.host("localhost")
       .port(1433)
       .database("master")
       .authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
       .trust_cert();

    let mut client = Client::connect(&cfg).await?;

    let rows = client
        .simple_query("SELECT name FROM sys.databases")
        .await?
        .into_first_result();

    for row in rows {
        let name: String = row.get("name").unwrap();
        println!("{name}");
    }
    Ok(())
}
```

The example trusts the server certificate for local development. Remove
`trust_cert()` and use a trusted certificate for production.
`Debug` output for `Config` and `AuthMethod` redacts passwords and access tokens.

## Migration from tiberius

| tiberius | mssql-rs |
|----------|----------------------|
| `tiberius::Config` | `Config` (same fluent API) |
| `tiberius::Client` | `Client` |
| `Client::connect(config, tcp)` | `Client::connect(&config)` (handles TCP internally) |
| `conn.simple_query(sql)` | `client.simple_query(sql)` |
| `conn.query(sql, &[&p1])` | `client.query(sql, &[&p1])` |
| cached connection-health check (no round trip) | `client.ping()` |
| connection-pool session cleanup | `client.reset_session()` (the default `TdsManager` recycling policy) |
| `stream.into_first_result()` | `.into_first_result()` |
| `row.get::<&str, _>("col")` | `row.get::<&str, _>("col")` |
| `tiberius::AuthMethod::sql_server` | `AuthMethod::sql_server` |

### Pooling behavior change

`TdsManager::new` and `TdsManager::create_pool` now reset reused sessions using
`mssql-tds`'s native `RESETCONNECTION` flag, restore `READ COMMITTED` isolation,
and validate the response. Temporary tables, changed session settings, open
transactions, and prepared handles no longer carry over to the next borrower.
Prepare and close statements within one checkout. Finish transactions before
returning a connection: recycling runs at the next checkout, not at check-in.

For intentional legacy session reuse, build a pool with
`TdsManager::new(config).with_recycling_method(RecyclingMethod::Ping)`.
`Ping` now uses the driver's cached `is_connection_dead()` status instead of
`SELECT 1`: it performs no I/O and leaves outstanding results untouched.
Success means "not known dead," not verified server responsiveness; an idle
connection failure may only be detected by the next operation.
See [connection pooling](https://github.com/microsoft/mssql-rs/blob/main/mssql-rs/docs/connection-examples.md#connection-pooling) for
examples and timeout configuration. `deadpool` still owns capacity and checkout;
`mssql-tds` provides the native reset and health primitives.

## Cancellation and timeouts

Dropping an in-flight driver operation with `tokio::time::timeout`, `select!`,
or task cancellation marks the connection dead. Later driver operations fail
immediately with `Error::Tds(mssql_tds::error::Error::ConnectionClosed(...))`.
Check `client.is_connection_dead()` without I/O, then drop the client and
reconnect. The pool rejects dead clients before reset or ping and replaces them
on checkout.

Wire streams remain reusable when dropped between fully yielded rows; dropping
a stream while its I/O is pending marks the connection dead. Cancelling only
`stream.next()` retains the internal future if you keep the stream and resume it.
Unpolled futures, unused bulk builders, and buffered result streams do not poison
the connection. Completed SQL errors retain the native driver's liveness outcome.
A failed or cancelled session reset always retires the connection.

Cancellation does not guarantee that SQL stopped executing or rolled back, so
do not automatically retry writes. Native cooperative cancellation requires
polling through cleanup; an external timeout that drops the operation is different.
Direct calls through `inner_mut()` bypass the driver's guards: after abandoning
native I/O, mark the native client dead and discard it instead of returning it
to a pool as healthy. See the `Client` rustdoc for the full contract.

## Spatial values

`geography` and `geometry` columns can be read directly in buffered or streamed
queries as `Vec<u8>` (owned) or `&[u8]` (borrowed). SQL `NULL` returns `None`.
Their column types are `ColumnType::Geography` and `ColumnType::Geometry`;
other CLR user-defined types report `ColumnType::Udt`.

```rust
use mssql_rs::ColumnType;

let rows = client
    .simple_query("SELECT geography::Point(47.6, -122.3, 4326) AS location")
    .await?
    .into_first_result();
assert_eq!(rows[0].columns()[0].column_type(), ColumnType::Geography);
let bytes: &[u8] = rows[0].get("location").unwrap();
```

These are SQL Server's native serialized bytes (equivalent to `.Serialize()`),
including the SRID, **not OGC Well-Known Binary (WKB)**. Use `.STAsBinary()` in
SQL when you need WKB instead. `Row::raw_value()` continues to return
`ColumnValues::Bytes`; no new value enum or spatial parser is required.
Point/LineString/Polygon parsing, `geo` integration, and native spatial parameter
binding are not provided. Existing byte parameters remain `varbinary`.

## Runtime requirements

The driver uses `mssql-tds` and its platform TLS backend. Linux builds require
OpenSSL development libraries, and dynamically linked binaries need compatible
OpenSSL libraries at runtime. Integrated authentication also loads a system
security library at runtime.

By default, the driver enables `mssql-tds`'s default features, including its
platform TLS backends. Disabling the driver's default features opts out unless
another dependency enables them.

### SQL authentication (`AuthMethod::sql_server`)

Requires the platform TLS dependencies and a working TCP stack.

### AAD token authentication (`AuthMethod::aad_token`)

Requires the platform TLS dependencies. Supply the JWT yourself, typically via
`azure_identity` or MSAL.

### Integrated authentication (`AuthMethod::Integrated`) — Linux / macOS

Uses Kerberos via `libgssapi_krb5`, loaded at **runtime** with `dlopen`. The library is **not** linked at build time; binaries build without it, but calling `Client::connect` with `Integrated` auth will fail at runtime if it's missing.

| OS | Package | Library file searched |
|---|---|---|
| Debian / Ubuntu | `libgssapi-krb5-2` (usually preinstalled; install via `apt-get install libgssapi-krb5-2`) | `libgssapi_krb5.so.2` |
| RHEL / Fedora / Rocky | `krb5-libs` | `libgssapi_krb5.so.2` |
| Alpine | `krb5-libs` (`apk add krb5-libs`) | `libgssapi_krb5.so.2` |
| Arch | `krb5` | `libgssapi_krb5.so.2` |
| macOS | Bundled with the OS (Heimdal) | `libgssapi_krb5.dylib` / `/usr/lib/libgssapi_krb5.dylib` |

**Distroless / scratch images:** must add the libgssapi-krb5 shared object explicitly, or use a base image that includes it. `gcr.io/distroless/cc-debian12` does **not** include it.

In addition to the library, integrated auth needs:

- A valid Kerberos ticket-granting ticket. Run `kinit user@REALM` (or use a keytab) before connecting.
- A correctly configured `/etc/krb5.conf` pointing at your KDC.
- Network reachability to the KDC (typically port 88) and a SPN registered for the SQL Server service principal.

### Integrated authentication (`AuthMethod::Integrated`) — Windows

Uses SSPI via `secur32.dll`, which is part of every supported Windows install. **No extra packages required.** The Windows account running the process must be a domain account (or have cached domain credentials) for Kerberos/NTLM to succeed.

### TLS

`mssql-tds` uses `native-tls`, which on Linux requires OpenSSL at runtime (already a dep of nearly every Linux distro and most container base images). Alpine needs `apk add openssl ca-certificates`.

## Development

`Cargo.toml` enables the Clippy lints listed in the
[`microsoft/mssql-rs` workspace manifest](https://github.com/microsoft/mssql-rs/blob/main/Cargo.toml),
including its commented-out lints. CI treats warnings as errors across all
targets and features, including benchmarks:

```sh
cargo clippy --all-targets --all-features -- -D warnings
```

## License

MIT
