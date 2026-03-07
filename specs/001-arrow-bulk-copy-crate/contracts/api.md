# API Contract: `mssql-arrow`

**Feature**: 001-arrow-bulk-copy-crate  
**Date**: 2026-03-07  
**Type**: Rust library crate — public API surface

## Crate Re-exports (`lib.rs`)

```rust
// Primary types
pub use bulk_copy::ArrowBulkCopy;
pub use query_reader::ArrowQueryReader;
pub use error::ArrowError;

// Re-exports from mssql-tds for user convenience
pub use mssql_tds::connection::bulk_copy::{
    BulkCopyOptions, BulkCopyResult, BulkCopyProgress,
    ColumnMapping, ColumnMappingSource,
};
pub use mssql_tds::core::{CancelHandle, TdsResult};
```

---

## `ArrowBulkCopy` — Write Path

### Construction

```rust
impl<'a> ArrowBulkCopy<'a> {
    /// Create a new ArrowBulkCopy builder for the given table.
    pub fn new(client: &'a mut TdsClient, table_name: impl Into<String>) -> Self;
}
```

### Builder Methods (chainable, all return `Self`)

```rust
impl<'a> ArrowBulkCopy<'a> {
    pub fn batch_size(self, size: usize) -> Self;
    pub fn timeout(self, timeout: Duration) -> Self;
    pub fn check_constraints(self, enabled: bool) -> Self;
    pub fn fire_triggers(self, enabled: bool) -> Self;
    pub fn keep_identity(self, enabled: bool) -> Self;
    pub fn keep_nulls(self, enabled: bool) -> Self;
    pub fn table_lock(self, enabled: bool) -> Self;
    pub fn use_internal_transaction(self, enabled: bool) -> Self;
    pub fn add_column_mapping(self, mapping: ColumnMapping) -> Self;
    pub fn on_progress<F>(self, callback: F) -> Self
    where
        F: FnMut(BulkCopyProgress) + Send + 'a;
}
```

### Terminal Operations

```rust
impl<'a> ArrowBulkCopy<'a> {
    /// Bulk insert a single RecordBatch.
    /// Retrieves destination metadata, validates schema compatibility,
    /// pre-serializes Arrow data to TDS wire format, and writes to server.
    pub async fn write_batch(&mut self, batch: &RecordBatch) -> TdsResult<BulkCopyResult>;

    /// Bulk insert multiple RecordBatches in a single session.
    /// All batches must have the same schema.
    pub async fn write_batches(&mut self, batches: &[RecordBatch]) -> TdsResult<BulkCopyResult>;
}
```

### Behavior Contract

1. **Schema validation** occurs before any data is sent:
   - Arrow column types are checked against the type mapping registry
   - Column count/naming is verified against destination metadata
   - Unsupported types produce `TdsResult::Err` naming the type
   - Type incompatibilities produce `TdsResult::Err` with column details

2. **Pre-serialization** processes columns in tight loops:
   - Each Arrow column buffer is iterated contiguously
   - Null bitmap is checked; null values emit type-specific null wire bytes
   - Fixed-size types produce zero heap allocations
   - String values are transcoded UTF-8 → UTF-16LE
   - PLP-typed columns (NVARCHAR(MAX), VARCHAR(MAX), VARBINARY(MAX)) emit PLP framing: 8-byte header + 4-byte chunk length + data + 4-byte terminator; null PLP emits 8-byte `PLP_NULL`

3. **Cancellation**: Propagated via the existing `CancelHandle` mechanism on `TdsClient`

4. **Progress**: If `on_progress` is set, callback fires at `notification_interval` row boundaries

---

## `ArrowQueryReader` — Read Path

### Construction

```rust
impl ArrowQueryReader {
    /// Create a new reader initialized from column metadata.
    /// Column builders are typed upfront from the metadata — no lazy discovery.
    /// The reader will accumulate rows and produce a RecordBatch
    /// when `batch_size` rows have been collected or the result set ends.
    pub fn from_metadata(
        metadata: &[ColumnMetadata],
        batch_size: usize,
    ) -> TdsResult<Self>;
}
```

### Consuming Results

```rust
impl ArrowQueryReader {
    /// Consume the accumulated rows and produce a RecordBatch.
    /// Returns None if no rows have been accumulated.
    /// After calling finish(), the reader is reset for the next batch.
    pub fn finish(&mut self) -> TdsResult<Option<RecordBatch>>;

    /// Returns the number of rows accumulated in the current batch.
    pub fn row_count(&self) -> usize;

    /// Returns true if the current batch is full (row_count >= batch_size).
    pub fn is_batch_ready(&self) -> bool;

    /// Convenience: read all rows from a result set into Vec<RecordBatch>.
    /// Calls next_row_into in a loop, producing batches at batch_size boundaries.
    pub async fn read_result_set(
        client: &mut TdsClient,
        batch_size: usize,
    ) -> TdsResult<Vec<RecordBatch>>;
}
```

