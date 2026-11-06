# TDS client usage and data types

Detailed usage notes for
[`mssql-tds`](../README.md). See the [API reference](https://docs.rs/mssql-tds)
for public types and methods, and the
[connection behavior guide](connection-behavior.md) for connection policy,
TLS, encoding, and runtime details.

## Using a source checkout

Examples follow this repository's source APIs, which can be ahead of crates.io.
For an unpublished version, clone the repository and use local path dependencies.
Follow the [repository prerequisites](../../README.md#prerequisites) before
building.

With your application and the cloned `mssql-rs` in sibling directories, replace
the corresponding registry dependencies in your application's `Cargo.toml`:

```toml
[dependencies]
mssql-tds = { path = "../mssql-rs/mssql-tds" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

[dev-dependencies]
mssql-mock-tds = { path = "../mssql-rs/mssql-mock-tds" }
```

Paths are relative to that manifest; adjust them to your checkout location.
The mock dependency is only needed for tests. Keep both crates on the same
checkout so the examples use matching APIs.

## Request timeouts

`execute` accepts `()` for defaults or `ExecuteOptions` for timeout,
cancellation, and other per-command settings. SQL-batch timeout accounting
includes both sending the batch and reading its response.

Use `begin_transaction_with_options`, `commit_transaction_with_options`, and
`rollback_transaction_with_options` to bound transaction requests with the same
options. The timeout covers both sending the request and consuming its response.
Options do not leak into later requests. Their original counterparts remain
unbounded. Commit and rollback never reconnect onto a different session.

## Response lifecycle

Drain a response with `close_query()` before reuse, or call
`cancel_query(timeout_secs)` to abandon it and wait for cancellation
acknowledgement. `abandon_query(timeout_secs)` abandons it the way an ODBC
statement close does: unread messages and statement errors are discarded, and
ATTENTION is skipped when the response has already fully arrived. Zero uses the
transport default; failed cancellation settlement retires the connection.
Discarding rows during cancellation does not decode values or resolve Always
Encrypted keys.

Write deadlines apply to SQL batches, RPCs (including streamed parameters), and
bulk copy. If a timeout interrupts an in-flight packet, the connection is retired
without TLS shutdown or automatic session recovery, including during later explicit
closure. There is no packet-completion grace period, and cancellation cannot extend
the request's write deadline. Transport failures remain eligible for negotiated
recovery; at a clean packet boundary, normal withdrawal or cancellation recovery
remains available.

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
Serialization narrows each element to IEEE 754 binary16; reads widen it back to
`f32`. Conversion is lossy and can produce zero or infinity without an error.
Validate inputs if your application must reject that loss.

### Float16 conversion

Conversion uses round-to-nearest, ties-to-even: magnitudes at or below `2^-25`
become signed zero, and magnitudes of `65520.0` or more become infinity without an
error. Values between `65504.0` and `65520.0`, such as `65505.0f32`, round to
`65504.0`. These conversions do not imply that SQL Server accepts non-finite
elements.

Contributor test setup is in the
[float16 integration test guide](../tests/TRANSPORT_TESTS_README.md#float16-vector-integration-tests).

## Legacy LOB parameters

`SqlType::Text`, `SqlType::NText`, and `SqlType::Image` RPC parameters accept
materialized, empty, and NULL values. A streamed value can keep its legacy
declaration through `RpcParameter::with_streamed_declaration` while sending a
MAX/PLP payload, without buffering the whole value.
