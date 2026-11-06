// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Tier-2 repro tests for selected upstream tiberius issues.
//!
//! Unit tests run without SQL Server. Live repros are ignored by default and
//! use the shared live-test environment (`tests/common/mod.rs`) when run.
//!
//! Triage status when added: #368 and the chrono path for #316 did not
//! reproduce. #333 preserves special-character passwords and fails with a clean
//! authentication error when the test user is absent.

pub mod common;

use mssql_rs::{AuthMethod, Client, Config, DecimalParts, FromSql, ToSql};
use mssql_tds::datatypes::column_values::{ColumnValues, SqlDateTime};

#[test]
fn test_368_negative_numeric_display_keeps_single_sign() {
    let decimal = DecimalParts::from_string("-17.80", 18, 2).expect("decimal parts");

    assert_eq!(decimal.to_string(), "-17.80");

    let rust_decimal = rust_decimal::Decimal::from_sql(&ColumnValues::Numeric(decimal))
        .expect("convert to rust_decimal");
    assert_eq!(rust_decimal.to_string(), "-17.80");
}

#[test]
fn test_316_datetime_before_1900_chrono_does_not_panic() {
    let value = ColumnValues::DateTime(SqlDateTime {
        days: -2, // 1899-12-30 relative to 1900-01-01.
        time: 0,
    });

    let got = std::panic::catch_unwind(|| chrono::NaiveDateTime::from_sql(&value))
        .expect("chrono conversion panicked");
    assert_eq!(
        got,
        Some(
            chrono::NaiveDate::from_ymd_opt(1899, 12, 30)
                .expect("valid test date")
                .and_hms_opt(0, 0, 0)
                .expect("valid test time")
        )
    );
}

#[test]
fn test_333_special_character_password_is_preserved_in_config() {
    let password = format!("{}!@%&*()", uuid::Uuid::new_v4());
    let mut cfg = Config::new();
    cfg.authentication(AuthMethod::sql_server("user", &password));

    let ctx = cfg.to_client_context();
    assert_eq!(ctx.user_name, "user");
    assert_eq!(ctx.password, password);
}

async fn live_client() -> Option<Client> {
    let cfg = common::live_config()?;
    Some(
        Client::connect(&cfg)
            .await
            .expect("connect to live SQL Server"),
    )
}

#[tokio::test]
#[ignore]
async fn test_160_trigger_rows_affected_has_single_update_count() {
    let Some(mut client) = live_client().await else {
        eprintln!("SQL_PASSWORD not set; skipping #160 live repro");
        return;
    };

    let suffix = uuid::Uuid::new_v4().simple();
    let table = format!("t_bridge_tier2_160_{suffix}");
    let trigger = format!("tr_bridge_tier2_160_{suffix}");
    client
        .simple_query(&format!(
            "BEGIN TRANSACTION;
             CREATE TABLE dbo.[{table}](id int NOT NULL);
             INSERT INTO dbo.[{table}](id) VALUES (1);
             EXEC('CREATE TRIGGER dbo.[{trigger}] ON dbo.[{table}] AFTER INSERT, UPDATE, DELETE AS BEGIN PRINT ''trigger fired''; END');"
        ))
        .await
        .expect("create trigger repro objects");

    let result = client
        .execute(&format!("UPDATE dbo.[{table}] SET id += 1"), &[])
        .await
        .expect("update trigger table");
    let counts: Vec<u64> = result.into_iter().collect();

    client
        .simple_query("ROLLBACK TRANSACTION")
        .await
        .expect("cleanup trigger repro objects");

    assert_eq!(
        counts,
        vec![1],
        "rows affected should contain only the outer UPDATE count"
    );
}

#[tokio::test]
#[ignore]
async fn test_380_into_results_preserves_empty_middle_result_set() {
    let Some(mut client) = live_client().await else {
        eprintln!("SQL_PASSWORD not set; skipping #380 live repro");
        return;
    };

    let results = client
        .simple_query("SELECT 1 AS a; SELECT * FROM (VALUES (1)) v(a) WHERE 1=0; SELECT 3 AS a;")
        .await
        .expect("multi-result query")
        .into_results();

    assert_eq!(results.len(), 3);
    assert_eq!(
        results
            .first()
            .expect("expected result set at index 0")
            .len(),
        1
    );
    assert_eq!(
        results
            .get(1)
            .expect("expected result set at index 1")
            .len(),
        0
    );
    assert_eq!(
        results
            .get(2)
            .expect("expected result set at index 2")
            .len(),
        1
    );
    assert_eq!(
        results
            .first()
            .expect("expected result set at index 0")
            .first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(1)
    );
    assert_eq!(
        results
            .get(2)
            .expect("expected result set at index 2")
            .first()
            .expect("expected row at index 0")
            .get::<i32, _>(0usize),
        Some(3)
    );
}

