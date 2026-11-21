# Async pool foundation: contributor context

Start here when reading the private Rust/Tokio pool manager and SQL checkout
implementation. They are compiled in normal builds, but are not connected to AQE
or exposed to Python. Explicit awaited release implements return-to-pool cleanup. The A1
Deadpool experiment in [fit.rs](fit.rs) is test-only, not a production pool or
evidence of live SQL reuse.

The [pool design](../../../docs/async-connection-pooling-mssql-rs.md) defines
behavior; the [slot adaptation](../../../docs/async-pool-core-build-vs-reuse.md#51-slot-based-integration)
defines the Deadpool boundary. This guide maps those contracts to code, not a
second specification.

For the compiled manager and slot lifecycle, see the
[lifecycle implementation](../../../docs/async-connection-pooling-mssql-rs.md#103-a2-compiled-lifecycle).
For native SQL initialization and reuse, see
[private SQL checkout](../../../docs/async-connection-pooling-mssql-rs.md#104-a3-private-sql-checkout).

## Type map

| Contract | Responsibility and boundary |
|---|---|
| `PoolConfig` in [config.rs](config.rs) | Validated per-key capacity and queue limits, idle policy and separate acquisition/cleanup/shutdown budgets. Builders return updated settings; they do not reconfigure a running manager. |
| `AcquireControl` in [control.rs](control.rs) | One caller-owned absolute deadline and cancellation token shared across acquisition stages. It drops interrupted work, but does not supervise detached work or cancel TDS/Python operations for their owners. |
| `AcquireStage` in [error.rs](error.rs) | Identifies where acquisition timed out; changing stages does not start a new budget. |
| `PoolKey` in [connection.rs](connection.rs) | Opaque fingerprint that partitions reuse. The Rust preparation adapter must supply a fingerprint, never a raw connection string or token; this type only rejects empty input, not noncanonical keys. |
| `AuthBinding` in [connection.rs](connection.rs) | Separate opaque identity/credential-version fingerprint expected for a physical open. Equality is not proof of a real login principal. |
| `TokenExpiry` and `AuthMetadata` in [connection.rs](connection.rs) | Distinguish non-token authentication, known UTC expiry and unknown raw-token expiry. Metadata carries the binding to compare with the expected one; it does not implement credential refresh or reuse policy. |
| `ConnectSpec` in [connection.rs](connection.rs) | Immutable per-call effective options and expected authentication binding. Native checkout calls `validate_pool()` before reservation to validate the context and reject nonzero recovery counts, unsupported authentication and Always Encrypted. |
| `ConnectionFactory` in [connection.rs](connection.rs) | Object-safe physical-open interface receiving the current call's specification and TDS cancellation handle explicitly. |
| `SqlConnectionFactory` in [connection.rs](connection.rs) | Opens a SQL-password connection through `TdsConnectionProvider::create_client` using the current call's specification. After successful login, returns the expected binding and non-token authentication metadata. |
| `OpenedConnection` in [connection.rs](connection.rs) | Owns one `TdsClient` and its authentication metadata by value. `into_parts()` moves both to the next owner; no shared raw-client owner escapes this handoff. |
| `Isolation` in [session.rs](session.rs) | Selects one concrete transaction isolation SET; rejects `NoChange` before checkout. |
| `AsyncPoolManager::acquire` in [session.rs](session.rs) | Validates pooled options, reserves a Deadpool slot, initializes a fresh connection or resets a cached candidate, and publishes only after authentication checks and fully drained, quiescent initialization. |
| `PoolLease` in [session.rs](session.rs) | Uniquely owns the client and capacity guard. Client access is borrowed from the lease; Drop discards rather than returning a reusable session. |
| `PoolLease::release` / `release_with_deadline` in [session.rs](session.rs) | Drain before residual rollback under one capped cleanup deadline, then atomically return only a quiescent client to its accepting generation. Cancellation or failure discards before slot return. |
| `PoolResult`, `PoolError` and `OpenError` in [error.rs](error.rs) | Typed outcomes, including preserved TDS/provider causes. Routine `Display`/`Debug` excludes source messages; Python error mapping is later integration work. |

`max_waiting = None` means unlimited waiting; a configured limit is positive and
counts queued requests, not assigned grants. The timeout budgets are required
inputs. Zero idle timeout is allowed and means expiry at the next lazy prune,
not a background cleanup task.

## How the contracts fit

Native SQL checkout is implemented within the private manager. Production
return is available through the lease; AQE/Python integration remains unwired:

```text
Caller adapter
  |-- PoolConfig + PoolKey --> per-key slot admission (private manager)
  |-- AcquireControl -------> one deadline/cancellation across stages
  `-- ConnectSpec + Isolation --> AsyncPoolManager::acquire
                                   |-- validate pooled options before admission
                                   |-- reserve a Deadpool slot
                                   |-- cached: reset + isolation SET + ACK + drain
                                   |-- fresh: factory open + isolation SET + drain
                                   `--> PoolLease (client and capacity guard)
```

A reserved acquisition guard must own its capacity slot before
physical opening/reset begins. Per-call options stay explicit rather than
being installed as shared "current request" state on the factory or scheduler.
Failed cached candidates are discarded. Retries are bounded by the initial
candidate count and rejoin FIFO with the original deadline and cancellation
token; the final replacement can retain its empty reservation for one fresh open.
The [key contract](../../../docs/async-connection-pooling-mssql-rs.md#71-key-contract)
assigns canonical key construction to the Rust preparation layer, using the same
immutable effective settings and authentication binding as physical opening.
This is an integration responsibility, not part of Deadpool slot scheduling.
The [Python integration design](../../../docs/async-connection-pooling-mssql-python-integration.md)
retains Python's connection-string parsing, authentication integration,
logical connection/cursor ownership and result-delivery responsibilities.

## Ownership and cancellation rules

- Never release capacity while the associated client or partial-open future can
  still live. The slot adaptation requires local client destruction before
  returning an empty slot; bare Drop is not successful clean return.
- `PoolLease` Drop destroys the client before releasing capacity.
  macOS Network.framework always force-cancels on Drop, including interrupted
  physical opens; discarded clients cannot keep a deferred graceful drain alive
  after releasing their slot. No pool-specific disposal option is required.
  `return_quiescent()` is test-only fault injection, not a production check-in API.
  Callers use `release()` or pass their original close deadline to
  `release_with_deadline()`. Both cap cleanup at the configured timeout.
  `mark_discard()` is irreversible and must be used after interrupted or
  indeterminate client I/O; later successful draining cannot undo it.
  Marked leases discard locally without protocol cleanup. No release path commits
  or requires graceful logout; errors and timeouts discard synchronously.
- The Rust preparation layer constructs canonical keys and expected authentication
  bindings from the effective connection settings; Python does not build keys.
  Native checkout owns pooled-context validation, including the zero-recovery
  restriction; it also checks returned metadata for binding equality and
  non-token authentication before using a client.
- Detached assigned-slot retirement rechecks acceptance and its original
  generation before return. Closure discards cached clients off-lock while
  retaining their capacity and shutdown-settlement accounting.
- `AcquireControl::run` checks interruption before work and again after work
  completes. A completed result is not permission to skip the manager's
  generation/publication check or the AQE close barrier.
- An acquisition cancellation token and a TDS `CancelHandle` serve different
  layers. SQL opening and initialization own operation guards that cancel TDS
  handles on interruption. Parallel TCP attempts are caller-owned and drop
  synchronously before capacity is released. Python callback cancellation and
  settlement remain future integration work.
- Treat fingerprints as opaque input and keep sensitive source messages out of
  routine diagnostics. Authentication adapters must supply verified metadata;
  these types do not authenticate a connection.
- The fit experiment tests the fixed-capacity, non-I/O slot-hook contract.
  Do not assume its accounting remains valid with asynchronous/fallible hooks,
  live resizing or raw library-object access.
