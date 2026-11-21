// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

use mssql_rs::bulk::{BulkCopyResult, BulkLoadRow};
use mssql_rs::{Client, Result};

async fn native_batch<R>(client: &mut Client, rows: Vec<R>) -> Result<BulkCopyResult>
where
    R: BulkLoadRow,
{
    client
        .bulk_insert("#items")
        .batch_size(100)
        .send(rows)
        .await
}

fn main() {}
