# Connection examples

Terse examples for every connection option exposed by `mssql-rs`. All samples assume they run inside an async context.

## Quick start

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("localhost").authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD"))).trust_cert();
    let mut client = Client::connect(&cfg).await?;
    client.ping().await?;
    Ok(())
}
```

## Endpoint

### `Config::new()` defaults

`Config::new()` defaults to `localhost`, port `1433`, database `master`, encryption `On`, certificate validation enabled, SQL authentication with empty credentials, read-write intent, Unicode string parameters, and MultiSubnetFailover off. Override at least authentication before connecting.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `host(...)`

Default is `localhost`. Override it for any remote SQL Server, Azure SQL endpoint, AG listener, or container host name.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `port(...)`

Default is `1433`. Override it for non-default SQL Server listeners or port-mapped containers; do not rely on it when `instance_name(...)` is set, because the instance datasource omits the port for SQL Browser resolution.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("localhost")
        .port(14330)
        .authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .trust_cert();
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `database(...)`

Default is `master`. Set this to choose the initial database in the Login7 packet instead of connecting to `master` and issuing `USE` later.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .database("appdb")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `instance_name(...)`

Default is no named instance, so the datasource is `tcp:host,port`. Set an instance such as `SQLEXPRESS` when you want SQL Browser / SSRP to resolve the TCP port; when set, the bridge builds `tcp:host\\instance` and drops the explicit port.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("mssql.lab")
        .instance_name("SQLEXPRESS")
        .authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .trust_cert();
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

## Authentication

### `authentication(AuthMethod::sql_server(...))`

`Config::new()` starts with empty SQL authentication, which is not useful for real logins. Use `AuthMethod::sql_server(user, password)` for SQL Server authentication; it maps to `user_name` and `password` in the client context.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .authentication(AuthMethod::sql_server("app_user", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `authentication(AuthMethod::integrated())`

Default is SQL authentication, not integrated auth. Use `AuthMethod::integrated()` for Windows SSPI or Kerberos/GSSAPI; the bridge sets `TdsAuthenticationMethod::SSPI`.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.corp.example")
        .database("appdb")
        .authentication(AuthMethod::integrated());
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `authentication(AuthMethod::aad_token(...))`

Default is SQL authentication, not Entra ID. Use `AuthMethod::aad_token(token)` with a pre-acquired JWT scoped for `https://database.windows.net/.default`; the bridge sets access-token authentication and leaves username/password empty.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};
    let token = std::env::var("SQL_ACCESS_TOKEN").expect("set SQL_ACCESS_TOKEN");
    let mut cfg = Config::new();
    cfg.host("myserver.database.windows.net")
        .database("appdb")
        .encryption(EncryptionLevel::Required)
        .authentication(AuthMethod::aad_token(token));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

## Encryption / TLS

### `encryption(EncryptionLevel::On)`

`On` is the default. Keep it for normal encrypted connections where the server may validate with the platform trust store, a pinned CA, or a development-only trust override.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .encryption(EncryptionLevel::On);
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `encryption(EncryptionLevel::Required)`

Default `On` requests encryption; `Required` fails if the server cannot encrypt. Use it for production policies where silent downgrade is unacceptable.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .encryption(EncryptionLevel::Required);
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `encryption(EncryptionLevel::Strict)`

Default is not Strict. `Strict` is TDS 8.0 strict mode: TLS starts before TDS pre-login, ALPN uses `tds/8.0`, certificate validation is enforced, and `trust_cert()` is ignored.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .encryption(EncryptionLevel::Strict)
        .trust_cert_ca("/etc/ssl/sql-ca.pem");
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `encryption(EncryptionLevel::Off)`

Default is `On`. `Off` is an **alias of `NotSupported`** — both map to `EncryptionSetting::PreferOff` in the underlying `mssql-tds` wiring. The pair exists for parity with tiberius' API; pick whichever reads better. The server can still upgrade the connection to TLS regardless.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};
    let mut cfg = Config::new();
    cfg.host("legacy-sql.local")
        .authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .encryption(EncryptionLevel::Off);
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `encryption(EncryptionLevel::NotSupported)`

