// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

// Compatibility issue: https://github.com/saurabh500/mssql-tiberius-bridge/issues/131
use mssql_rs::ExecuteResult;

fn consume<I: IntoIterator<Item = u64>>(_: I) {}

fn probe(result: ExecuteResult) {
    consume(result);
}

fn main() {}
