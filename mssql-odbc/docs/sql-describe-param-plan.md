# Plan: `SQLDescribeParam` support

## Work item

[AB#47373](https://sqlclientdrivers.visualstudio.com/mssql-rs/_workitems/edit/47373)

## Goal

Implement `SQLDescribeParam` with behavior compatible with msodbcsql and support
mssql-python's unresolved `None` parameter flow:

```text
SQLPrepare
  -> SQLDescribeParam
  -> SQLBindParameter(SQL_C_DEFAULT, inferred SQL type, SQL_NULL_DATA)
  -> SQLExecute
```

The API must infer parameter metadata before values are bound, return ODBC
metadata for any requested marker, and preserve that inferred type when a NULL
is sent over TDS.

## Reference behavior

The parity reference is the classic msodbcsql implementation in
`Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp`.

`SQLDescribeParam` does not correspond to a dedicated TDS token. The driver
implements it by:

1. Rewriting ODBC `?` markers to `@P1`, `@P2`, and so on during prepare.
2. Sending the rewritten SQL as the positional `nvarchar(max)` argument of an
   RPC to `sp_describe_undeclared_parameters`.
3. Reading every metadata row returned by the procedure.
4. Mapping the TDS type ID, length, precision, and scale to ODBC parameter
   metadata.
5. Caching all parameter records so later ordinal requests require no additional
   round trip.

The response must be fully drained even if a row cannot be mapped, keeping the
TDS stream usable for subsequent operations.

## Implementation

### ODBC API and statement state

- Export `SQLDescribeParam` and advertise it through every supported
  `SQLGetFunctions` form.
- Return `HY010` unless the statement contains prepared SQL.
- Return `07009` for ordinal zero or an ordinal beyond the prepared marker
  count.
- Report inferred parameters as `SQL_NULLABLE`.
- Store all inferred records in statement state and clear them whenever
  prepare, direct execution, or another statement-producing operation
  supersedes the SQL.

### TDS metadata discovery

- Claim the statement's connection using the existing execution ownership
  model.
- Execute `sp_describe_undeclared_parameters` with the rewritten prepared SQL.
- Parse `parameter_ordinal`, `suggested_precision`, `suggested_scale`,
  `suggested_tds_type_id`, and `suggested_tds_length`.
- Restore the connection and propagate server diagnostics through the existing
  ODBC diagnostic path.

The mapping must follow msodbcsql for:

- Integer widths and floating-point precision.
- Decimal/numeric precision and scale.
- Unicode byte lengths versus ODBC character lengths.
- MAX/PLP types, which msodbcsql reports as 0. Its unbounded sentinel
  `SQL_PREC_UNLIMITED` *is* 0 (`Sql/Ntdbms/sqlncli/tds/tds.h`), as is the public
  `SQL_SS_LENGTH_UNLIMITED` (`msodbcsql.h`), so reporting 0 is a direct match
  rather than a substitution; a parity run against msodbcsql 18.6.2.1 confirmed
  it. This matches the existing `describe_col::column_size`.
- GUID and scale-dependent temporal display sizes.
- SQL Server extension types, including variant, XML, UDT, table, and vector.

### mssql-python typed NULL execution

mssql-python describes unresolved `None` values and then binds them with
`SQL_C_DEFAULT` and `SQL_NULL_DATA`. Parameter conversion must therefore:

- Accept `SQL_C_DEFAULT` for NULL input values.
- Select the TDS NULL type from the described ODBC SQL type.
- Preserve exact declarations for decimal precision/scale, temporal scale,
  sized character and binary values, and vector dimensions.
- Use the exact declaration when building `sp_prepexec`, `sp_execute`, or
  `sp_executesql` parameter lists.

`SQLBindParameter` resolves `SQL_C_DEFAULT` to the SQL type's default C type
(`type_rules::resolve_default_c_type`) before storing the binding, so conversion
never sees `SQL_C_DEFAULT` itself. Keying the typed NULL off the resolved C type
would be wrong - a `decimal` resolves to `SQL_C_CHAR`, and would go out as a NULL
`varchar`. `BoundParam` therefore records a `c_type_defaulted` flag, and the NULL
path builds the value from `sql_type` whenever it is set.

The same flag exempts defaulted bindings from the character conversion matrix.
`resolve_default_c_type` returns the C type ODBC *defines* as that SQL type's
default, so the pairing is supported by construction; the matrix only enumerates
the explicit character pairings implemented so far and would otherwise reject the
describe-then-bind flow outright. `SQL_SS_UDT` and `SQL_SS_TABLE` stay rejected at
bind time because they need a fully qualified server type name that
`SQLDescribeParam` does not report.

Precision and scale that a NULL `SqlType` cannot carry (decimal
precision/scale lives inside the `Option` payload, temporal scale likewise)
travel in a single `RpcTypeMetadata` value attached to the RPC parameter. That
one value feeds both the rendered declaration text and the wire `TYPE_INFO`
header, so the two can never disagree: declaring `@P1 decimal(12,3)` while
serializing a `NUMERIC(1,0)` header would truncate the first non-NULL value a
caller sends on that statement.

Non-NULL `SQL_C_DEFAULT` conversion and binding NULL values for server types
whose required type names are not exposed by `SQLDescribeParam` are outside this
work item. That exclusion is enforced in code: `bound_param_to_value` rejects a
non-NULL defaulted bind unless `sql_type` is one of the six character SQL types.
Without that guard the `c_type` match would read the buffer as text for the four
SQL types `resolve_default_c_type` maps onto a character C type -- `SQL_DECIMAL`,
`SQL_NUMERIC`, `SQL_SS_VARIANT` and `SQL_SS_XML` -- and send `varchar(max)` /
`nvarchar(max)`. `sql_variant` is the sharp edge, since the server cannot assign
`varchar(max)` to it, so the application would see an opaque server-side error
instead of `HYC00`.

### Accepted parity deviations and confirmed matches

- **Zero `ColumnSize` on `char`/`nchar`/`binary`** -- *matches* msodbcsql for
  ODBC 3.x applications. `CheckSqlPrec` (`sqlcdesc.cpp`) treats a zero precision
  as invalid and returns `HY104`, clamping to the maximum only for a 2.x
  application (`IS2xAPP`). We report the same `HY104`, at execute rather than at
  bind. `varchar`/`nvarchar` accept zero and widen to `max`, which also matches:
  msodbcsql skips precision validation entirely for `SQL_VARCHAR`/`SQL_WVARCHAR`
  (`sqlcmisc.cpp`) and uses the data length instead.
- **Unbounded sizes reported as `0`** -- matches msodbcsql 18.6.2.1. Its
  `SQL_PREC_UNLIMITED` is itself `0` (`tds.h`), and `GetIPDRec` assigns exactly
  that for var-max and UDT parameters, so `0` is the same value msodbcsql
  reports rather than a substitute for a larger sentinel.

## Test plan

### Rust tests

- API state and ordinal diagnostics.
- Cached metadata output without a second RPC.
- Wire encoding of the metadata RPC.
- Representative TDS-to-ODBC type mappings and malformed metadata.
- Cache invalidation when prepared SQL is superseded.
- Typed NULL conversion and exact SQL declaration generation.
- Rejection of non-NULL defaulted binds outside the character SQL types.
- The metadata-RPC error tail against a scripted server: an empty result set
  reports `HY000`, surfaces the server's info message, and hands the connection
  back idle.
- `SQLGetFunctions` advertisement.

#### Deferred coverage

The error tail of the RPC orchestration in `sql_describe_param_safe` is covered
by `empty_metadata_result_set_reports_hy000_and_returns_connection`, which
scripts a `TdsClient` onto the `Dbc` and drives `advance_to_rows`, the drain
loop, `close_query()`, `collector.finish()`, `fail_metadata_response`, the
info-message path, and the client hand-back that keeps the connection usable
after a failure.

What remains deferred is the **success** path: mapping real metadata rows end to
end in Rust. That one needs ROW-token scripting in
`mssql_tds::test_client_support`, which today can script COLMETADATA, DONE, INFO
and ENVCHANGE but not rows -- `ScriptedToken`'s inner field is private, so there
is no way to emit a ROW without adding a helper to that crate. Row *mapping*
itself is unit-tested directly through `parse_parameter_row`, so the untested
remainder is only the wiring between `next_row()` and that function; the success
path is otherwise exercised by the C++ e2e suite, which does not feed
`cargo-llvm-cov`. Tracked as follow-up rather than done here, since it is a
change to another crate's test surface rather than to `SQLDescribeParam`.

### ODBC end-to-end parity tests

Run the same test binary against mssql-odbc and msodbcsql and require matching
observable behavior for:

- Function advertisement.
- `HY010` and `07009` diagnostics.
- Representative numeric, character, binary, decimal, and temporal metadata.
- A single mssql-python-style unresolved NULL.
- Multiple unresolved NULLs described before any binding.
- Typed NULL execution.
- Metadata invalidation after reprepare.
- `*(max)` parameters and a described decimal round-tripping its precision and
  scale.

Queries used for execution coverage must be independently inferable by SQL
Server so a shared inference failure is not mistaken for driver parity.

## Completion criteria

- The API is exported, advertised, and state-safe.
- Metadata discovery and caching match msodbcsql behavior.
- Representative mssql-python `None` bindings execute with the inferred SQL
  types.
- ODBC E2E comparison reports no divergence from msodbcsql.
- Repository formatting, linting, and targeted tests pass.
