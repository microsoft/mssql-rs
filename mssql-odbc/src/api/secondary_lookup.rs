// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::Duration;

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::TdsResult;
use mssql_tds::error::{Error as TdsError, TimeoutErrorType};
use tracing::debug;

use crate::handles::dbc::DbcState;

pub(super) enum Route {
    Session,
    Secondary {
        context: Box<ClientContext>,
        catalog: String,
        database_change_count: u64,
    },
}

pub(super) fn select_route(state: &DbcState, client: &TdsClient) -> Option<Route> {
    if state.wire.statement_owner().is_none() {
        return Some(Route::Session);
    }
    let context = client.secondary_client_context()?;
    if prompts_for_sign_in(&context) {
        return None;
    }
    Some(Route::Secondary {
        context: Box::new(context),
        catalog: client.database().to_owned(),
        database_change_count: client.database_change_count(),
    })
}

pub(super) fn cache_is_current(
    catalog: &str,
    database_change_count: u64,
    client: &TdsClient,
) -> bool {
    catalog == client.database() && database_change_count == client.database_change_count()
}

// SpawnConnection does not replay Authentication, so metadata must not prompt.
pub(super) fn prompts_for_sign_in(context: &ClientContext) -> bool {
    matches!(
        context.tds_authentication_method,
        TdsAuthenticationMethod::ActiveDirectoryInteractive
            | TdsAuthenticationMethod::ActiveDirectoryDeviceCodeFlow
    )
}

/// One deadline covers login and read. Box the work polled on the application's
/// thread to preserve the small-stack budget.
pub(super) async fn read_on_secondary<T>(
    context: Box<ClientContext>,
    timeout_secs: u32,
    timeout_message: &'static str,
    read: impl AsyncFnOnce(&mut TdsClient, u32) -> TdsResult<T>,
) -> TdsResult<T> {
    let lookup = Box::pin(async move {
        let data_source = context.data_source.clone();
        let mut client = TdsConnectionProvider::new()
            .create_client(*context, &data_source, None)
            .await?;
        debug!(
            client_connection_id = %client.client_connection_id(),
            timeout_message,
            "metadata lookup: logged in separately"
        );
        read(&mut client, 0).await
    });
    if timeout_secs == 0 {
        return lookup.await;
    }
    tokio::time::timeout(Duration::from_secs(u64::from(timeout_secs)), lookup)
        .await
        .unwrap_or_else(|_| {
            Err(TdsError::TimeoutError(TimeoutErrorType::String(
                timeout_message.to_string(),
            )))
        })
}
