// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashMap;
use std::time::Instant;

use mssql_tds::connection::tds_client::{ExecuteOptions, StatementResult, TdsClient};
use mssql_tds::core::{CancelHandle, TdsResult};
use mssql_tds::error::Error as TdsError;
use mssql_tds::message::parameters::rpc_parameters::RpcParameter;
use tracing::{debug, error};

use super::exec_common::{deduct_query_timeout, query_timeout_expired_error};
use crate::handles::DbcHandle;

#[derive(Clone, Copy)]
enum Procedure {
    Columns = 1,
    TypeInfo = 2,
}

impl Procedure {
    fn from_legacy(name: &str) -> Option<Self> {
        match name {
            "sp_columns_100" => Some(Self::Columns),
            "sp_datatype_info_100" => Some(Self::TypeInfo),
            _ => None,
        }
    }

    fn modern(self) -> &'static str {
        match self {
            Self::Columns => "sp_columns_170",
            Self::TypeInfo => "sp_datatype_info_170",
        }
    }
}

/// Negative entries belong to a physical session and an exact database name,
/// matching sqlcdd.cpp's CatalogProcedureIsMissing on feature/JsonFeature.
#[derive(Default)]
pub(crate) struct CatalogProcedureCache {
    session: Option<uuid::Uuid>,
    missing: HashMap<String, u8>,
}

impl CatalogProcedureCache {
    fn select(&mut self, session: uuid::Uuid, database: &str, procedure: Procedure) -> bool {
        if self.session != Some(session) {
            self.missing.clear();
            self.session = Some(session);
        }
        self.missing
            .get(database)
            .is_some_and(|bits| bits & procedure as u8 != 0)
    }

    fn remember(&mut self, session: uuid::Uuid, database: String, procedure: Procedure) {
        if self.session == Some(session) {
            *self.missing.entry(database).or_default() |= procedure as u8;
        }
    }
}

fn missing_procedure(error: &TdsError) -> bool {
    matches!(error, TdsError::SqlServerError { diagnostics }
        if diagnostics.errors.first().is_some_and(|error| error.number == 2812))
}

/// Select after recovery, and retry only a missing modern procedure. The
/// connection-bound RPC prevents a stale choice from reaching a recovered session.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute(
    dbc: &DbcHandle,
    client: &mut TdsClient,
    legacy: &str,
    database: Option<&str>,
    build_params: impl Fn() -> (Vec<RpcParameter>, Option<Vec<RpcParameter>>),
    qualify: impl Fn(&str) -> String,
    budget: u32,
    started: Instant,
    cancel: &CancelHandle,
) -> TdsResult<StatementResult> {
    let mut retry_legacy = false;
    loop {
        let session = client.client_connection_id();
        let procedure = Procedure::from_legacy(legacy);
        let effective_database = database.unwrap_or_else(|| client.database());
        let modern = if !retry_legacy && client.negotiated_vector_version() != 0 {
            if let Some(procedure) = procedure {
                let mut state = dbc.inner.lock().map_err(|_| {
                    error!("catalog procedure cache: DBC mutex poisoned");
                    TdsError::UsageError("Catalog procedure cache mutex poisoned".into())
                })?;
                !state
                    .catalog_procedures
                    .select(session, effective_database, procedure)
            } else {
                false
            }
        } else {
            false
        };
        // Only modern calls can add a negative entry; preserve the pre-send
        // database even if the response contains a database ENVCHANGE.
        let cache_database = modern.then(|| effective_database.to_owned());
        let name = match (modern, procedure) {
            (true, Some(procedure)) => procedure.modern(),
            _ => legacy,
        };
        let timeout = deduct_query_timeout(budget, started.elapsed())
            .map_err(|()| query_timeout_expired_error())?;
        let (positional, named) = build_params();
        let result = dbc
            .runtime
            .block_on(client.execute_stored_procedure_on_connection(
                qualify(name),
                Some(positional),
                named,
                ExecuteOptions::new().timeout_secs(timeout).cancel(cancel),
                session,
            ));
        if matches!(&result, Err(TdsError::UsageError(_)))
            && client.client_connection_id() != session
        {
            retry_legacy = false;
            continue;
        }
        if modern && result.as_ref().is_err_and(missing_procedure) {
            if let (Some(procedure), Some(database)) = (procedure, cache_database) {
                let mut state = dbc.inner.lock().map_err(|_| {
                    error!("catalog procedure cache: DBC mutex poisoned");
                    TdsError::UsageError("Catalog procedure cache mutex poisoned".into())
                })?;
                state
                    .catalog_procedures
                    .remember(session, database, procedure);
            }
            debug!(name, "catalog procedure missing; retrying legacy procedure");
            let _ = client.take_info_messages();
            retry_legacy = true;
            continue;
        }
        return result;
    }
}

#[cfg(test)]
mod tests;
