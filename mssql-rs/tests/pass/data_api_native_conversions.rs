// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

use mssql_rs::{ColumnValues, FromSql, Result, Row};

struct NativeI32(i32);

impl<'a> FromSql<'a> for NativeI32 {
    fn from_sql(value: &'a ColumnValues) -> Option<Self> {
        i32::from_sql(value).map(Self)
    }
}

fn native_convert(value: &ColumnValues) -> Option<i32> {
    i32::from_sql(value)
}

fn native_row_access(row: &Row) -> Result<Option<NativeI32>> {
    row.try_get("value")
}

fn main() {
    let value = NativeI32(1);
    let _ = value.0;
    let _ = native_convert(&ColumnValues::Int(1));
    let _ = native_row_access;
}
