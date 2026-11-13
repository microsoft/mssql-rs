// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

// Compatibility issue: https://github.com/saurabh500/mssql-tiberius-bridge/issues/129
use mssql_rs::{IntoRow, IntoSql, TokenRow};

fn main() {
    let mut row = TokenRow::with_capacity(2);
    row.push(1i32.into_sql().expect("valid parameter"));
    let _ = row.get(0);
    let _ = row.iter().count();
    let _ = row.clone().into_iter().count();
    row.clear();
    let _ = 1i32.into_row().expect("valid row");
    let _ = (1i32, 2i32).into_row().expect("valid row");
    let _ = (1i32, 2i32, 3i32).into_row().expect("valid row");
    let _ = (1i32, 2i32, 3i32, 4i32).into_row().expect("valid row");
    let _ = (1i32, 2i32, 3i32, 4i32, 5i32).into_row().expect("valid row");
    let _ = (1i32, 2i32, 3i32, 4i32, 5i32, 6i32).into_row().expect("valid row");
    let _ = (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32).into_row().expect("valid row");
    let _ = (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32).into_row().expect("valid row");
    let _ = (
        1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32, 9i32,
    )
        .into_row().expect("valid row");
    let _ = (
        1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32, 9i32, 10i32,
    )
        .into_row().expect("valid row");
}
