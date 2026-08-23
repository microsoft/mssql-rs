## Plan: TVP Support in mssql-tds

Add Table-Valued Parameter (TVP) support to the mssql-tds Rust crate so that stored procedures accepting TVPs can be called. TVPs are input-only parameters (TDS type `0xF3`) that send tabular data as a single RPC parameter. The design serves two consumers: (1) mssql-python for direct Python→TDS usage, and (2) a future ODBC driver built on mssql-tds.

### Design Approach

Use `SqlType` consistently for both column type definitions (with `None` values) and row cell values (with `Some`/`None` values). This reuses existing serialization infrastructure without creating a parallel type hierarchy.

**New types:**
- `SqlType::Table(TvpTypeName, Option<TvpTableData>)` — type name always present (required on wire even for NULL TVPs); `None` table data = NULL TVP, `Some` with empty rows = empty TVP
- `TvpTableData` — column definitions, row data, optional sort/unique hints
- `TvpColumnDef` — column type (`SqlType` with `None` value) + flags (default) + optional metadata overrides (`precision: Option<u8>`, `scale: Option<u8>`) for the **only** types whose metadata lives inside the `Option` and is therefore lost when the value is `None`: Decimal, Numeric, Time, DateTime2, DateTimeOffset. Variable-length string/binary types (`Varchar`/`NVarchar`/`VarBinary`/`Char`/`Binary`) carry their length as a separate non-`Option` field, so no override is needed — the column's `SqlType` variant fully describes the width
- `TvpTypeName` — db_name (catalog, always `None`/empty in practice — SQL Server forbids cross-database TVP types but the field is sent on the wire as `0x00`), schema_name, type_name
- `TvpColumnFlags` — bitflags: `NULLABLE = 0x01` (**always set** in TVP column metadata — per-cell null-ness is governed by the row data, not this flag, matching dotnet-sqlclient and mssql-jdbc), `DEFAULT = 0x200`
- `TvpOrderHint` — column ordinal + ASC/DESC/UNIQUE flags

**Wire format** (TDS `0xF3`): type byte → 3-part name → column count + per-column metadata (UserType, Flags, TypeInfo, ColName) → optional order/unique token → end metadata token → per-row `TVP_ROW_TOKEN` + values → `TVP_END_TOKEN`.

---

### Steps

**Phase 1: Core Types & Constants**

1. **Add TDS type** in `sqldatatypes.rs` — `SqlTable = 0xF3` to `TdsDataType` enum
2. **Create `sql_tvp.rs` module** — `TvpTableData`, `TvpTypeName`, `TvpColumnDef`, `TvpColumnFlags`, `TvpOrderHint` structs, plus TVP protocol constants (`TVP_ROW_TOKEN`, `TVP_END_TOKEN`, `TVP_ORDER_UNIQUE_TOKEN`, `TVP_NOMETADATA_TOKEN`). Register as `pub mod sql_tvp` in `datatypes.rs`. Module naming follows `sql_*.rs` convention (`sql_json.rs`, `sql_vector.rs`). `TvpColumnDef` includes optional `precision`, `scale` override fields (only needed for Decimal/Numeric/Time/DateTime2/DateTimeOffset, whose metadata lives inside the value `Option`)
3. **Add `Table` variant to `SqlType`** in `sqltypes.rs` — replace `// TVP` comment with `Table(TvpTypeName, Option<TvpTableData>)`. Also add `Table` arm to `get_nullable_type()` → `TdsDataType::SqlTable` (required for exhaustive match; called even by short-circuit paths like `serialize_json`)

**Phase 2: Serialization** *(depends on Phase 1)*

