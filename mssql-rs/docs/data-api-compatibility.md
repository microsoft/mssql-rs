# Tiberius public data API compatibility

`mssql-rs` provides Tiberius-style data APIs alongside its native query and
result APIs. This comparison covers data operations, not connection construction
or connection management.

The reference is [saurabh500/tiberius at commit `e01b6a4`](https://github.com/saurabh500/tiberius/commit/e01b6a433ff97f91afb3db75852eab879f2927b2)
(baseline `c1d2e741ee6fe0935091826d91fb5886b6a4082a`), specifically
`DATA_API_COVERAGE.md`, `tests/data_api.rs`, `tests/query.rs`, and
`tests/bulk.rs`. The `TRACEABILITY` matrix in `tests/data_api_compat.rs` records
40 scenarios from that reference:

| Status | Scenarios |
|---|---:|
| Passes with the unchanged Tiberius-facing call | 10 |
| Passes with an adapted driver call | 27 |
| Compile/API gap | 1 |
| Behavioral gap | 0 |
| Intentional improvement | 2 |

These counts cover the selected scenarios, not the entire Tiberius API. The
intentional improvements are recoverable out-of-range numeric `try_get` access
and preservation of empty result sets that the pinned Tiberius implementation
can drop.

The recorded API gap is Tiberius's schema-bearing `XmlData` wrapper.
`ColumnData::Xml` preserves raw XML values for row iteration and parameter
encoding, and the native string conversion can read XML. The compatibility
`FromSql` and `FromSqlOwned` traits do not provide an `XmlData` conversion.

## Query streams and results

`Client::query_compat` and `Client::simple_query_compat` return a `QueryStream`
of `QueryItem::Metadata` and `QueryItem::Row` values. Metadata exposes columns
and a zero-based result index; rows expose their result index as well.
`into_row_stream` yields rows across result sets, while `into_results`,
`into_first_result`, and `into_row` collect results asynchronously.
`into_results` preserves empty middle and trailing result sets, and
`into_first_result` drains later results before returning.

An error encountered while `QueryStream::columns()` looks ahead is returned
once and consumed; subsequent stream polling does not repeat it.

The native `Client::query` and `Client::simple_query` methods return buffered
results. `query_streamed` and `simple_query_streamed` provide row-only streams.

## Conversions and rows

`ColumnData`, `FromSqlOwned`, and `IntoSql` are available at the crate root and
under `compat`. The `compat::FromSql` and `compat::ToSql` traits provide
Tiberius-shaped conversions without changing the native root traits.

`compat::FromSql` returns `Result<Option<T>>`: a target-compatible typed SQL
NULL is `Ok(None)`, while an incompatible value is `Error::Conversion`.
`Row::try_get_compat` uses this error channel for buffered and streamed rows.
Native `Row::get` and `Row::try_get` represent either NULL or a type mismatch
as `None`; missing columns still produce an error from `try_get` and a panic
from `get`.

`Row::cells()` and consuming row iteration expose compatibility `ColumnData`
in column order. The representation is created on first compatibility access,
so native-only row access does not pay for it. Borrowed strings and byte slices
remain borrowed through `IntoSql`.

Unlike Tiberius's infallible parameter conversion, native `ToSql::to_sql`,
`compat::ToSql::to_sql`, `IntoSql::into_sql`, `Query::bind`, and
`IntoRow::into_row` return `Result`, as does native `query::build_params`.
Propagate their errors with `?`.
Dates outside 0001-01-01 through 9999-12-31 return `Error::Conversion`;
offset datetime conversions check the UTC date. Failed parameter conversion
does not start a query or invalidate the connection.

Conversions cover primitives, strings and bytes, UUID, `rust_decimal`,
`chrono`, and the `time` and `jiff` types when their features are enabled.
Native vector, variant, and table parameters use `ColumnData::Native` rather
than a Tiberius-equivalent representation.

## Dynamic queries and affected rows

`Query`, also available as `compat::Query`, accepts borrowed or owned SQL.
`bind` appends parameters in `@P1`, `@P2`, ... order and preserves typed NULLs.
Calling `query` or `execute` consumes the builder and uses the driver's
parameter and execution paths.

`ExecuteResult::rows_affected()` borrows the per-statement counts in order,
including zero-row entries. The counts can also be consumed through
`IntoIterator` or summed with `total()`.

## Bulk inserts

`TokenRow` and `IntoRow` support scalar values and tuples of two through ten
values. Awaiting a `BulkInsert` builder creates a compatibility request whose
`send` method retains rows until `finalize`. Finalization performs one native
bulk operation and returns an `ExecuteResult` containing the client-side row
count, including `[0]` for an empty load.

Rows with inconsistent widths are rejected by `send` without changing the
request. Schema-dependent width, type, and length checks happen during
`finalize`; recognized input failures surface as `Error::BulkInput`. Dropping
the request before finalization sends no rows and leaves the connection usable.
For large inputs, the native `BulkInsert::send` accepts an iterator and streams
rows instead of retaining the compatibility request's entire input.

## Compatibility tests

`tests/data_api_compat.rs` maps the reference scenarios to test evidence, and
`tests/data_api_compile.rs` checks the fixtures in `tests/pass/data_api_*.rs`.
Live cases use the shared SQL Server test configuration and skip when no
password is configured.
