# Tiberius public data API compatibility

This contract is grounded in `saurabh500/tiberius` commit
`e01b6a433ff97f91afb3db75852eab879f2927b2` (baseline
`c1d2e741ee6fe0935091826d91fb5886b6a4082a`), specifically
`DATA_API_COVERAGE.md`, `tests/data_api.rs`, `tests/query.rs`, and
`tests/bulk.rs`. Connection construction and all connection-management
behavior are excluded.

The machine-readable traceability matrix is `TRACEABILITY` in
`tests/data_api_compat.rs`. Its 40 scenarios currently classify as:

| Status | Scenarios |
|---|---:|
| Passes with the unchanged Tiberius-facing call | 14 |
| Passes through the current bridge API | 23 |
| Compile/API gap | 1 |
| Behavioral gap | 0 |
| Intentional bridge improvement | 2 |

The two intentional improvements are recoverable out-of-range numeric access
and preservation of an empty middle result set in collected results. Tiberius
panics for the former and drops the empty set when consecutive metadata items
are collected for the latter.
The remaining API gap is Tiberius's schema-bearing `XmlData` wrapper. Raw XML
values remain available through `ColumnData::Xml` and the bridge-native string
conversion.

Issue #125 is available through the separate `compat` module and the
bridge-native `Client::query_compat` / `Client::simple_query_compat` entry
points. They expose `QueryStream`, `QueryItem`, `ResultMetadata`, columns, and
zero-based result indexes while reusing the native wire stream. Its collectors
intentionally preserve every metadata boundary, including empty middle and
trailing result sets; the pinned Tiberius implementation can drop those empty
sets. Errors returned while `QueryStream::columns()` looks ahead are delivered
once because the bridge's native error is not cloneable; a later stream poll
continues after that error. Existing buffered and row-only streaming methods
keep their contracts.

Issue #128 adds the conversion/error layer without changing existing
bridge-native APIs. `compat::FromSql` has Tiberius's fallible
`Result<Option<T>>` shape over compatibility `ColumnData`; a target-compatible
typed SQL NULL is `Ok(None)`, while an incompatible variant is
`Error::Conversion`.
`Row::try_get_compat` uses that error channel for buffered and
compatibility-stream rows. Existing root `FromSql`, `Row::get`, and
`Row::try_get` keep their signatures and their historical behavior of
representing either NULL or mismatch as `None`.

`ColumnData`, `FromSqlOwned`, and `IntoSql` are additive crate-root re-exports
and are also available under `compat`. `compat::FromSql` and `compat::ToSql`
provide Tiberius-shaped return values without changing the bridge-native root
conversion traits used by connection APIs. Borrowed strings and byte slices
remain borrowed through `IntoSql`; `Row::cells()` borrows compatibility values
from a cache populated on first compatibility access, so native-only row paths
do not pay for the second representation.

Issue #127 adds the dynamic `compat::Query` builder and its crate-root
re-export. The builder accepts borrowed or owned SQL, appends dynamic
parameters in `@P1`, `@P2`, ... order, preserves typed NULLs, and is consumed
by `query` or `execute`. It reuses the compatibility stream and bridge-native
parameter/execution paths; all existing `Client` methods and root/native
result behavior remain unchanged.
Encoding delegates to the existing native `ToSql` implementations for
primitives, strings and bytes, UUID, `rust_decimal`, `chrono`, and enabled
`time`/`jiff` types. Native vector, variant, and table parameters use
`ColumnData::Native`; they have no claimed Tiberius `ColumnData` equivalent.
`ColumnData::Xml` preserves XML values for row iteration and raw parameter
encoding, but the bridge does not expose Tiberius's schema-bearing `XmlData`
wrapper, so XML conversion through `FromSql`/`FromSqlOwned` is not implemented.

Issue #131 additively completes Tiberius-compatible `ExecuteResult` data
access with `rows_affected()` and standard consuming `IntoIterator`. Existing
bridge-native `total()` and inherent `into_iter()` calls remain unchanged.
Counts retain statement order and zero-row entries from the shared execution
collector used by direct, dynamic-query, and prepared execution.

Issue #130 additively exposes borrowed `Row::cells()` and consuming
`IntoIterator` in indexed column order. Both use the compatibility
`ColumnData` representation from #128, preserve SQL NULL cells, and adapt the
existing native row storage without changing `get`, `try_get`, `raw_value`,
result indexes, cloning, or equality.

Issue #129 additively exposes `TokenRow`, `IntoRow` for scalar and tuple
arities 2 through 10, and the awaited incremental bulk lifecycle. Awaiting the
existing `BulkInsert` builder selects the compatibility adapter; calling its
existing builder methods and batch `send` remains unchanged. Compatibility
`send` retains rows and checks that every row has the same width. `finalize`
performs one native `mssql-tds` bulk operation and returns `ExecuteResult`
with the client-side serialized row count, including `[0]` for an empty load.
Native usage failures from row encoding, including destination-width and
oversized-value failures, are exposed as `Error::BulkInput`; server and
connection errors keep their native error shape. Same-request width mismatches
fail from `send`; schema-dependent width, type, and length checks run in the
native writer during `finalize`.

Finalization is mandatory: dropping the compatibility request before
`finalize` writes no rows. Because the adapter has not started native I/O, the
client remains reusable. This is stronger than pinned Tiberius, which sends
`INSERT BULK` before returning its request but has no drop cleanup and leaves
connection reuse unspecified if the request is abandoned. The adapter also
leaves its retained rows unchanged after a rejected width mismatch, unlike
pinned Tiberius encoding, which can modify its packet buffer before returning
`BulkInput`. The native writer already checks every row against destination
metadata, and its client-side count avoids inflated results from multiple
server `DONE_COUNT` tokens.

## Phase 3 order

| Order | Logical API | Issue | Compile fixtures |
|---:|---|---|---|
| 1 | Query stream/items, metadata, result indexes, async collectors | [#125](https://github.com/saurabh500/mssql-tiberius-bridge/issues/125) | `data_api_query_stream.rs`, `data_api_query_stream_collectors.rs` |
| 2 | Public conversion traits and error channel | [#128](https://github.com/saurabh500/mssql-tiberius-bridge/issues/128) | `data_api_conversions.rs`, `data_api_conversion_errors.rs` |
| 3 | Dynamic `Query` builder | [#127](https://github.com/saurabh500/mssql-tiberius-bridge/issues/127) | `data_api_query_builder.rs` |
| 4 | `ExecuteResult` access and standard iteration (implemented, additive) | [#131](https://github.com/saurabh500/mssql-tiberius-bridge/issues/131) | `data_api_execute_rows_affected.rs`, `data_api_execute_into_iterator.rs` |
| 5 | Row cell/consuming iteration (implemented, additive) | [#130](https://github.com/saurabh500/mssql-tiberius-bridge/issues/130) | `data_api_row_iteration.rs` |
| 6 | `TokenRow`, `IntoRow`, and incremental bulk lifecycle (implemented, additive) | [#129](https://github.com/saurabh500/mssql-tiberius-bridge/issues/129) | `data_api_bulk_row.rs`, `data_api_bulk_lifecycle.rs` |

Unimplemented fixtures are expected compile failures in `tests/compile_fail`.
Implementing an issue means moving the same source to `tests/pass`; changing a
probe to fit an incompatible API does not satisfy the issue.