**Alias of `Off`** — both map to `EncryptionSetting::PreferOff`. Use whichever spelling matches the tiberius code you're porting.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};
    let mut cfg = Config::new();
    cfg.host("legacy-sql.local")
        .authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .encryption(EncryptionLevel::NotSupported);
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `trust_cert()`

Default is `false`, so the server certificate is validated. Use `trust_cert()` only for local development, test containers, or throwaway environments with self-signed certificates; it is ignored under `EncryptionLevel::Strict`.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("localhost")
        .authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .trust_cert();
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `trust_cert_ca(...)`

Default is no pinned certificate or CA path. Set `trust_cert_ca(path)` to provide a PEM or DER X.509 file for private PKI or self-signed SQL Server certificates; it sets `server_certificate` in the underlying TLS options.

**Supersedes `trust_cert()`**: if you call both, `mssql-tds` logs a warning and uses the pinned certificate (`ServerCertificate takes precedence`); `trust_cert()` becomes a no-op.

**Mutually exclusive with `host_name_in_certificate(...)`**: setting both makes `mssql-tds` fail the TLS handshake with a usage error. Pin a certificate *or* override the SNI hostname, not both.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config, EncryptionLevel};
    let mut cfg = Config::new();
    cfg.host("db.internal")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .encryption(EncryptionLevel::Required)
        .trust_cert_ca("/etc/ssl/private-sql-ca.pem");
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `host_name_in_certificate(...)`

Default is no override, so TLS validates against `host(...)`. Set this when connecting through an IP, CNAME, proxy, or listener whose DNS name differs from the certificate SAN/CN.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("192.0.2.15")
        .host_name_in_certificate("sql-prod.internal")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

## Identity & telemetry

### `application_name(...)`

Default is the underlying driver default. Set it to label sessions in `sys.dm_exec_sessions`, SQL auditing, and server-side telemetry.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .application_name("orders-api")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `client_name(...)`

Default comes from `mssql-tds`'s client context. Set it to control the workstation name / `workstation_id` sent in Login7.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .client_name("worker-01")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### Built-in driver identity

There is no setter for the driver identity. `to_client_context()` always sends library name `MS-MSSQL-RS`, the Cargo package version as driver version, and UserAgent fields so SQL Server can identify this driver.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .application_name("billing-job")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

## Read-only routing

### `readonly(...)`

