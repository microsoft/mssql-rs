// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use adbc_core::options::OptionDatabase;
use adbc_core::{Connection, Database, Driver, Statement};
use adbc_driver_mssql::{MssqlDriver, options};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let get = |key| std::env::var(key);
    let database = MssqlDriver.new_database_with_opts([
        (
            OptionDatabase::from(options::DATA_SOURCE),
            get("ADBC_TEST_SERVER")?.into(),
        ),
        (
            OptionDatabase::from(options::DATABASE),
            get("ADBC_TEST_DATABASE")?.into(),
        ),
        (OptionDatabase::Username, get("ADBC_TEST_USER")?.into()),
        (OptionDatabase::Password, get("ADBC_TEST_PASSWORD")?.into()),
    ])?;
    let mut connection = database.new_connection()?;
    let mut statement = connection.new_statement()?;
    statement.set_sql_query(std::env::var("ADBC_SQL").unwrap_or_else(|_| "SELECT TOP(100000) CAST(42 AS int) AS n, CAST(REPLICATE('x',100) AS varchar(100)) AS text FROM sys.all_objects a CROSS JOIN sys.all_objects b".into()))?;
    let start = Instant::now();
    let mut rows = 0;
    let mut batches = 0;
    let mut max_batch_bytes = 0;
    for batch in statement.execute()? {
        let batch = batch?;
        rows += batch.num_rows();
        batches += 1;
        max_batch_bytes = max_batch_bytes.max(batch.get_array_memory_size());
    }
    println!(
        "rows={rows} batches={batches} elapsed_ms={} max_batch_bytes={max_batch_bytes}",
        start.elapsed().as_millis()
    );
    Ok(())
}
