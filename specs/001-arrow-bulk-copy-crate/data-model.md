# Data Model: Arrow Bulk Copy Crate

**Feature**: 001-arrow-bulk-copy-crate  
**Date**: 2026-03-07

## Entities

### ArrowBulkCopy

Builder for bulk inserting Arrow RecordBatch data into SQL Server.

**Fields**:
| Field | Type | Description |
|---|---|---|
| client | `&mut TdsClient` | Borrowed mssql-tds client connection |
| table_name | `String` | Destination SQL Server table |
| options | `BulkCopyOptions` | Batch size, timeout, constraints, triggers, etc. (from mssql-tds) |
| column_mappings | `Vec<ColumnMapping>` | Optional explicit column mappings (re-exported from mssql-tds) |
| progress_callback | `Option<Box<dyn FnMut(BulkCopyProgress) + Send>>` | Optional progress notification |

**Relationships**:
- Wraps `mssql_tds::connection::bulk_copy::BulkCopy` internally
- Produces `BulkCopyResult` on completion
- Uses `ArrowSerializer` for type conversion
- Uses `TypeMappingRegistry` for schema validation

**Validation**:
- Table name must be non-empty
- RecordBatch must have ≥1 column
- All Arrow column types must have a supported mapping (checked before data transfer)
- Column count must match destination (or explicit mappings must resolve)

---

### ArrowQueryReader

Accumulates SQL Server query results into Arrow RecordBatch format by implementing the `RowWriter` trait. Uses upfront metadata-based schema inference — column builders are initialized from `ColumnMetadata` before any rows are consumed.

**Fields**:
| Field | Type | Description |
|---|---|---|
| schema | `arrow_schema::Schema` | Arrow schema resolved from `ColumnMetadata` |
| columns | `Vec<ColumnBuilder>` | Per-column Arrow array builders (typed upfront from metadata) |
| names | `Vec<String>` | Column names from result metadata |
| row_count | `usize` | Rows accumulated in current batch |
| batch_size | `usize` | Maximum rows per batch before producing a RecordBatch |

**Relationships**:
- Implements `mssql_tds::datatypes::row_writer::RowWriter`
- Produces `arrow_array::RecordBatch` via `finish()`
- Independent copy based on `mssql-tds/src/datatypes/arrow_writer.rs` (currently `#[cfg(test)]`)
- Uses `column_metadata_to_data_type()` for upfront schema resolution

**State transitions**:
1. **Initialized** → created from `&[ColumnMetadata]`, all column builders typed upfront
2. **Accumulating** → receiving rows via `RowWriter` methods, values appended to correctly-typed builders
3. **Ready** → `row_count >= batch_size` or result set exhausted, call `finish()` to produce RecordBatch
4. **Consumed** → after `finish()`, builders are consumed; create new reader for next batch

---

### ColumnBuilder (internal)

Per-column Arrow array builder, initialized upfront from `ColumnMetadata` → `DataType` mapping. No `Uninitialized` state.

**Variants**:
| Variant | Arrow Builder Type | Resolved from TdsDataType |
|---|---|---|
| Boolean | BooleanBuilder | Bit, BitN |
| UInt8 | UInt8Builder | Int1, IntN(len=1) |
| Int16 | Int16Builder | Int2, IntN(len=2) |
| Int32 | Int32Builder | Int4, IntN(len=4) |
| Int64 | Int64Builder | Int8, IntN(len=8) |
| Float32 | Float32Builder | Flt4, FltN(len=4) |
| Float64 | Float64Builder | Flt8, FltN(len=8), Money, Money4, MoneyN |
| Decimal128 { precision: u8, scale: i8 } | Decimal128Builder | DecimalN, NumericN |
| Utf8 | StringBuilder | NVarChar, NChar, BigVarChar, BigChar, Text, NText, Xml, Json, Vector |
| Binary | BinaryBuilder | BigVarBinary, BigBinary, VarBinary, Binary, Image |
| Date32 | Date32Builder | DateN |
| Time64Microsecond | Time64MicrosecondBuilder | TimeN |
| TimestampMicrosecond | TimestampMicrosecondBuilder | DateTime, DateTim4, DateTimeN, DateTime2N |
| TimestampMicrosecondUtc | TimestampMicrosecondBuilder (tz="+00:00") | DateTimeOffsetN |
| FixedSizeBinary16 | FixedSizeBinaryBuilder(16) | Guid |

