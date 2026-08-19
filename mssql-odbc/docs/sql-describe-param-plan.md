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
- MAX/PLP types, which msodbcsql reports as 0. The `SQL_PREC_UNLIMITED`
  (2147483647) path in `Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp` only passes
  through a precision that is already unlimited and is not reached from a PLP
  wire length; a parity run against msodbcsql 18.6.2.1 confirmed 0. This
  matches the existing `describe_col::column_size`.
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

Precision and scale that a NULL `SqlType` cannot carry (decimal
precision/scale lives inside the `Option` payload, temporal scale likewise)
travel in a single `RpcTypeMetadata` value attached to the RPC parameter. That
one value feeds both the rendered declaration text and the wire `TYPE_INFO`
header, so the two can never disagree: declaring `@P1 decimal(12,3)` while
serializing a `NUMERIC(1,0)` header would truncate the first non-NULL value a
caller sends on that statement.

Non-NULL `SQL_C_DEFAULT` conversion and binding NULL values for server types
whose required type names are not exposed by `SQLDescribeParam` are outside this
work item.

## Test plan

### Rust tests

- API state and ordinal diagnostics.
- Cached metadata output without a second RPC.
- Wire encoding of the metadata RPC.
- Representative TDS-to-ODBC type mappings and malformed metadata.
- Cache invalidation when prepared SQL is superseded.
- Typed NULL conversion and exact SQL declaration generation.
- `SQLGetFunctions` advertisement.

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
