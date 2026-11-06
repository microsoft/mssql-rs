// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

// Compatibility issue: https://github.com/saurabh500/mssql-tiberius-bridge/issues/125
use mssql_rs::{QueryStream, Result, Row};

async fn all_results(stream: QueryStream<'_>) -> Result<()> {
    let _: Vec<Vec<Row>> = stream.into_results().await?;
    Ok(())
}

async fn first_result(stream: QueryStream<'_>) -> Result<()> {
    let _: Vec<Row> = stream.into_first_result().await?;
    Ok(())
}

async fn one_row(stream: QueryStream<'_>) -> Result<()> {
    let _: Option<Row> = stream.into_row().await?;
    Ok(())
}

fn main() {}
