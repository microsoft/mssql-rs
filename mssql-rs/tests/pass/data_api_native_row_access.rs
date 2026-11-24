// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

use mssql_rs::{ColumnValues, Result, Row};

fn native_access(row: &Row) -> Result<Option<i32>> {
    let _ = row.columns();
    let _ = row.raw_value(0);
    let _ = row.get::<i32, _>("value");
    row.try_get::<i32, _>(0usize)
}

fn native_clone_and_equality(row: &Row) {
    let clone = row.clone();
    let _ = clone == *row;
}

fn main() {
    let _ = native_access;
    let _ = native_clone_and_equality;
    let _ = ColumnValues::Null;
}
