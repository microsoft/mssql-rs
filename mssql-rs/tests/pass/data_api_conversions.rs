// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

// Compatibility issue: https://github.com/saurabh500/mssql-tiberius-bridge/issues/128
use std::borrow::Cow;

use mssql_rs::{
    compat::{FromSql, ToSql as CompatToSql},
    ColumnData, FromSqlOwned, IntoSql,
};

struct Borrowing<'a>(&'a str);

impl CompatToSql for Borrowing<'_> {
    fn to_sql(&self) -> ColumnData<'_> {
        ColumnData::String(Some(Cow::Borrowed(self.0)))
    }
}

fn conversions<T, O>(value: T, borrowed: &ColumnData<'static>)
where
    T: IntoSql<'static>,
    O: FromSqlOwned,
{
    let data = <T as IntoSql>::into_sql(value);
    let _ = <O as FromSqlOwned>::from_sql_owned(data);
    let _ = <&str as FromSql>::from_sql(borrowed);
}

fn borrowed_into_sql<'a>(text: &'a String, bytes: &'a Vec<u8>) {
    let _: ColumnData<'a> = <&String as IntoSql>::into_sql(text);
    let _: ColumnData<'a> = <&Vec<u8> as IntoSql>::into_sql(bytes);
    let _: ColumnData<'a> = <Cow<'a, str> as IntoSql>::into_sql(Cow::Borrowed(text));
    let _: ColumnData<'a> = <Cow<'a, [u8]> as IntoSql>::into_sql(Cow::Borrowed(bytes));
    let _: ColumnData<'_> = CompatToSql::to_sql(&Cow::<str>::Borrowed(text.as_str()));
    let _: ColumnData<'_> = CompatToSql::to_sql(&Cow::<[u8]>::Borrowed(bytes.as_slice()));
}

fn main() {}
