// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! sqlcmd's own driver connection, when the caller supplies one.
//!
//! The checks connect with `mssql-tds`, which is not the driver sqlcmd uses
//! for queries: a driver that is missing or broken, an option only the driver
//! honors, or a different TLS stack can make sqlcmd fail where `mssql-tds`
//! connects. With a [`DriverHook`], the connection attempt and session
//! validation are made by sqlcmd itself, with its own driver and settings,
//! and decide the result; `mssql-tds` then only adds evidence (phase timings,
//! the TLS failure, Kerberos) where the driver's errors leave the cause open.

use std::fmt;
use std::sync::Arc;

/// The query the driver runs for session validation. One row: the engine
/// edition, the server's clock (days and milliseconds since 1970, which every
/// supported version can compute), the product version, the transport the
/// connection uses, the server's name and the database in use.
pub const DRIVER_QUERY: &str = "SELECT CONVERT(int, SERVERPROPERTY('EngineEdition')), \
     DATEDIFF(day, '19700101', t.now), \
     DATEDIFF(millisecond, CONVERT(datetime2, CONVERT(date, t.now)), t.now), \
     CONVERT(nvarchar(128), SERVERPROPERTY('ProductVersion')), \
     CONVERT(nvarchar(40), CONNECTIONPROPERTY('net_transport')), \
     @@SERVERNAME, DB_NAME() \
     FROM (SELECT SYSUTCDATETIME() AS now) AS t";

/// The columns of [`DRIVER_QUERY`]'s row.
pub(crate) mod column {
    pub const ENGINE_EDITION: usize = 0;
    pub const DAYS: usize = 1;
    pub const MILLISECONDS: usize = 2;
    pub const VERSION: usize = 3;
    pub const TRANSPORT: usize = 4;
    pub const SERVER_NAME: usize = 5;
    pub const DATABASE: usize = 6;
}

/// One diagnostic record of the driver connection, as sqlcmd reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriverRecord {
    /// The ODBC SQLSTATE, e.g. `28000`.
    pub sql_state: String,
    /// SQL Server's error number, or the driver's native error (0 for none).
    pub native_error: i32,
    /// SQL Server's severity (class), when the record came from the server.
    pub severity: Option<i32>,
    pub state: Option<i32>,
    /// The message without the driver's `[vendor][component]` prefix.
    pub message: String,
    /// The line sqlcmd prints for the record, e.g. `Sqlcmd: Error: Microsoft
    /// ODBC Driver 18 for SQL Server : Login failed for user 'sa'..`; empty
    /// when sqlcmd prints none for it.
    pub line: String,
}

/// What sqlcmd's driver connection did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriverOutcome {
    pub connected: bool,
    /// How long connecting took.
    pub connect_ms: u64,
    /// The query failed (it was asked for and the connection succeeded).
    pub query_failed: bool,
    pub query_ms: u64,
    /// The query's row, column by column (`None` for SQL `NULL`); empty when it
    /// was not asked for or failed.
    pub row: Vec<Option<String>>,
    /// The records of the connection, then of the query, in order.
    pub records: Vec<DriverRecord>,
    /// The driver's name, e.g. `Microsoft ODBC Driver 18 for SQL Server`.
    pub driver_name: Option<String>,
}

impl DriverOutcome {
    pub(crate) fn cell(&self, column: usize) -> Option<&str> {
        self.row.get(column).and_then(Option::as_deref)
    }
}

/// Connects with sqlcmd's own driver and settings, runs `query` when given,
/// and disconnects. Called at most once per diagnosis, on the thread running
/// it, with the time limit in seconds; it blocks until the driver returns.
/// The driver connection a [`DriverHook`] calls.
pub type DriverConnect = dyn Fn(Option<&str>, u32) -> DriverOutcome + Send + Sync;

#[derive(Clone)]
pub struct DriverHook(pub Arc<DriverConnect>);

impl DriverHook {
    pub fn new(
        connect: impl Fn(Option<&str>, u32) -> DriverOutcome + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(connect))
    }

    pub(crate) fn connect(&self, query: Option<&str>, timeout_seconds: u32) -> DriverOutcome {
        (self.0)(query, timeout_seconds)
    }
}

impl fmt::Debug for DriverHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DriverHook")
    }
}

impl PartialEq for DriverHook {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for DriverHook {}

/// The transport name a `net_transport` value gives, as the report names it.
pub(crate) fn transport_name(net_transport: &str) -> &'static str {
    match net_transport.trim().to_ascii_lowercase().as_str() {
        "shared memory" => "sharedMemory",
        "named pipe" => "namedPipe",
        "tcp" => "tcp",
        _ => "other",
    }
}

/// Whether a driver failure leaves nothing for `mssql-tds` to add: the server
/// refused the login or the database (its error says why; a second failed
/// login would only count against the account), or the driver itself could
/// not be loaded.
pub(crate) fn needs_no_evidence(outcome: &DriverOutcome, login: &[u32], database: &[u32]) -> bool {
    outcome.records.iter().any(|r| {
        u32::try_from(r.native_error).is_ok_and(|n| login.contains(&n) || database.contains(&n))
            || r.sql_state.starts_with("IM")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transports_are_named_as_the_report_names_them() {
        assert_eq!(transport_name("Shared memory"), "sharedMemory");
        assert_eq!(transport_name("TCP"), "tcp");
        assert_eq!(transport_name("Named pipe"), "namedPipe");
        assert_eq!(transport_name("VIA"), "other");
    }

    #[test]
    fn a_refused_login_or_a_missing_driver_needs_no_second_attempt() {
        let record = |native_error, sql_state: &str| DriverOutcome {
            records: vec![DriverRecord {
                sql_state: sql_state.to_string(),
                native_error,
                ..DriverRecord::default()
            }],
            ..DriverOutcome::default()
        };
        assert!(needs_no_evidence(
            &record(18456, "28000"),
            &[18456],
            &[4060]
        ));
        assert!(needs_no_evidence(&record(4060, "42000"), &[18456], &[4060]));
        assert!(needs_no_evidence(&record(0, "IM002"), &[18456], &[4060]));
        assert!(!needs_no_evidence(&record(258, "08001"), &[18456], &[4060]));
        assert!(!needs_no_evidence(&record(0, "HYT00"), &[18456], &[4060]));
    }

    #[test]
    fn hooks_compare_by_identity() {
        let hook = DriverHook::new(|_, _| DriverOutcome::default());
        assert_eq!(hook.clone(), hook);
        assert_ne!(hook, DriverHook::new(|_, _| DriverOutcome::default()));
    }
}
