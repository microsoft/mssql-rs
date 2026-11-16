# Async pool foundation: contributor context

Start here when reading the private Rust/Tokio contracts in this module.
They are compiled in normal builds, but are not connected to AQE or exposed to
Python. The A1 Deadpool experiment in [fit.rs](fit.rs) is test-only, not a
production pool or evidence of live SQL reuse.

The [pool design](../../../docs/async-connection-pooling-mssql-rs.md) defines
behavior; the [slot adaptation](../../../docs/async-pool-core-build-vs-reuse.md#51-slot-based-integration)
defines the Deadpool boundary. This guide maps those contracts to code, not a
second specification.

For the compiled manager and slot lifecycle, see the
[lifecycle implementation](../../../docs/async-connection-pooling-mssql-rs.md#103-a2-compiled-lifecycle).

## Type map

| Contract | Responsibility and boundary |
|---|---|
| `PoolConfig` in [config.rs](config.rs) | Validated per-key capacity and queue limits, idle policy and separate acquisition/cleanup/shutdown budgets. Builders return updated settings; they do not reconfigure a running manager. |
| `AcquireControl` in [control.rs](control.rs) | One caller-owned absolute deadline and cancellation token shared across acquisition stages. It drops interrupted work, but does not supervise detached work or cancel TDS/Python operations for their owners. |
| `AcquireStage` in [error.rs](error.rs) | Identifies where acquisition timed out; changing stages does not start a new budget. |
| `PoolKey` in [connection.rs](connection.rs) | Opaque fingerprint that partitions reuse. The caller adapter must supply a fingerprint, never a raw connection string or token; this type only rejects empty input, not noncanonical keys. |
| `AuthBinding` in [connection.rs](connection.rs) | Separate opaque identity/credential-version fingerprint expected for a physical open. Equality is not proof of a real login principal. |
| `TokenExpiry` and `AuthMetadata` in [connection.rs](connection.rs) | Distinguish non-token authentication, known UTC expiry and unknown raw-token expiry. Metadata carries the binding to compare with the expected one; it does not implement credential refresh or reuse policy. |
| `ConnectSpec` in [connection.rs](connection.rs) | Immutable, already-validated effective connection options and expected authentication binding supplied for each open. Recovery policy validation belongs to the caller. |
| `ConnectionFactory` in [connection.rs](connection.rs) | Object-safe physical-open interface receiving the current call's specification and TDS cancellation handle explicitly. No concrete factory is enabled here. |
| `OpenedConnection` in [connection.rs](connection.rs) | Owns one `TdsClient` and its authentication metadata by value. `into_parts()` moves both to the next owner; no shared raw-client owner escapes this handoff. |
| `PoolResult`, `PoolError` and `OpenError` in [error.rs](error.rs) | Typed outcomes, including preserved TDS/provider causes. Routine `Display`/`Debug` excludes source messages; Python error mapping is later integration work. |

`max_waiting = None` means unlimited waiting; a configured limit is positive and
counts queued requests, not assigned grants. The timeout budgets are required
inputs. Zero idle timeout is allowed and means expiry at the next lazy prune,
not a background cleanup task.

## How the contracts fit

These contracts support the private manager; physical SQL acquisition and AQE
checkout remain unwired:

```text
Caller adapter
  |-- PoolConfig + PoolKey --> per-key slot admission (private manager)
  |-- AcquireControl -------> one deadline/cancellation across stages
  `-- ConnectSpec ----------> ConnectionFactory::open
                                `--> OpenedConnection
                                       |-- uniquely owned TdsClient
                                       `-- AuthMetadata
```

A reserved acquisition guard must own its capacity slot before
physical opening/reset begins. Per-call options stay explicit rather than
being installed as shared "current request" state on the factory or scheduler.
The [Python integration design](../../../docs/async-connection-pooling-mssql-python-integration.md)
owns key construction, logical connection/cursor ownership and result delivery.

## Ownership and cancellation rules

- Never release capacity while the associated client or partial-open future can
  still live. The slot adaptation requires local client destruction before
  returning an empty slot; bare Drop is not successful clean return.
- Detached assigned-slot retirement rechecks acceptance and its original
  generation before return. Closure discards cached clients off-lock while
  retaining their capacity and shutdown-settlement accounting.
- `AcquireControl::run` checks interruption before work and again after work
  completes. A completed result is not permission to skip the manager's
  generation/publication check or the AQE close barrier.
- An acquisition cancellation token and a TDS `CancelHandle` serve different
  layers. Operation owners still need cancellation/settlement guards for TDS I/O,
  transport tasks and Python callbacks.
- Treat fingerprints as opaque input and keep sensitive source messages out of
  routine diagnostics. Authentication adapters must supply verified metadata;
  these types do not authenticate a connection.
- The fit experiment tests the fixed-capacity, non-I/O slot-hook contract.
  Do not assume its accounting remains valid with asynchronous/fallible hooks,
  live resizing or raw library-object access.
