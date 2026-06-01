// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Once;

pub mod api;
mod connection;
mod error;
mod handles;

static INIT_TRACING: Once = Once::new();

const ENV_TRACE: &str = "MSSQL_TDS_TRACE";
const ENV_TRACE_LEVEL: &str = "MSSQL_TDS_TRACE_LEVEL";
const DEFAULT_TRACE_LEVEL: &str = "warn";

pub(crate) fn init_tracing() {
    INIT_TRACING.call_once(|| {
        let enabled = std::env::var(ENV_TRACE)
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if !enabled {
            return;
        }

        let level = std::env::var(ENV_TRACE_LEVEL).unwrap_or_else(|_| DEFAULT_TRACE_LEVEL.into());
        let filter = tracing_subscriber::EnvFilter::try_new(level.as_str()).unwrap_or_else(|err| {
            eprintln!(
                "[mssql-odbc] ERROR: Invalid {ENV_TRACE_LEVEL} value '{level}': {err}. Falling back to '{DEFAULT_TRACE_LEVEL}'."
            );
            tracing_subscriber::EnvFilter::new(DEFAULT_TRACE_LEVEL)
        });

        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .try_init();
    });
}
