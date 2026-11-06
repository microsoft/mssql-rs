// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

// Compatibility issue: https://github.com/saurabh500/mssql-tiberius-bridge/issues/129
use mssql_rs::{Client, Error, ExecuteResult, IntoRow, Result};

async fn send(client: &mut Client) -> Result<()> {
    let mut bulk = client.bulk_insert("#items").await?;
    bulk.send((1i32, Option::<&str>::None).into_row()).await?;
    let result: ExecuteResult = bulk.finalize().await?;
    let _: &[u64] = result.rows_affected();
    Ok(())
}

fn bulk_input_error(error: Error) {
    assert!(matches!(error, Error::BulkInput(_)));
}

fn main() {}
