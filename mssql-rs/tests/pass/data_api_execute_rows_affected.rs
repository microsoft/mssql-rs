// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

// Compatibility issue: https://github.com/saurabh500/mssql-tiberius-bridge/issues/131
use mssql_rs::ExecuteResult;

fn counts(result: &ExecuteResult) -> &[u64] {
    result.rows_affected()
}

fn main() {}
