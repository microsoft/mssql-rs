// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Integration test for `Client::prepare` + `PreparedStatement` (issue #56).
//!
//! Exercises the full sp_prepare / sp_execute / sp_unprepare round-trip
//! against a live SQL Server. Skipped when the shared live-test password is
//! unavailable.

pub mod common;

use mssql_rs::Client;

#[tokio::test]
async fn prepare_select_arithmetic_runs_many_times_with_single_plan() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");

    let stmt = client
        .prepare("SELECT @P1 + @P2 AS s", &[&0i32, &0i32])
        .await
        .expect("prepare");
    assert!(
        stmt.handle() > 0,
        "sp_prepare should return positive handle"
    );
    assert_eq!(stmt.sql(), "SELECT @P1 + @P2 AS s");

    for (a, b, expected) in [
        (1i32, 2i32, 3i32),
        (10, 20, 30),
        (-5, 5, 0),
        (100, 200, 300),
    ] {
        let rows = stmt
            .query(&mut client, &[&a, &b])
            .await
            .expect("query_prepared")
            .into_first_result();
        assert_eq!(rows.len(), 1);
        let sum: i32 = rows
            .first()
            .expect("expected row at index 0")
            .get("s")
            .expect("s column");
        assert_eq!(sum, expected, "{a} + {b} should be {expected}");
    }

    stmt.close(&mut client).await.expect("sp_unprepare");

    // Connection should still be usable after unprepare.
    let rows = client
        .simple_query("SELECT 1 AS one")
        .await
        .expect("simple_query after unprepare")
        .into_first_result();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("one"),
        Some(1)
    );
}

#[tokio::test]
async fn prepare_with_string_param_and_multiple_executions() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");

    let stmt = client
        .prepare("SELECT @P1 AS msg, LEN(@P1) AS n", &[&"sample"])
        .await
        .expect("prepare");

    for s in ["hello", "world", "a longer phrase"] {
        let rows = stmt
            .query(&mut client, &[&s])
            .await
            .expect("query_prepared")
            .into_first_result();
        let msg: &str = rows
            .first()
            .expect("expected row at index 0")
            .get("msg")
            .expect("expected non-NULL column msg");
        let n: i32 = rows
            .first()
            .expect("expected row at index 0")
            .get("n")
            .expect("expected non-NULL column n");
        assert_eq!(msg, s);
        assert_eq!(
            usize::try_from(n).expect("string length must be nonnegative"),
            s.len()
        );
    }

    stmt.close(&mut client).await.expect("sp_unprepare");
}

#[tokio::test]
async fn prepare_with_no_params_works() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");

    let stmt = client
        .prepare("SELECT 42 AS answer", &[])
        .await
        .expect("prepare no-params");

    let rows = stmt
        .query(&mut client, &[])
        .await
        .expect("query no-params")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("answer"),
        Some(42)
    );

    stmt.close(&mut client).await.expect("close");
}

#[tokio::test]
async fn prepared_execute_against_dml() {
    let cfg = common::live_config_or_exit();
    let mut client = Client::connect(&cfg).await.expect("connect");

    // Use a temp table so we don't litter master.
    client
        .simple_query(
            "CREATE TABLE #t_prep_exec (id INT NOT NULL PRIMARY KEY, name NVARCHAR(64) NOT NULL)",
        )
        .await
        .expect("create temp table");

    let stmt = client
        .prepare(
            "INSERT INTO #t_prep_exec (id, name) VALUES (@P1, @P2)",
            &[&0i32, &""],
        )
        .await
        .expect("prepare INSERT");

    let rows = client
        .simple_query("SELECT COUNT(*) AS c FROM #t_prep_exec")
        .await
        .expect("count before execution")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("c"),
        Some(0),
        "prepare must not execute sample values"
    );

    for (id, name) in [(1i32, "Alice"), (2, "Bob"), (3, "Carol")] {
        let result = stmt
            .execute(&mut client, &[&id, &name])
            .await
            .expect("execute_prepared");
        assert_eq!(result.rows_affected(), &[1]);
        assert_eq!(result.total(), 1);
    }

    stmt.close(&mut client).await.expect("close");

    let rows = client
        .simple_query("SELECT COUNT(*) AS c FROM #t_prep_exec")
        .await
        .expect("count")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("c"),
        Some(3)
    );
}