Default is `false`, which sends `ApplicationIntent=ReadWrite`. Set `readonly(true)` for Always On readable secondaries or Azure SQL geo-replicas that route read-only sessions differently.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("ag-listener.example.com")
        .database("reporting")
        .authentication(AuthMethod::sql_server("report_user", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .readonly(true);
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

## Multi-subnet failover

### `multi_subnet_failover(...)`

Default is `false`, the normal single-target TCP connect path. Set `multi_subnet_failover(true)` for Always On listeners spanning multiple subnets; the driver resolves all A/AAAA records and races TCP connects in parallel. Available from the next release if your current release does not expose it yet.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("ag-listener.example.com")
        .database("appdb")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .trust_cert()
        .multi_subnet_failover(true);
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `is_multi_subnet_failover()`

This is a readback helper, not a connection setting. Use it for assertions or diagnostics when constructing config in layers.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("ag-listener.example.com")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .multi_subnet_failover(true);
    assert!(cfg.is_multi_subnet_failover());
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

## Misc

### `send_string_parameters_as_unicode(...)`

Default is `true`, so `&str` and `String` parameters are sent as NVARCHAR. Set it to `false` only when you intentionally want VARCHAR parameter metadata for legacy schema, collation, or index behavior.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .send_string_parameters_as_unicode(false);
    let mut client = Client::connect(&cfg).await?;
    let _ = client.query("SELECT @P1", &[&"varchar-shaped"]).await?;
    Ok(())
}
```

### `get_addr()`

This is a convenience accessor, not a setting. It returns `host:port` for logging and does not reflect `instance_name(...)` datasource rewriting.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .port(1433)
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let _addr = cfg.get_addr();
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `datasource_string()`

This is a convenience accessor for the string passed to `TdsConnectionProvider`. It returns `tcp:host,port` normally or `tcp:host\\instance` when `instance_name(...)` is set.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("mssql.lab")
        .instance_name("SQLEXPRESS")
        .authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
        .trust_cert();
    let _datasource = cfg.datasource_string();
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

### `to_client_context()`

This is the internal bridge to `mssql-tds`, exposed publicly for inspection. It wires database, authentication, TLS options, application/client names, driver identity, application intent, and MultiSubnetFailover into `ClientContext`; normal code should call `Client::connect(&cfg)` instead.

```rust,no_run
async fn example() -> mssql_rs::Result<()> {
    use mssql_rs::{AuthMethod, Client, Config};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let _ctx = cfg.to_client_context();
    let mut client = Client::connect(&cfg).await?;
    Ok(())
}
```

## Connection pooling

`Client` owns one connection and is not cloneable; use `TdsManager` with
`deadpool` for shared concurrency. Connections are created lazily with
`Client::connect(&config)`. Dropping a checked-out connection returns it to the
pool; native reset and validation happen before its next checkout.

The default `RecyclingMethod::Reset` rejects connections known to be dead and
calls `Client::reset_session()`. This drains any outstanding query, sets
`mssql-tds`'s native `RESETCONNECTION` flag, and executes
`SET TRANSACTION ISOLATION LEVEL READ COMMITTED`. That batch carries the reset
and validates the server's acknowledgement in one round trip; no separate ping
is needed. SQL Server reset does not restore isolation, so it is set explicitly.

The reset rolls back open transactions, clears temporary tables and session
settings, and restores the login database. Prepared statements from before a
reset are rejected with `Error::InvalidPreparedStatement` on a usable client;
known-dead clients return `ConnectionClosed` first. Prepare and close
them within a single checkout. A failed or cancelled reset marks the connection
dead, and the pool discards failed recycle attempts.

**Transaction ownership:** commit or roll back before dropping the connection.
Checkout-time reset prevents state reaching another borrower, but does not
release an abandoned transaction's locks while the connection sits idle.

If a bridge operation is dropped while I/O is pending (for example by
`tokio::time::timeout`), the client is marked dead. Recycling checks that native
state before reset or ping and rejects the client so deadpool replaces it. Outside a pool,
drop the client and reconnect; later bridge calls return a typed `ConnectionClosed`
error without I/O. `Client::is_connection_dead()` is a cached, no-I/O check.

This does not abort or roll back SQL reliably, so do not automatically retry
writes. Dropping a wire stream between complete rows preserves reuse; abandoning
a stream during pending I/O does not. Direct `inner_mut()` operations are
unguarded: mark the native connection dead and discard it after dropping native
I/O. Native cooperative cancellation must finish its async cleanup instead.

```rust,no_run
async fn example() -> std::result::Result<(), Box<dyn std::error::Error>> {
    use mssql_rs::{AuthMethod, Config, TdsManager};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .database("appdb")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let pool = TdsManager::create_pool(cfg, 16)?;
    let mut conn = pool.get().await?;
    let _ = conn.simple_query("SELECT 1").await?;
    Ok(())
}
```

For a custom `deadpool` builder, construct `TdsManager::new(cfg)` and pass it to
`Pool::builder`. Specify `Runtime::Tokio1` when setting wait, create, or recycle
timeouts. The convenience `create_pool` method already selects this runtime,
so its pool also supports `timeout_get`. No pool-level deadlines are set unless
requested.

```rust,no_run
fn example() -> std::result::Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    use deadpool::{managed::Timeouts, Runtime};
    use mssql_rs::{AuthMethod, Config, Pool, TdsManager};
    let mut cfg = Config::new();
    cfg.host("sql.example.com")
        .authentication(AuthMethod::sql_server("app", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")));
    let manager = TdsManager::new(cfg);
    let _pool = Pool::builder(manager)
        .max_size(8)
        .runtime(Runtime::Tokio1)
        .timeouts(Timeouts {
            wait: Some(Duration::from_secs(5)),
            create: Some(Duration::from_secs(30)),
            recycle: Some(Duration::from_secs(5)),
        })
        .build()?;
    Ok(())
}
```

To retain session state and use a cached health check instead of native reset:

```rust,no_run
fn example() -> std::result::Result<(), Box<dyn std::error::Error>> {
    use mssql_rs::{Config, Pool, RecyclingMethod, TdsManager};
    let manager = TdsManager::new(Config::new())
        .with_recycling_method(RecyclingMethod::Ping);
    let _pool = Pool::builder(manager).max_size(8).build()?;
    Ok(())
}
```

`Ping` calls `Client::ping()`, which checks the native
`Client::is_connection_dead()` status without sending SQL, resetting session
state, or consuming outstanding results. Both APIs perform no I/O. This retains
open transactions and prepared statements, but unlike the former `SELECT 1`
probe, does not verify server responsiveness. Success means "not observed dead";
a connection that silently failed while idle may fail on the next operation.
The async `client.ping().await?` call remains source-compatible. A known-dead
connection returns `Error::Tds` containing `mssql_tds::error::Error::ConnectionClosed`.
Use this policy only when retaining session state and relying on cached health
are intentional; default `Reset` recycling still performs a server round trip.

Outside a pool, call `client.reset_session().await?` directly for the same
reset-and-validate behavior. Use this bridge method rather than arming a reset
through `inner_mut()` so bridge-owned prepared handles are also invalidated.

## Tiberius API mapping

Use this table when migrating code that already builds a `tiberius::Config`.

| Bridge option | tiberius `Config` API |
|---|---|
| `Config::new()` | `tiberius::Config::new()` |
| `host(...)` | `tiberius::Config::host(...)` |
| `port(...)` | `tiberius::Config::port(...)` |
| `database(...)` | `tiberius::Config::database(...)` |
| `authentication(AuthMethod::sql_server(...))` | `tiberius::Config::authentication(tiberius::AuthMethod::sql_server(...))` |
| `authentication(AuthMethod::integrated())` | `tiberius::Config::authentication(tiberius::AuthMethod::Integrated)` / integrated auth helper in your tiberius version |
| `authentication(AuthMethod::aad_token(...))` | `tiberius::Config::authentication(tiberius::AuthMethod::aad_token(...))` |
| `encryption(EncryptionLevel::On)` | `tiberius::Config::encryption(tiberius::EncryptionLevel::On)` |
| `encryption(EncryptionLevel::Required)` | `tiberius::Config::encryption(tiberius::EncryptionLevel::Required)` |
| `encryption(EncryptionLevel::Strict)` | `tiberius::Config::encryption(tiberius::EncryptionLevel::Strict)` |
| `encryption(EncryptionLevel::Off)` | `tiberius::Config::encryption(tiberius::EncryptionLevel::Off)` |
| `encryption(EncryptionLevel::NotSupported)` | `tiberius::Config::encryption(tiberius::EncryptionLevel::NotSupported)` |
| `trust_cert()` | `tiberius::Config::trust_cert()` |
| `trust_cert_ca(...)` | `tiberius::Config::trust_cert_ca(...)` |
| `host_name_in_certificate(...)` | `tiberius::Config::host_name_in_certificate(...)` |
| `application_name(...)` | `tiberius::Config::application_name(...)` |
| `client_name(...)` | `tiberius::Config::client_name(...)` |
| `readonly(...)` | `tiberius::Config::readonly(...)` |
| `multi_subnet_failover(...)` | `tiberius::Config::multi_subnet_failover(...)` |
| `send_string_parameters_as_unicode(...)` | `tiberius::Config::send_string_parameters_as_unicode(...)` |
| `instance_name(...)` | `tiberius::Config::instance_name(...)` |
| `get_addr()` | `tiberius::Config::get_addr()` |
| `datasource_string()` | No direct migration requirement; bridge-specific inspection of the datasource passed to `mssql-tds` |
| `to_client_context()` | No tiberius equivalent; bridge-specific conversion to `mssql-tds` `ClientContext` |
| `Client::connect(&cfg)` | `tiberius::Client::connect(config, tcp_stream)`; the bridge owns TCP setup |
| `TdsManager::create_pool(cfg, max_size)` | Common tiberius + `deadpool` manager pattern, provided by the bridge |
