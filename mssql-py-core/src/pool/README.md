# Async pool foundation: contributor context

Start here when reading the private Rust/Tokio pool manager and SQL checkout
implementation. They are compiled in normal builds, but are not connected to AQE
or exposed to Python. Production return-to-pool cleanup remains gated. The A1
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
| `PoolKey` in [connection.rs](connection.rs) | Opaque fingerprint that partitions reuse. The caller adapter must supply a fingerprint, never a raw connection string or token; this type only rejects empty input, not noncanonical keys. |
| `PreparedSqlConnection` in [connection_key.rs](../connection_key.rs) | Private Rust SQL-auth key authority above the scheduler. Validates Python-adapted options, freezes native endpoint policy, and derives a process-private HMAC key and authentication binding paired with the same immutable `ConnectSpec`. Not exported to Python or called by direct/pooled connect. |
| `AuthBinding` in [connection.rs](connection.rs) | Separate opaque identity/credential-version fingerprint expected for a physical open. Equality is not proof of a real login principal. |
| `TokenExpiry` and `AuthMetadata` in [connection.rs](connection.rs) | Distinguish non-token authentication, known UTC expiry and unknown raw-token expiry. Metadata carries the binding to compare with the expected one; it does not implement credential refresh or reuse policy. |
| `ConnectSpec` in [connection.rs](connection.rs) | Immutable per-call effective options and expected authentication binding. Native checkout calls `validate_pool()` before reservation to validate the context and reject nonzero recovery counts, unsupported authentication and Always Encrypted. |
| `ConnectionFactory` in [connection.rs](connection.rs) | Object-safe physical-open interface receiving the current call's specification and TDS cancellation handle explicitly. |
| `SqlConnectionFactory` in [connection.rs](connection.rs) | Opens a SQL-password connection through `TdsConnectionProvider::create_client` using the current call's specification. After successful login, returns the expected binding and non-token authentication metadata. |
| `OpenedConnection` in [connection.rs](connection.rs) | Owns one `TdsClient` and its authentication metadata by value. `into_parts()` moves both to the next owner; no shared raw-client owner escapes this handoff. |
| `Isolation` in [session.rs](session.rs) | Selects one concrete transaction isolation SET; rejects `NoChange` before checkout. |
| `AsyncPoolManager::acquire` in [session.rs](session.rs) | Validates pooled options, reserves a Deadpool slot, initializes a fresh connection or resets a cached candidate, and publishes only after authentication checks and fully drained, quiescent initialization. |
| `PoolLease` in [session.rs](session.rs) | Uniquely owns the client and capacity guard. Client access is borrowed from the lease; Drop discards rather than returning a reusable session. |
| `PoolResult`, `PoolError` and `OpenError` in [error.rs](error.rs) | Typed outcomes, including preserved TDS/provider causes. Routine `Display`/`Debug` excludes source messages; Python error mapping is later integration work. |

`max_waiting = None` means unlimited waiting; a configured limit is positive and
counts queued requests, not assigned grants. The timeout budgets are required
inputs. Zero idle timeout is allowed and means expiry at the next lazy prune,
not a background cleanup task.

## How the contracts fit

Native SQL checkout is implemented within the private manager. Production
return-to-pool cleanup and AQE/Python integration remain unwired:

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

Rust's [key authority](../connection_key.rs) owns SQL-auth key construction; the
[Python integration design](../../../docs/async-connection-pooling-mssql-python-integration.md)
defines parser/adaptation compatibility, logical connection/cursor ownership and
result delivery.

## SQL-auth key preparation

The private entry point accepts only the 21 canonical context fields currently
produced by Python's connection-string adaptation and login-timeout argument.
It rejects unknown fields, token/provider/attribute hooks, trusted authentication,
invalid types and unsigned/range violations. It reuses native authentication,
enum and context validation rather than adding a connection-string parser.