---

### ArrowSerializer (internal)

Converts Arrow columnar data to TDS wire format bytes using per-type serialization functions.

**Responsibilities**:
- Iterate Arrow columns in tight loops (columnar access pattern)
- Pre-serialize each column's values into per-row byte buffers
- Handle null encoding per TDS type class
- Handle precision-dependent decimal wire lengths
- Handle UTF-8 → UTF-16LE string transcoding with byte-count length prefix

**Key functions**:
| Function | Input | Output (appended to row buffer) |
|---|---|---|
| `serialize_bool` | BooleanArray | `[0x01][val]` or `[0x00]` for null |
| `serialize_u8` | UInt8Array | `[0x01][val]` or `[0x00]` for null |
| `serialize_i16` | Int16Array | `[0x02][i16 LE]` or `[0x00]` for null |
| `serialize_i32` | Int32Array | `[0x04][i32 LE]` or `[0x00]` for null |
| `serialize_i64` | Int64Array | `[0x08][i64 LE]` or `[0x00]` for null |
| `serialize_f32` | Float32Array | `[0x04][f32 LE]` or `[0x00]` for null |
| `serialize_f64` | Float64Array | `[0x08][f64 LE]` or `[0x00]` for null |
| `serialize_decimal` | Decimal128Array | `[len][sign][value LE]` or `[0x00]` for null |
| `serialize_nvarchar` | StringArray/LargeStringArray/StringViewArray | `[byte_count u16 LE][UTF-16LE]` or `[0xFF][0xFF]` for null |
| `serialize_nvarchar_plp` | StringArray/LargeStringArray/StringViewArray | `[PLP_UNKNOWN_LEN 8B][chunk_len 4B][UTF-16LE][PLP_TERM 4B]` or `[PLP_NULL 8B]` for null |
| `serialize_binary` | BinaryArray | `[byte_count u16 LE][bytes]` or `[0xFF][0xFF]` for null |
| `serialize_binary_plp` | BinaryArray | `[PLP_UNKNOWN_LEN 8B][chunk_len 4B][bytes][PLP_TERM 4B]` or `[PLP_NULL 8B]` for null |
| `serialize_date` | Date32Array | `[0x03][3 bytes LE]` or `[0x00]` for null |
| `serialize_time` | Time64MicrosecondArray | `[len][time bytes]` or `[0x00]` for null |
| `serialize_datetime2` | TimestampMicrosecondArray | `[len][time][date]` or `[0x00]` for null |
| `serialize_datetimeoffset` | TimestampMicrosecondArray (tz) | `[len][time][date][offset]` or `[0x00]` for null |
| `serialize_uuid` | FixedSizeBinaryArray(16) | `[0x10][16 bytes swapped]` or `[0x00]` for null |

---

### TypeMappingRegistry (internal)

Maps between Arrow DataType and SqlDbType, validates compatibility, determines wire encoding parameters.

**Fields**:
| Field | Type | Description |
|---|---|---|
| mappings | `Vec<ResolvedTypeMapping>` | Per-column resolved type pairs |

**ResolvedTypeMapping fields**:
| Field | Type | Description |
|---|---|---|
| source_index | `usize` | Arrow column index |
| dest_index | `usize` | SQL Server column index |
| arrow_type | `DataType` | Arrow column data type |
| sql_type | `SqlDbType` | SQL Server destination type |
| precision | `u8` | Decimal precision (from dest metadata) |
| scale | `u8` | Decimal scale (from dest metadata) |
| max_length | `i32` | Max byte length for variable-length types |
| encoding | `Option<EncodingType>` | Text encoding from dest metadata |
| is_nullable | `bool` | Whether dest column allows nulls |

