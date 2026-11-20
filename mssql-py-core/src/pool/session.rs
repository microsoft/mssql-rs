// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::connection::tds_client::{ExecuteOptions, TdsClient};
use mssql_tds::core::CancelHandle;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;

use super::connection::{ConnectSpec, ConnectionFactory, OpenedConnection, PoolKey};
use super::control::AcquireControl;
use super::error::{AcquireStage, PoolError, PoolResult};
use super::lifecycle::SlotGuard;
use super::manager::AsyncPoolManager;

/// A concrete isolation level; the protocol's NoChange is not valid at checkout.
pub(crate) struct Isolation(&'static str);

impl TryFrom<TransactionIsolationLevel> for Isolation {
    type Error = PoolError;

    fn try_from(level: TransactionIsolationLevel) -> PoolResult<Self> {
        Ok(Self(match level {
            TransactionIsolationLevel::ReadUncommitted => {
                "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED"
            }
            TransactionIsolationLevel::ReadCommitted => {
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED"
            }
            TransactionIsolationLevel::RepeatableRead => {
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ"
            }
            TransactionIsolationLevel::Serializable => {
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"
            }
            TransactionIsolationLevel::Snapshot => "SET TRANSACTION ISOLATION LEVEL SNAPSHOT",
            TransactionIsolationLevel::NoChange => {
                return Err(PoolError::InvalidRequest {
                    field: "isolation_level",
                    reason: "must select a concrete isolation level",
                });
            }
        }))
    }
}

struct OperationCancel(CancelHandle);

impl Drop for OperationCancel {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Drop always discards. Safe, awaited return is a separate implementation slice.
pub(crate) struct PoolLease {
    slot: SlotGuard<OpenedConnection>,
}

impl PoolLease {
    pub(crate) fn client_mut(&mut self) -> &mut TdsClient {
        self.slot
            .value_mut()
            .expect("published client")
            .client_mut()
    }

    #[cfg(test)]
    pub(crate) fn return_quiescent(self) -> PoolResult<()> {
        self.slot.return_quiescent()
    }
}

fn quiescent(client: &TdsClient) -> bool {
    !client.is_connection_dead()
        && !client.reset_pending()
        && !client.has_open_batch()
        && !client.has_active_transaction()
        && !client.is_session_recovery_enabled()
}

async fn initialize(
    mut connection: OpenedConnection,
    spec: &ConnectSpec,
    isolation: &Isolation,
    reuse: bool,
) -> PoolResult<OpenedConnection> {
    connection.verify_auth(spec)?;
    let client = connection.client_mut();
    if !quiescent(client) {
        return Err(PoolError::InvalidSession);
    }
    let cancel = OperationCancel(CancelHandle::new());
    if reuse {
        client.prepare_reset_connection(false);
    }
    client
        .execute(
            isolation.0.to_owned(),
            ExecuteOptions::new().cancel(&cancel.0),
        )
        .await
        .map_err(|error| PoolError::Initialize(Box::new(error)))?;
    client
        .close_query()
        .await
        .map_err(|error| PoolError::Initialize(Box::new(error)))?;
    if !quiescent(client) {
        return Err(PoolError::InvalidSession);
    }
    Ok(connection)
}

impl AsyncPoolManager {
    pub(crate) async fn acquire(
        &self,
        key: PoolKey,
        spec: &ConnectSpec,
        factory: &dyn ConnectionFactory,
        isolation: Isolation,
        control: AcquireControl,
    ) -> PoolResult<Option<PoolLease>> {
        spec.validate_pool()?;
        let Some(mut slot) = self.reserve(key, control).await? else {
            return Ok(None);
        };
        let mut remaining = slot.candidate_limit()?;
        while let Some(connection) = slot.take_value() {
            let result = slot
                .run(
                    AcquireStage::Reset,
                    initialize(connection, spec, &isolation, true),
                )
                .await;
            match result {
                Ok(connection) => {
                    slot.install(connection)?;
                    slot.publish()?;
                    return Ok(Some(PoolLease { slot }));
                }
                Err(error @ (PoolError::Initialize(_) | PoolError::InvalidSession)) => {
                    tracing::debug!(%error, "discarding invalid pool candidate");
                }
                Err(error) => return Err(error),
            }
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
            slot = slot.retry().await?;
        }
        let connection = slot
            .run(AcquireStage::Open, async {
                let cancel = OperationCancel(CancelHandle::new());
                factory.open(spec, &cancel.0).await.map_err(PoolError::from)
            })
            .await?;
        let connection = slot
            .run(
                AcquireStage::Isolation,
                initialize(connection, spec, &isolation, false),
            )
            .await?;
        slot.install(connection)?;
        slot.publish()?;
        Ok(Some(PoolLease { slot }))
    }
}

#[cfg(test)]
mod tests;
