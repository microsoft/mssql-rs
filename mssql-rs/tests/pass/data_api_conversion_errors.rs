// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

// Compatibility issue: https://github.com/saurabh500/mssql-tiberius-bridge/issues/128
use mssql_rs::{compat::FromSql, ColumnData, FromSqlOwned, Result};

struct Custom(i32);

impl<'a> FromSql<'a> for Custom {
    fn from_sql(value: &'a ColumnData<'static>) -> Result<Option<Self>> {
        i32::from_sql(value).map(|value| value.map(Custom))
    }
}

impl FromSqlOwned for Custom {
    fn from_sql_owned(value: ColumnData<'static>) -> Result<Option<Self>> {
        i32::from_sql_owned(value).map(|value| value.map(Custom))
    }
}

fn convert(value: &ColumnData<'static>) -> Result<Option<i32>> {
    i32::from_sql(value)
}

fn main() {}