4. **Factor out type metadata serialization** — extract type-byte + length/precision/scale/collation from `SqlType::write_rpc_type_metadata()` into a reusable `write_type_info()` helper, since TVP column metadata uses the same encoding wrapped in additional fields
5. **TVP type name serialization** — `write_tvp_type_name()` in `sql_tvp.rs`: db/schema/type as B-STRINGs. B-STRINGs are UTF-16LE encoded with a `u8` prefix for the character count (not byte count) *(parallel with 6-7)*
6. **TVP column metadata serialization** — `write_tvp_column_metadata()` in `sql_tvp.rs`: column count (USHORT) + per-column **UserType (4 bytes / DWORD, value `0`)**, **Flags (2 bytes / USHORT — `NULLABLE = 0x01` always set, plus `DEFAULT = 0x200` when applicable)**, TypeInfo (via step 4 helper), ColName(0). TypeInfo uses `TvpColumnDef` override fields (precision/scale) when present, falling back to `SqlType(None)` defaults otherwise. **Variable-length string/binary encoding is variant-driven**: reuse the `write_type_info()` helper from step 4 so the column's `SqlType` variant decides the wire form — sized variants (`Varchar(_, n)`, `NVarchar(_, n)`, `VarBinary(_, n)`) emit their declared 2-byte length and use sized row encoding; explicit MAX variants (`VarcharMax`/`NVarcharMax`/`VarBinaryMax`) emit `Len = 0xFFFF` and PLP row encoding. This matches dotnet-sqlclient and mssql-jdbc (which preserve declared length when it fits) rather than msodbcsql's always-MAX (an ODBC DAE artifact that doesn't apply here since mssql-tds always knows the length from pre-loaded rows). A future ODBC driver can still opt into MAX/DAE by mapping DAE-bound columns to the `*Max` variants. Legacy LOB types (`Text`, `NText`, `Image`) are not permitted in table types — reject them with a `UsageError` *(depends on 4)*
7. **TVP order/unique metadata** — `write_tvp_order_unique()` in `sql_tvp.rs`: `0x10` token + ordinal/flags pairs + `0x00` end token *(parallel with 5-6)*
8. **TVP row data serialization** — `write_tvp_rows()` in `sql_tvp.rs`: per-row `0x01` token + column values (via `TdsValueSerializer::serialize_value`), then `0x00` end token. Row value `TdsTypeContext` must be derived from the **column definition** (not from the cell value) so that type widths are consistent across all rows. NULL TVP (`TvpTableData` is `None`): column count `0xFFFF` + **two** `TVP_END_TOKEN` bytes (first = end of optional metadata, second = end of row data) *(depends on 6)*
9. **Wire into `SqlType::serialize()`** — add `SqlType::serialize_table()` method in `sqltypes.rs` (following the `serialize_json()` pattern) that **short-circuits before `to_column_value_and_context()`** and delegates to `sql_tvp.rs` functions for wire encoding. Also add a `Table` arm in `to_column_value_and_context()` that returns `(ColumnValues::Null, default_ctx)` — unlike JSON which has a matching `ColumnValues::Json` variant, Table is input-only with no `ColumnValues` counterpart, but a safe fallback is needed since `to_column_value_and_context()` doesn't return `Result` and `unreachable!()` would panic if the method were called directly *(depends on 5-8)*

**Phase 3: Integration & Validation** *(depends on Phase 2)*

10. **Update `rpc_parameters.rs`** — three changes required:
    - Add `Table` arm in `From<&SqlType> for TdsDataType` → map to `TdsDataType::SqlTable`
    - Add `Table` arm in `get_sql_name_impl()` → return `"[schema].[TypeName] READONLY"` (schema-qualified, `READONLY` suffix required by SQL Server for TVP parameter declarations in `sp_executesql`)
    - Verify TVP params don't set `BY_REF_VALUE` (input-only)
11. **Type validation** — in `TvpTableData`: each row has `columns.len()` values, cell types match column defs, column count ≤ 1024, type_name non-empty
12. **Integration tests** — stored proc round-trip with multi-type TVP (varchar, int, decimal, datetime, binary, UUID), NULL TVP, empty TVP, schema-qualified names, large values (MAX types with PLP encoding)

---

### Relevant Files

**Modify:**
- `rust/mssql-rs/mssql-tds/src/datatypes/sqldatatypes.rs` — add `SqlTable = 0xF3`
- `rust/mssql-rs/mssql-tds/src/datatypes/sqltypes.rs` — add `Table` variant, `get_nullable_type()` arm, `serialize_table()` short-circuit, safe fallback in `to_column_value_and_context()`, extract `write_type_info()`
- `rust/mssql-rs/mssql-tds/src/datatypes.rs` — add `pub mod sql_tvp`

**Create:**
- `rust/mssql-rs/mssql-tds/src/datatypes/sql_tvp.rs` — TVP types (TvpTableData, TvpTypeName, TvpColumnDef, TvpColumnFlags, TvpOrderHint), protocol constants, and serialization functions

**Modify (minor):**
- `rust/mssql-rs/mssql-tds/src/message/parameters/rpc_parameters.rs` — add `Table` arm in `From<&SqlType> for TdsDataType` and `get_sql_name_impl()`

**No changes needed:**
- `rust/mssql-rs/mssql-tds/src/connection/tds_client.rs` — signature unchanged

**Reference implementations:**
- msodbcsql `tdsrpc.cpp` — `WriteTVPHeader`/`WriteTVPColMetadata`/`PushTVPRowData`
- dotnet-sqlclient `TdsParser.cs` — `WriteTvpTypeInfo` (L11367), `WriteTvpColumnMetaData` (L11405)
- JDBC `IOBuffer.java` — `writeTVP` (L4967), `writeTVPColumnMetaData` (L5451)
- pyodbc `tests/sqlserver_test.py` — `test_tvp` (L1472) for the Python API pattern

---

### Verification

1. Unit test: serialize known `TvpValue` → assert byte output matches TDS wire format
2. Integration: stored proc with TVP of mixed types, verify returned rows match
3. NULL TVP: `SqlType::Table(name, None)` → type name + column count `0xFFFF` + two `TVP_END_TOKEN` bytes
4. Empty TVP: 0 rows → proc returns 0 rows
5. Schema-qualified: `schema.TypeName` name serialization
6. Large values: `VARCHAR(MAX)` / `VARBINARY(MAX)` with PLP encoding in row data
7. Validation: mismatched row/column counts, empty type_name, >1024 columns → errors
8. Run existing test suite for regressions

---

### Decisions

- **Row data**: `Vec<Vec<SqlType>>` (pre-loaded, not streaming). Sufficient for all current use cases; streaming row source can be added later via a trait
- **Column type template**: `SqlType` with `None` values (e.g., `SqlType::Int(None)` defines an int column) — avoids parallel enum. Only types whose metadata is stored inside the value `Option` (Decimal, Numeric, Time, DateTime2, DateTimeOffset) need `TvpColumnDef` override fields; variable-length types already carry their length as a separate non-`Option` field on the variant
- **NULL TVP**: `SqlType::Table(type_name, None)` — type name is always present because the TDS wire format requires it even for NULL TVPs. `TvpTableData` being `None` triggers the `0xFFFF` + two `TVP_END_TOKEN` wire format (first token = end of optional metadata, second = end of row data), matching msodbcsql
- **Variable-length encoding is variant-driven** (matches dotnet-sqlclient/mssql-jdbc): sized variants (`Varchar(_, n)`/`NVarchar(_, n)`/`VarBinary(_, n)`) write their declared length and use sized row encoding; explicit MAX variants (`VarcharMax`/`NVarcharMax`/`VarBinaryMax`) write `Len = 0xFFFF` and PLP. This diverges from msodbcsql's always-MAX, which is an ODBC DAE artifact (ODBC can't know the length up front); mssql-tds always knows the length from pre-loaded rows. A future ODBC driver maps DAE-bound columns to the `*Max` variants to get MAX/PLP
- **Catalog/db_name**: included in the 3-part name on the wire (`0x00` when empty) for spec compliance with dotnet-sqlclient/mssql-jdbc, but defaults to `None` since SQL Server forbids cross-database TVP types
- **NULLABLE flag**: always set in TVP column metadata; per-cell null-ness comes from row data, not the flag (matches dotnet-sqlclient/mssql-jdbc)
- **Input-only**: No `BY_REF_VALUE` support for TVPs per SQL Server spec
- **Collation**: Use connection's `db_collation` for string columns (already threaded through serialization)
- **Excluded**: No TVP response parsing, no SQL_VARIANT in TVPs, no streaming row source