### RowWriter Implementation

```rust
impl RowWriter for ArrowQueryReader {
    // All 25 RowWriter methods implemented:
    fn write_null(&mut self, col: usize);
    fn write_bool(&mut self, col: usize, value: bool);
    fn write_u8(&mut self, col: usize, value: u8);
    fn write_i16(&mut self, col: usize, value: i16);
    fn write_i32(&mut self, col: usize, value: i32);
    fn write_i64(&mut self, col: usize, value: i64);
    fn write_f32(&mut self, col: usize, value: f32);
    fn write_f64(&mut self, col: usize, value: f64);
    fn write_string(&mut self, col: usize, value: SqlString);
    fn write_bytes(&mut self, col: usize, value: Vec<u8>);
    fn write_decimal(&mut self, col: usize, value: DecimalParts);
    fn write_numeric(&mut self, col: usize, value: DecimalParts);
    fn write_date(&mut self, col: usize, value: SqlDate);
    fn write_time(&mut self, col: usize, value: SqlTime);
    fn write_datetime(&mut self, col: usize, value: SqlDateTime);
    fn write_smalldatetime(&mut self, col: usize, value: SqlSmallDateTime);
    fn write_datetime2(&mut self, col: usize, value: SqlDateTime2);
    fn write_datetimeoffset(&mut self, col: usize, value: SqlDateTimeOffset);
    fn write_money(&mut self, col: usize, value: SqlMoney);
    fn write_smallmoney(&mut self, col: usize, value: SqlSmallMoney);
    fn write_uuid(&mut self, col: usize, value: Uuid);
    fn write_xml(&mut self, col: usize, value: SqlXml);
    fn write_json(&mut self, col: usize, value: SqlJson);
    fn write_vector(&mut self, col: usize, value: SqlVector);
    fn end_row(&mut self);
}
```

### Behavior Contract

1. **Upfront schema inference**: Column builders are typed at construction from `ColumnMetadata` via `column_metadata_to_data_type()`. All-null columns produce correctly-typed arrays filled with nulls (not `NullArray`). Schema is deterministic regardless of data content.

2. **Batch accumulation**: `end_row()` increments the row count. When `row_count >= batch_size`, calling code should call `finish()` to extract the batch.

3. **Schema source**: Column names and types come from `ResultSet::get_metadata()`. Arrow types are resolved from `TdsDataType` + `TypeInfo` disambiguation (IntN by length, DecimalN by precision/scale, MAX by PLP flag).

4. **Multiple result sets**: Each result set produces independent batches. The reader resets column builders between result sets via `finish()`.

---

## `ArrowError` — Error Types

```rust
/// Errors specific to Arrow ↔ TDS operations.
/// All variants convert to mssql_tds TdsResult via From<ArrowError> for Error.
pub enum ArrowError {
    /// Arrow column type has no SQL Server mapping
    UnsupportedArrowType {
        column_name: String,
        arrow_type: DataType,
    },

    /// Arrow column type is incompatible with destination SQL Server column
    TypeMismatch {
        column_name: String,
        arrow_type: DataType,
        sql_type: SqlDbType,
    },

    /// Arrow schema has different column count than destination table
    ColumnCountMismatch {
        arrow_columns: usize,
        sql_columns: usize,
    },

    /// Decimal precision exceeds SQL Server maximum of 38
    PrecisionOverflow {
        column_name: String,
        precision: u8,
    },

    /// String value exceeds NVARCHAR max_length and column is not PLP
    StringTruncation {
        column_name: String,
        value_len: usize,
        max_len: usize,
    },

    /// RecordBatch has zero columns
    EmptySchema,

    /// Arrow operation failed
    ArrowError(arrow_schema::ArrowError),
}
```

---

## Dependency Graph

```text
mssql-arrow
├── mssql-tds (path = "../mssql-tds")
│   └── [TdsClient, BulkCopy, BulkLoadRow, RowWriter, ResultSet, ...]
├── arrow-array = "57"
├── arrow-schema = "57"
├── arrow-buffer = "57"
├── async-trait
└── tracing
```

No reverse dependency: `mssql-tds` does not depend on `mssql-arrow`.
