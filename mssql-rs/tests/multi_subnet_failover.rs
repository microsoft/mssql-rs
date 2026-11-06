// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! MultiSubnetFailover integration test (issue #61).
//!
//! Real MSF only matters against an Always On listener whose DNS record
//! resolves to multiple IPs across subnets — we don't have one of those in
//! CI. What we *can* assert in CI is that enabling MSF against a normal
//! single-IP endpoint still connects and queries successfully (i.e., the
//! parallel-connect code path doesn't break the common case). This is the
//! same posture the .NET driver tests take for MSF in their bring-up suite.
//!
//! Uses the shared live-test environment; skips when its password is
//! unavailable.

pub mod common;

use mssql_rs::{Client, Config};

fn msf_config() -> Option<Config> {
    let mut cfg = common::live_config()?;
    cfg.multi_subnet_failover(true);
    Some(cfg)
}

#[tokio::test]
async fn multi_subnet_failover_connects_to_single_endpoint() {
    let Some(cfg) = msf_config() else {
        eprintln!(
            "skipping multi_subnet_failover_connects_to_single_endpoint: SQL_PASSWORD not set"
        );
        return;
    };
    assert!(cfg.is_multi_subnet_failover());

    let mut client = Client::connect(&cfg)
        .await
        .expect("MSF connect against single-IP endpoint should succeed");
    let rows = client
        .simple_query("SELECT 1 AS value")
        .await
        .expect("query failed")
        .into_first_result();
    assert_eq!(
        rows.first()
            .expect("expected row at index 0")
            .get::<i32, _>("value"),
        Some(1)
    );
}
