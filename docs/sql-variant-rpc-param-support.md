# Plan: `sql_variant` RPC parameter support in `mssql-tds`

## Goal

Allow callers to pass a `sql_variant` value as an RPC / `sp_executesql` parameter.

The value-level serializer (`TdsValueSerializer::serialize_as_variant`) and the decode
path (`GenericDecoder::read_sql_variant`) already exist and are tested. The missing piece
is the public `SqlType` wrapper plus the RPC TYPE_INFO/declaration-name plumbing that routes
a parameter through the existing variant serializer.

This closes feature gap #7 ("`sql_variant` RPC parameter") in the feature-set wiki.

## Background: wire format

`sql_variant` is TDS type `0x62` (`TdsDataType::SsVariant`).

- **TYPE_INFO (RPC metadata):** type byte `0x62` followed by a 4-byte (`DWORD`) maximum
  data length. We send `8009` (`0x1F49`), the SQL Server maximum data size for a variant.
  This matches the existing bulk-load metadata path, which also writes a 4-byte length for
  `SsVariant`.
- **Value (`TYPE_VARBYTE`):** `[u32 total length][1-byte base type][1-byte prop-byte count]
  [N prop bytes][data]`. A `NULL` variant is encoded as a 4-byte length of `0`. This is
  already produced by `serialize_as_variant`.

`sql_variant` cannot hold: MAX types (`nvarchar(max)`, `varchar(max)`, `varbinary(max)`),
`xml`, `json`, `text`/`ntext`, vectors, or a nested `sql_variant`.

## Key files and current state

- `mssql-tds/src/datatypes/sqltypes.rs`
  - `pub enum SqlType` ends with placeholder comments (`// Variant`, `// TVP`). No `Variant`
    case.
  - `get_nullable_type()` maps `SqlType -> TdsDataType` (nullable). No `Variant` arm.
  - `to_column_value_and_context()` maps `SqlType -> (ColumnValues, TdsTypeContext)`; it sets
    `ctx.collation` for strings and `precision`/`scale` for decimals. No `Variant` arm.
  - `serialize()` short-circuits JSON, otherwise calls `write_rpc_type_metadata()` then
    `TdsValueSerializer::serialize_value()`. A variant flows through this normal path.
  - `write_rpc_type_metadata()` writes the RPC TYPE_INFO preamble. No `Variant` arm.
- `mssql-tds/src/datatypes/tds_value_serializer.rs`
  - `serialize_value()` routes to `serialize_as_variant()` when `ctx.tds_type == SQL_VARIANT`
    (`0x62`). Already implemented and tested. Reads the base type from `ColumnValues` via
    `get_variant_base_type()` and uses `ctx.collation` for string base types. Errors on
    `Vector`/`Xml`/`Json`, but cannot distinguish MAX/`text` (they collapse to `String`).
- `mssql-tds/src/message/parameters/rpc_parameters.rs`
  - `From<&SqlType> for TdsDataType`: no `Variant` arm.
  - `get_sql_name_impl()` builds the declaration string via `TdsDataType::get_meta_type_name()`,
    which already maps `SsVariant -> "sql_variant"`. The length match has a `_ => ""` catch-all,
    so a variant declares as `sql_variant` with no length suffix once the `From` arm is added.

## Design decisions

- Add `Variant(Box<SqlType>)` to `SqlType`. `Box` keeps the enum small; the inner `SqlType`
  carries the base type, the value, and its nullability. An inner value of `None` produces a
  `NULL` variant.
- `to_column_value_and_context()` `Variant` arm: recurse on the inner type to get
  `(cv, inner_ctx)`, then return `(cv, inner_ctx)` with `tds_type` overridden to `SsVariant`.
  This preserves collation/precision/scale that the inner type computed.
- Validation: reject inner types a `sql_variant` cannot hold (`NVarcharMax`, `VarcharMax`,
  `VarBinaryMax`, `Text`, `NText`, `Xml`, `Json`, `Vector`, and a nested `Variant`) with a
  `UsageError`. Validate in `write_rpc_type_metadata()` (called first by `serialize()`) so the
  error surfaces before any bytes are written. The serializer already rejects
  `Vector`/`Xml`/`Json`, but cannot catch the MAX/`text` cases because they collapse to a
  plain `String`/`Bytes` — so the authoritative check lives here.

## Implementation steps

1. `sqltypes.rs`: add `Variant(Box<SqlType>)` to `SqlType` (replacing the placeholder comment),
   with a doc comment.
2. `sqltypes.rs` `get_nullable_type()`: `SqlType::Variant(_) => TdsDataType::SsVariant`.
3. `sqltypes.rs` `to_column_value_and_context()`: `Variant(inner)` arm — recurse and override
   `tds_type` to `SsVariant`.
4. `sqltypes.rs` `write_rpc_type_metadata()`: `Variant` arm — call `validate_variant_inner()`
   first, then write `0x62` + `u32(8009)`.
5. `sqltypes.rs`: add `validate_variant_inner(&SqlType) -> TdsResult<()>` helper.
6. `rpc_parameters.rs` `From<&SqlType> for TdsDataType`: `SqlType::Variant(_) => SsVariant`.
7. Fix any other exhaustive `match` on `SqlType` the compiler flags.
8. Add unit tests (below).

## Verification

- `cargo build`, `cargo clippy`, `cargo test` in `mssql-rs`.
- Unit: `write_rpc_type_metadata(Variant(Int))` emits `[0x62, u32(8009)]`.
- Unit: `Variant(Int/NVarchar/Decimal/Uuid/Time)` round-trips through the serializer + decoder.
- Unit: `Variant(Int(None))` (NULL inner) emits a 4-byte length of `0`.
- Unit: `get_sql_name(Variant(..)) == "sql_variant"`.
- Unit: validation rejects `Vector`/`Xml`/`Json`/MAX/`text`/nested `Variant` with `UsageError`.

## Out of scope

- Decode side (already implemented).
- Higher-level `mssql` crate API and the Python/JS FFI wrappers (follow-up work).
- Always Encrypted variant parameters.
