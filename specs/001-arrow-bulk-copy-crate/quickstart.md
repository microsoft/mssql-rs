# Quickstart: `mssql-arrow`

**Feature**: 001-arrow-bulk-copy-crate  
**Date**: 2026-03-07

## Setup

Add to your `Cargo.toml`:

```toml
[dependencies]
mssql-arrow = { path = "../mssql-arrow" }
mssql-tds = { path = "../mssql-tds" }
arrow-array = "57"
arrow-schema = "57"
tokio = { version = "1", features = ["full"] }
```

## Bulk Insert Arrow Data

```rust
use arrow_array::{Int32Array, Float64Array, StringArray, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use mssql_arrow::ArrowBulkCopy;
use mssql_tds::TdsConnectionProvider;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to SQL Server
    let provider = TdsConnectionProvider::new();
    let context = /* build ClientContext with credentials */;
    let mut client = provider.create_client(context, "tcp:myserver,1433", None).await?;

    // Create an Arrow RecordBatch
    let batch = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("price", DataType::Float64, true),
            Field::new("name", DataType::Utf8, true),
        ])),
        vec![
            Arc::new(Int32Array::from(vec![1, 2, 3])),
            Arc::new(Float64Array::from(vec![Some(9.99), None, Some(29.99)])),
            Arc::new(StringArray::from(vec![Some("Widget"), Some("Gadget"), None])),
        ],
    )?;

    // Bulk insert into SQL Server
    let result = ArrowBulkCopy::new(&mut client, "dbo.Products")
        .batch_size(10_000)
        .table_lock(true)
        .write_batch(&batch)
        .await?;

    println!("{} rows inserted in {:?}", result.rows_affected, result.elapsed);
    Ok(())
}
```

## Read Query Results as Arrow

```rust
use mssql_arrow::ArrowQueryReader;

// Execute a query
client.execute("SELECT id, price, name FROM dbo.Products".into(), None, None).await?;

// Read all results into Arrow batches (10,000 rows per batch)
let batches = ArrowQueryReader::read_result_set(&mut client, 10_000).await?;

for batch in &batches {
    println!("Batch: {} rows, {} columns", batch.num_rows(), batch.num_columns());
    println!("Schema: {:?}", batch.schema());
}
```

## Stream Multiple Batches

```rust
// Stream multiple RecordBatches into a single bulk insert session
let batches: Vec<RecordBatch> = load_from_parquet_partitions();

let result = ArrowBulkCopy::new(&mut client, "dbo.LargeTable")
    .batch_size(50_000)
    .on_progress(|p| {
        println!("Copied {} rows ({:.1} rows/s)", p.rows_copied, p.rows_per_second);
    })
    .write_batches(&batches)
    .await?;
```

## Column Mapping

```rust
use mssql_arrow::ColumnMapping;

// Map Arrow columns to different destination columns by name
let result = ArrowBulkCopy::new(&mut client, "dbo.Products")
    .add_column_mapping(ColumnMapping::by_name("product_id", "id"))
    .add_column_mapping(ColumnMapping::by_name("unit_price", "price"))
    .add_column_mapping(ColumnMapping::by_name("product_name", "name"))
    .write_batch(&batch)
    .await?;

// Or map by ordinal position
let result = ArrowBulkCopy::new(&mut client, "dbo.Products")
    .add_column_mapping(ColumnMapping::by_ordinal(0, "id"))
    .add_column_mapping(ColumnMapping::by_ordinal(1, "price"))
    .add_column_mapping(ColumnMapping::by_ordinal(2, "name"))
    .write_batch(&batch)
    .await?;
```

## Manual Batch Reading (Low-Level)

```rust
use mssql_arrow::ArrowQueryReader;

client.execute("SELECT * FROM dbo.LargeTable".into(), None, None).await?;

// Initialize reader from result set metadata (upfront schema inference)
let metadata = client.result_set().unwrap().get_metadata();
let mut reader = ArrowQueryReader::from_metadata(metadata, 10_000)?;
let mut all_batches = Vec::new();

// Read row by row, reader accumulates into Arrow batches
while client.next_row_into(&mut reader).await? {
    if reader.is_batch_ready() {
        if let Some(batch) = reader.finish()? {
            all_batches.push(batch);
        }
    }
}
// Flush remaining rows
if let Some(batch) = reader.finish()? {
    all_batches.push(batch);
}
```
