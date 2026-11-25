// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use mssql_tds::connection::tds_client::{ExecuteOptions, TdsClient};
use mssql_tds::core::{CancelHandle, TdsResult};
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::auth::OperationAuth;
use super::connection::{AuthBinding, ConnectSpec, ConnectionFactory, OpenedConnection, PoolKey};
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
    pub(crate) fn activate_auth(
        &mut self,
        factory: crate::async_entra_token_factory::AsyncPythonEntraIdTokenFactory,
        current_binding: &AuthBinding,
        margin: Duration,
    ) -> PoolResult<OperationAuth> {
        if self.discard {
            return Err(PoolError::InvalidSession);
        }
        self.slot.runtime()?;
        self.slot
            .value_mut()
            .expect("published client")
            .recovery_auth()
            .ok_or(PoolError::InvalidRequest {
                field: "authentication",
                reason: "this lease has no operation-scoped credential callback",
            })?
            .activate(factory, current_binding, margin)
    }

    pub(crate) fn client_mut(&mut self) -> &mut TdsClient {
        self.slot
            .value_mut()
            .expect("published client")
            .client_mut()
    }

    pub(crate) fn mark_discard(&mut self) {
        self.discard = true;
    }

    /// The binding reader must observe account/credential epoch changes, not JWT claims.
    pub(crate) async fn run_authenticated<T>(
        &mut self,
        deadline: Option<Instant>,
        cancellation: &CancellationToken,
        auth: OperationAuth,
        current_binding: impl Fn() -> PoolResult<AuthBinding>,
        operation: impl for<'a> FnOnce(
            &'a mut TdsClient,
            &'a CancelHandle,
        )
            -> Pin<Box<dyn Future<Output = TdsResult<T>> + Send + 'a>>,
    ) -> PoolResult<T> {
        self.slot.runtime()?;
        if !self
            .slot
            .value_mut()
            .expect("published client")
            .recovery_auth()
            .is_some_and(|retained| retained.owns(&auth))
        {
            self.mark_discard();
            self.client_mut().mark_connection_dead();
            return Err(PoolError::IdentityMismatch);
        }
        if let Err(error) = current_binding().and_then(|binding| auth.check_binding(&binding)) {
            self.mark_discard();
            self.client_mut().mark_connection_dead();
            return Err(error);
        }
        let result = self.run(deadline, cancellation, operation).await;
        let metadata = current_binding().and_then(|binding| auth.complete(&binding));
        match metadata {
            Ok(Some(metadata)) => {
                if let Err(error) = self
                    .slot
                    .value_mut()
                    .expect("published client")
                    .record_auth(metadata)
                {
                    self.mark_discard();
                    self.client_mut().mark_connection_dead();
                    return result.and(Err(error));
                }
            }
            Ok(None) => {}
            Err(PoolError::TokenExpired) => {
                self.mark_discard();
                self.client_mut().mark_connection_dead();
                tracing::debug!("discarding authenticated pool session after token expiry");
            }
            Err(error) => {
                self.mark_discard();
                self.client_mut().mark_connection_dead();
                return result.and(Err(error));
            }
        }
        if cancellation.is_cancelled()
            || deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.mark_discard();
            self.client_mut().mark_connection_dead();
            return Err(if cancellation.is_cancelled() {
                PoolError::Cancelled
            } else {
                PoolError::AcquireTimeout {
                    stage: AcquireStage::Execute,
                }
            });
        }
        result
    }

    /// Retains capacity and marks interrupted work unusable before its first poll.
    /// An optional command deadline covers recovery, execution and result delivery.
    pub(crate) async fn run<T>(
        &mut self,
        deadline: Option<Instant>,
        cancellation: &CancellationToken,
        operation: impl for<'a> FnOnce(
            &'a mut TdsClient,
            &'a CancelHandle,
        )
            -> Pin<Box<dyn Future<Output = TdsResult<T>> + Send + 'a>>,
    ) -> PoolResult<T> {
        if self.discard {
            return Err(PoolError::InvalidSession);
        }
        let check = || {
            if cancellation.is_cancelled() {
                return Err(PoolError::Cancelled);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(PoolError::AcquireTimeout {
                    stage: AcquireStage::Execute,
                });
            }
            Ok(())
        };
        check()?;
        let runtime = self.slot.runtime()?;
        let guard = LeaseOperation(self);
        guard.0.discard = true;
        let cancel = OperationCancel(CancelHandle::new());
        let result = {
            let work = async {
                let elapsed = async {
                    if let Some(deadline) = deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(PoolError::Cancelled),
                    _ = elapsed => Err(PoolError::AcquireTimeout { stage: AcquireStage::Execute }),
                    result = operation(guard.0.client_mut(), &cancel.0) => {
                        check()?;
                        result.map_err(|error| PoolError::Execute(Box::new(error)))
                    }
                }
            };
            tokio::pin!(work);
            std::future::poll_fn(|cx| {
                if let Err(error) = check_runtime(runtime) {
                    return std::task::Poll::Ready(Err(error));
                }
                std::future::Future::poll(work.as_mut(), cx)
            })
            .await
        };
        let settled = match &result {
            Ok(_) => true,
            Err(PoolError::Execute(error)) => !matches!(
                error.as_ref(),
                mssql_tds::error::Error::OperationCancelledError(_)
                    | mssql_tds::error::Error::TokenAcquisitionCancelled
                    | mssql_tds::error::Error::TimeoutError(_)
            ),
            _ => false,
        };
        if settled && !guard.0.client_mut().is_connection_dead() {
            guard.0.discard = false;
        }
        result
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
                let mut maintenance = self.client_mut().suspend_session_recovery();
                let client = &mut *maintenance;
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

struct LeaseOperation<'a>(&'a mut PoolLease);

impl Drop for LeaseOperation<'_> {
    fn drop(&mut self) {
        if self.0.discard {
            self.0.client_mut().mark_connection_dead();
        }
    }
}

fn quiescent(client: &TdsClient) -> bool {
    !client.is_connection_dead()
        && !client.reset_pending()
        && !client.command_is_busy()
        && !client.has_active_transaction()
}

async fn initialize(
    mut connection: OpenedConnection,
    spec: &ConnectSpec,
    isolation: &Isolation,
    reuse: bool,
) -> PoolResult<OpenedConnection> {
    connection.verify_auth(spec)?;
    let mut maintenance = connection.client_mut().suspend_session_recovery();
    let client = &mut *maintenance;
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
    drop(maintenance);
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