#[tokio::test]
#[ignore]
async fn test_371_query_streamed_yields_all_five_rows() {
    use futures_util::TryStreamExt;

    let Some(mut client) = live_client().await else {
        eprintln!("SQL_PASSWORD not set; skipping #371 live repro");
        return;
    };

    let rows: Vec<_> = client
        .simple_query_streamed("SELECT v.n FROM (VALUES (1),(2),(3),(4),(5)) v(n) ORDER BY v.n")
        .try_collect()
        .await
        .expect("stream five rows");

    let values: Vec<i32> = rows
        .iter()
        .map(|row| row.get::<i32, _>("n").expect("n"))
        .collect();
    assert_eq!(values, vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
#[ignore]
async fn test_282_stored_procedure_string_parameter_has_no_added_quotes() {
    let Some(mut client) = live_client().await else {
        eprintln!("SQL_PASSWORD not set; skipping #282 live repro");
        return;
    };

    client
        .simple_query(
            r#"
            CREATE PROCEDURE #p_bridge_tier2_282 @v NVARCHAR(50) AS
            BEGIN
                SET NOCOUNT ON;
                SELECT @v AS v;
            END
            "#,
        )
        .await
        .expect("create procedure");

    let rows = client
        .query("EXEC #p_bridge_tier2_282 @P1", &[&"hello"])
        .await
        .expect("exec procedure")
        .into_first_result();

    client
        .simple_query("DROP PROCEDURE #p_bridge_tier2_282")
        .await
        .expect("drop procedure");

    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<String, _>("v"),
        Some("hello".into())
    );
}

#[tokio::test]
#[ignore]
async fn test_221_nan_parameter_errors_without_poisoning_connection()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(mut client) = live_client().await else {
        eprintln!("SQL_PASSWORD not set; skipping #221 live repro");
        return Ok(());
    };

    client
        .simple_query("CREATE TABLE #t_bridge_tier2_221(v decimal(19, 4) NULL);")
        .await
        .expect("create NaN repro table");

    let err = client
        .execute(
            "INSERT INTO #t_bridge_tier2_221(v) VALUES (@P1)",
            &[&f64::NAN],
        )
        .await
        .expect_err("NaN insert into decimal should fail gracefully");

    let rows = client
        .simple_query("SELECT 1 AS ok; DROP TABLE #t_bridge_tier2_221;")
        .await
        .map_err(|follow_up| {
            format!(
                "connection was poisoned after NaN error {err:?}; follow-up failed: {follow_up:?}"
            )
        })?
        .into_first_result();
    let value = rows
        .first()
        .ok_or("follow-up query after NaN error returned no rows")?
        .get::<i32, _>("ok");
    if value != Some(1) {
        return Err(format!(
            "follow-up query after NaN error returned {value:?}, expected Some(1)"
        )
        .into());
    }
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_316_live_datetime_before_1900_chrono_does_not_panic() {
    let Some(mut client) = live_client().await else {
        eprintln!("SQL_PASSWORD not set; skipping #316 live repro");
        return;
    };

    let rows = client
        .simple_query("SELECT CAST('1899-12-30T00:00:00.000' AS datetime) AS dt")
        .await
        .expect("select pre-1900 datetime")
        .into_first_result();

    let got = std::panic::catch_unwind(|| {
        rows.first()
            .expect("expected row at index 0")
            .get::<chrono::NaiveDateTime, _>("dt")
    })
    .expect("reading pre-1900 datetime panicked");
    assert_eq!(
        got,
        Some(
            chrono::NaiveDate::from_ymd_opt(1899, 12, 30)
                .expect("valid test date")
                .and_hms_opt(0, 0, 0)
                .expect("valid test time")
        )
    );
}

#[tokio::test]
#[ignore]
async fn test_333_special_character_password_connect_fails_cleanly_or_succeeds() {
    let password = match std::env::var("BRIDGE_SPECIAL_PASSWORD") {
        Ok(password) => password,
        Err(_) => {
            eprintln!("BRIDGE_SPECIAL_PASSWORD not set; skipping #333 live repro");
            return;
        }
    };
    let Some(mut cfg) = common::live_config() else {
        eprintln!("SQL_PASSWORD not set; skipping #333 live repro");
        return;
    };
    cfg.authentication(AuthMethod::sql_server(
        std::env::var("BRIDGE_SPECIAL_PASSWORD_USER").unwrap_or_else(|_| "user".into()),
        password,
    ));

    match Client::connect(&cfg).await {
        Ok(mut client) => {
            let rows = client
                .simple_query("SELECT 1 AS ok")
                .await
                .expect("query with special-character password login")
                .into_first_result();
            assert_eq!(
                rows.first()
                    .expect("expected row at index 0")
                    .get::<i32, _>("ok"),
                Some(1)
            );
        }
        Err(err) => {
            let message = format!("{err:?}").to_lowercase();
            assert!(
                message.contains("login")
                    || message.contains("auth")
                    || message.contains("password")
                    || message.contains("user"),
                "special-character password should fail as a clean auth error, got: {err:?}"
            );
            assert!(
                !message.contains("parse") && !message.contains("datasource"),
                "special-character password caused a parse-like error: {err:?}"
            );
        }
    }
}

#[test]
fn test_221_nan_parameter_is_currently_encoded_as_float_nan() {
    let sql_type = f64::NAN.to_sql();
    assert!(
        matches!(sql_type, mssql_tds::datatypes::sqltypes::SqlType::Float(Some(v)) if v.is_nan()),
        "bridge currently passes NaN through to mssql-tds; live test verifies graceful recovery"
    );
}
