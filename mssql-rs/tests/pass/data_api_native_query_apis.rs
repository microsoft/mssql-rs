// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

use mssql_rs::{Client, ExecuteResult, QueryResult, QueryStream};

async fn native_apis_still_compile(client: &mut Client) -> mssql_rs::Result<()> {
    let result: QueryResult = client.query("SELECT @P1", &[&1i32]).await?;
    let _ = result.into_first_result();

    let result: ExecuteResult = client.execute("DELETE FROM #items WHERE id = @P1", &[&1i32]).await?;
    let _ = result.total();

    let result: ExecuteResult = client.execute("DELETE FROM #items WHERE id = @P1", &[&1i32]).await?;
    let _: &[u64] = result.rows_affected();
    let _ = ExecuteResult::into_iter(result);

    let result: ExecuteResult = client.execute("DELETE FROM #items WHERE id = @P1", &[&1i32]).await?;
    for _ in result {}

    let _: QueryStream<'_> = client.query_compat("SELECT @P1", &[&1i32]);
    Ok(())
}

fn main() {
    let _ = native_apis_still_compile;
}
