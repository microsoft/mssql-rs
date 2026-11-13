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
/// edition and server clock (days and milliseconds since 1970). Native sqlcmd
/// may append version, transport, server name and database when already known;
/// the native callback executes only this query.
pub const DRIVER_QUERY: &str = "SELECT CONVERT(int, SERVERPROPERTY('EngineEdition')), \
     DATEDIFF(day, '19700101', t.now), \
     DATEDIFF(millisecond, CONVERT(datetime2, CONVERT(date, t.now)), t.now) \
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
    /// The records from connecting, before any query records.
    pub connect_record_count: usize,
    /// The driver's name, e.g. `Microsoft ODBC Driver 18 for SQL Server`.
    pub driver_name: Option<String>,
    /// The callback's own status when it could not run the driver connection
    /// at all; `records` are then empty.
    pub failed_status: Option<i32>,
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
pub struct DriverHook(Arc<DriverConnect>);

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

pub(crate) fn driver_load_failure(state: &str) -> bool {
    matches!(state, "IM002" | "IM003" | "IM004" | "IM005" | "IM014")
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
    fn only_driver_load_states_identify_a_missing_driver() {
        for state in ["28000", "42000", "08001", "HYT00", "IM006"] {
            assert!(!driver_load_failure(state));
        }
        for state in ["IM002", "IM003", "IM004", "IM005", "IM014"] {
            assert!(driver_load_failure(state));
        }
    }

    #[test]
    fn hooks_compare_by_identity() {
        let hook = DriverHook::new(|_, _| DriverOutcome::default());
        assert_eq!(hook.clone(), hook);
        assert_ne!(hook, DriverHook::new(|_, _| DriverOutcome::default()));
    }
}
