// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::connection::tds_client::{ExecuteOptions, TdsClient};
use mssql_tds::core::CancelHandle;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use tokio::time::Instant;

use super::connection::{ConnectSpec, ConnectionFactory, OpenedConnection, PoolKey};
use super::control::AcquireControl;
use super::error::{AcquireStage, PoolError, PoolResult};
use super::lifecycle::SlotGuard;
use super::manager::{AsyncPoolManager, check_runtime};

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

/// Only successful awaited release permits reuse; Drop always discards.
pub(crate) struct PoolLease {
    slot: SlotGuard<OpenedConnection>,
    discard: bool,
}

impl PoolLease {
    pub(crate) fn client_mut(&mut self) -> &mut TdsClient {
        self.slot
            .value_mut()
            .expect("published client")
            .client_mut()
    }

    pub(crate) fn mark_discard(&mut self) {
        self.discard = true;
    }

    pub(crate) async fn release(self) -> PoolResult<()> {
        let deadline = self.slot.cleanup_deadline()?;
        self.release_with_deadline(deadline).await
    }

    pub(crate) async fn release_with_deadline(mut self, deadline: Instant) -> PoolResult<()> {
        let deadline = deadline.min(self.slot.cleanup_deadline()?);
        if Instant::now() >= deadline {
            return Err(PoolError::CleanupTimeout);
        }
        if self.discard {
            return Ok(());
        }
        let runtime = tokio::runtime::Handle::current().id();
        {
            // The slot owns the client throughout cleanup, including future drop.
            let cleanup = tokio::time::timeout_at(deadline, async {
                let client = self.client_mut();
                client
                    .close_query()
                    .await
                    .map_err(|error| PoolError::Cleanup(Box::new(error)))?;
                if client.command_is_busy() {
                    return Err(PoolError::Cleanup(Box::new(
                        mssql_tds::error::Error::ProtocolError(
                            "connection has an unfinished command after result drain".into(),
                        ),
                    )));
                }
                if client.has_active_transaction() {
                    let cancel = OperationCancel(CancelHandle::new());
                    client
                        .rollback_transaction_with_options(
                            None,
                            None,
                            ExecuteOptions::new().cancel(&cancel.0),
                        )
                        .await
                        .map_err(|error| PoolError::Cleanup(Box::new(error)))?;
                }
                if !quiescent(client) {
                    return Err(PoolError::Cleanup(Box::new(
                        mssql_tds::error::Error::ProtocolError(
                            "connection is not quiescent after cleanup".into(),
                        ),
                    )));
                }
                Ok(())
            });
            tokio::pin!(cleanup);
            std::future::poll_fn(|cx| {
                if let Err(error) = check_runtime(runtime) {
                    return std::task::Poll::Ready(Err(error));
                }
                std::future::Future::poll(cleanup.as_mut(), cx)
                    .map(|result| result.map_err(|_| PoolError::CleanupTimeout)?)
            })
            .await?;
        }
        self.slot.return_before(deadline)
    }

    #[cfg(test)]
    pub(crate) fn return_quiescent(self) -> PoolResult<()> {
        self.slot.return_quiescent()
    }
}

fn quiescent(client: &TdsClient) -> bool {
    !client.is_connection_dead()
        && !client.reset_pending()
        && !client.command_is_busy()
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
                    return Ok(Some(PoolLease {
                        slot,
                        discard: false,
                    }));
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
        Ok(Some(PoolLease {
            slot,
            discard: false,
        }))
    }
}

#[cfg(test)]
mod tests;