### Consumer Integration Notes (informing design, not in scope)

**mssql-python**: Would create `TvpTypeName` + `TvpTableData` from a Python list of tuples matching pyodbc's API — leading strings → `TvpTypeName`, first row infers column types → `Vec<TvpColumnDef>` (with precision/scale overrides for decimal/time types), each tuple → `Vec<SqlType>` row. Requires mssql-python to either adopt mssql-tds or call through its ODBC layer.

**Future ODBC driver**: `SQLBindParameter(SQL_SS_TABLE)` → `TvpTypeName`; `SQL_SOPT_SS_PARAM_FOCUS` + per-column binding → `Vec<TvpColumnDef>` (column size maps to the sized `SqlType` variant's length field; decimal digits map to the `precision`/`scale` overrides); DAE-bound columns map to the `*Max` variants for PLP; row arrays/DAE → `Vec<Vec<SqlType>>`; execute → `SqlType::Table(name, Some(data))` to mssql-tds. ODBC constants defined in the driver layer, not mssql-tds.

### Further Considerations

1. **SQL_VARIANT columns in TVPs** — JDBC and .NET support this but it adds significant complexity. Recommend deferring.
2. **Streaming row source** — For million-row TVPs, a `trait TvpRowSource` could replace `Vec<Vec<SqlType>>`. The wire format already supports row-by-row emission.
3. **DEFAULT columns** — `TvpColumnFlags::DEFAULT` flag is supported in metadata; the "skip default columns in row data" serialization can be deferred unless needed.