**Key function — `column_metadata_to_data_type()`**: Converts `ColumnMetadata` to Arrow `DataType` using the mapping table in R11. Handles IntN/FltN/MoneyN/DateTimeN disambiguation by `type_info.length`, DecimalN by `VarLenPrecisionScale` variant, MAX types by `is_plp()`.

**Validation rules**:
- Reject Arrow types not in the mapping table (return error naming the unsupported type)
- Reject incompatible pairs (e.g., Int32 → NVARCHAR) with descriptive error
- Reject Decimal128 precision > 38 (SQL Server maximum)
- Warn/error for string columns exceeding max_length if not PLP

---

### ArrowBulkLoadRow (internal)

Implements `BulkLoadRow` trait for pre-serialized row data. Zero per-value dispatch at write time.

**Fields**:
| Field | Type | Description |
|---|---|---|
| tds_bytes | `Vec<u8>` | Pre-serialized TDS wire bytes for all columns |
| col_count | `usize` | Number of columns (for column_index advancement) |

**BulkLoadRow implementation**: Calls `writer.write_raw_bytes(&self.tds_bytes)` and advances `column_index` by `col_count`.

---

## Type Mapping Table

### Bidirectional (Read + Write) — FR-030

| Arrow DataType | SQL Server Type | Wire encoding | Null sentinel |
|---|---|---|---|
| `Boolean` | BIT | BITN: `[0x01][val]` | `[0x00]` |
| `UInt8` | TINYINT | INTN: `[0x01][val]` | `[0x00]` |
| `Int16` | SMALLINT | INTN: `[0x02][i16 LE]` | `[0x00]` |
| `Int32` | INT | INTN: `[0x04][i32 LE]` | `[0x00]` |
| `Int64` | BIGINT | INTN: `[0x08][i64 LE]` | `[0x00]` |
| `Float32` | REAL | FLTN: `[0x04][f32 LE]` | `[0x00]` |
| `Float64` | FLOAT | FLTN: `[0x08][f64 LE]` | `[0x00]` |
| `Decimal128(p, s)` | DECIMAL(p, s) | DECIMALN: `[len][sign][value]` | `[0x00]` |
| `Utf8` / `LargeUtf8` / `Utf8View` | NVARCHAR | `[byte_count u16][UTF-16LE]` | `[0xFF][0xFF]` |
| `Utf8` / `LargeUtf8` / `Utf8View` | NVARCHAR(MAX) | PLP: `[8B hdr][4B chunk_len][UTF-16LE][4B term]` | `[PLP_NULL 8B]` |
| `Binary` / `LargeBinary` | VARBINARY | `[byte_count u16][bytes]` | `[0xFF][0xFF]` |
| `Binary` / `LargeBinary` | VARBINARY(MAX) | PLP: `[8B hdr][4B chunk_len][bytes][4B term]` | `[PLP_NULL 8B]` |
| `Date32` | DATE | DATEN: `[0x03][3 bytes LE]` | `[0x00]` |
| `Time64(Microsecond)` | TIME | TIMEN: `[len][time bytes]` | `[0x00]` |
| `Timestamp(μs, None)` | DATETIME2 | DATETIME2N: `[len][time][date]` | `[0x00]` |
| `Timestamp(μs, tz)` | DATETIMEOFFSET | DTON: `[len][time][date][offset]` | `[0x00]` |
| `FixedSizeBinary(16)` | UNIQUEIDENTIFIER | `[0x10][16 bytes swapped]` | `[0x00]` |
| `Utf8` | XML | Same as NVARCHAR | `[0xFF][0xFF]` |
| `Utf8` | JSON | Same as NVARCHAR | `[0xFF][0xFF]` |

### Read-Only (SQL Server → Arrow) — FR-031

| SQL Server Type | Arrow DataType | Conversion |
|---|---|---|
| DATETIME | `Timestamp(μs, None)` | days-since-1900 + 1/300s ticks → epoch μs |
| SMALLDATETIME | `Timestamp(μs, None)` | days + minutes → epoch μs |
| MONEY | `Float64` | mixed-endian TDS → f64 / 10,000 |
| SMALLMONEY | `Float64` | i32 → f64 / 10,000 |