| Classification | Effective settings |
|---|---|
| Partition | Endpoint/instance, database, exact SQL principal/password, application name/intent, TLS/trust/certificate/SPN, packet size, IP preference, keep-alive, login timeout and recovery count/interval |
| Equivalent | Canonical adapted inputs independent of dictionary order, accepted enum casing/defaults, implicit/explicit SQL password and Trusted_Connection=No, inherited MSF matching an explicit effective value |
| Rust ODBC behavior | PacketSize=0/omission uses TDS default 8000; nonzero sizes clamp to 512-32768. Unknown IP preference resolves to IPv4First. Keep-alive inputs have already been converted to milliseconds by the existing Python adapter |
| Fixed native defaults | Workstation, language, library/version and User-Agent are taken from the prepared native context, not guessed by Python |
| Rejected | Unsupported fields and authentication modes; no arbitrary key, token provider or attrs_before seam |

Structured serialization is versioned and preserves empty strings, Unicode and
field boundaries. The whole descriptor and the separate principal/credential
binding are HMAC-SHA256 fingerprints with domain separation and one private
random process key. Cryptography reuses TDS's approved native providers
(Windows CNG, Apple native crypto and OpenSSL), not a new crypto implementation.
Routine diagnostics redact both prepared objects and opaque fingerprints.

No lease settings are accepted by this physical-spec seam. Future integration
must prove their reset/reapplication contract before reusing a session across
such settings. Recovery values remain in the fingerprint; native checkout rejects
nonzero recovery counts before admission. Connecting this preparation to checkout,
production check-in, public API and authentication beyond SQL password remain
separate work. There is no change to the direct connection paths.

The direct PyCore adapter still defaults packet size to 4096 and falls back for
out-of-range values, unlike this private Rust-ODBC-aligned preparation. Reconcile
or explicitly gate/document that application-visible difference before enabling
pooling. A physical factory must consume the prepared context, never reconstruct
it through direct adaptation after keying.

Run the real consumer-parser contract with the active Python environment:

```text
python -m pip install --upgrade -r mssql-py-core/tests/requirements-pool-key.txt
python .pipeline/scripts/run-sql-pool-keys.py
```

For a source consumer, build its native bindings first and make that checkout
importable in this Python environment. The runner preserves its import paths in
the embedded interpreter, fetches locked Cargo dependencies before the frozen
test run, runs the normally ignored parser test explicitly, and
produces nextest CI-profile JUnit results.
On Linux it prepends the active interpreter's `LIBDIR` to `LD_LIBRARY_PATH`
so standalone test executables can load Python. On macOS it defaults an unset
or empty `MACOSX_DEPLOYMENT_TARGET` to 10.15, matching the native TLS minimum;
explicit deployment targets are preserved and remain subject to build validation.
The bridge exists only in Rust tests; it compares private keys without exporting
keys or a production preparation API.
It covers every accepted keyword spelling, canonical adaptation, aliases,
ordering, duplicates, escaping, Unicode and rejection cases.
The test consumer requires 1.18.0 or later, whose parser accepts `APP` and
`Application Name`; both aliases, application-name partitioning and exact-value
semantics are covered. Version 1.17.0 rejected these spellings and is not the
contract targeted by this latest-consumer test. Validate against an installed
release wheel: a source checkout can contain unreleased changes, and a package
mirror can lag behind the public release.

The PR pipeline has dedicated Windows/Linux/macOS key-contract legs. Their
configuration is not proof of passing results. Existing live-SQL regression jobs
remain separate; no key-only test claims pooled acquisition, SPID reuse or reset.
These legs intentionally test the latest supported consumer release: newly
accepted options or changed private parser contracts fail CI until reconciled,
including on otherwise unrelated Rust PRs. There is no older-version fallback.

## Ownership and cancellation rules

- Never release capacity while the associated client or partial-open future can
  still live. The slot adaptation requires local client destruction before
  returning an empty slot; bare Drop is not successful clean return.
- `PoolLease` Drop destroys the client before releasing capacity.
  macOS Network.framework always force-cancels on Drop, including interrupted
  physical opens; discarded clients cannot keep a deferred graceful drain alive
  after releasing their slot. No pool-specific disposal option is required.
  `return_quiescent()` is test-only, not a production check-in API. Safe,
  awaited return belongs to AB#50052.
- Rust's key authority constructs canonical keys and expected authentication
  bindings; its integration with native checkout remains unwired.
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
