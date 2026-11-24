// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

// Compatibility issue: https://github.com/saurabh500/mssql-tiberius-bridge/issues/125
use mssql_rs::{Column, QueryItem, QueryStream, Result};

async fn inspect(mut stream: QueryStream<'_>, item: QueryItem) -> Result<()> {
    let _ = format!("{stream:?}");
    let _: Option<&[Column]> = stream.columns().await?;
    if let Some(metadata) = item.as_metadata() {
        let _: usize = metadata.result_index();
    }
    if let Some(row) = item.as_row() {
        let _: usize = row.result_index();
    }
    Ok(())
}

fn main() {}
