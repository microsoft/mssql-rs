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
#[derive(Clone)]
pub(crate) struct Isolation(TransactionIsolationLevel);

impl TryFrom<TransactionIsolationLevel> for Isolation {
    type Error = PoolError;

    fn try_from(level: TransactionIsolationLevel) -> PoolResult<Self> {
        if matches!(level, TransactionIsolationLevel::NoChange) {
            return Err(PoolError::InvalidRequest {
                field: "isolation_level",
                reason: "must select a concrete isolation level",
            });
        }
        Ok(Self(level))
    }
}

impl Isolation {
    pub(crate) async fn apply(&self, client: &mut TdsClient) -> PoolResult<()> {
        let cancel = OperationCancel::new();
        client
            .initialize_session(self.0.clone(), ExecuteOptions::new().cancel(&cancel.handle))
            .await
            .map_err(|error| PoolError::Initialize(Box::new(error)))
    }
}

struct OperationCancel {
    handle: CancelHandle,
    cancel_on_drop: bool,
}

impl OperationCancel {
    fn new() -> Self {
        Self {
            handle: CancelHandle::new(),
            cancel_on_drop: true,
        }
    }

    fn disarm(&mut self) {
        self.cancel_on_drop = false;
    }
}

impl Drop for OperationCancel {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            self.handle.cancel();
        }
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
        current_binding: impl Fn() -> PoolResult<AuthBinding> + Send + Sync + 'static,
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
        operation: impl for<'a> FnOnce(
            &'a mut TdsClient,
            ExecuteOptions<'a>,
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
        if let Err(error) = auth.check_binding() {
            self.mark_discard();
            self.client_mut().mark_connection_dead();
            return Err(error);
        }
        let result = self
            .run_checked(deadline, cancellation, || auth.check_binding(), operation)
            .await;
        let metadata = auth.complete();
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
            ExecuteOptions<'a>,
        )
            -> Pin<Box<dyn Future<Output = TdsResult<T>> + Send + 'a>>,
    ) -> PoolResult<T> {
        self.run_checked(deadline, cancellation, || Ok(()), operation)
            .await
    }

    async fn run_checked<T>(
        &mut self,
        deadline: Option<Instant>,
        cancellation: &CancellationToken,
        validate: impl Fn() -> PoolResult<()>,
        operation: impl for<'a> FnOnce(
            &'a mut TdsClient,
            ExecuteOptions<'a>,
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
        let mut cancel = OperationCancel::new();
        let result = {
            let work = async {
                let options = ExecuteOptions {
                    // Round up for TDS seconds; the absolute timer remains authoritative.
                    timeout: deadline.map(|deadline| {
                        deadline
                            .saturating_duration_since(Instant::now())
                            .as_nanos()
                            .div_ceil(1_000_000_000)
                            .max(1)
                            .min(u128::from(u32::MAX)) as u32
                    }),
                    cancel: Some(&cancel.handle),
                    ..Default::default()
                };
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
                    result = operation(guard.0.client_mut(), options) => {
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
                if let Err(error) = check().and_then(|_| validate()) {
                    return std::task::Poll::Ready(Err(error));
                }
                std::future::Future::poll(work.as_mut(), cx)
            })
            .await
        };
        let settled = match &result {
            Ok(_) => true,
            // Other errors can leave an unread response before a batch is marked open.
            Err(PoolError::Execute(error)) => matches!(
                error.as_ref(),
                mssql_tds::error::Error::SqlServerError { .. }
            ),
            _ => false,
        };
        if settled && !guard.0.client_mut().is_connection_dead() {
            cancel.disarm();
            guard.0.discard = false;
        }
        result
    }

    pub(crate) async fn release(self) -> PoolResult<()> {
        let deadline = self.slot.cleanup_deadline()?;
        self.release_with_deadline(deadline).await
    }

    pub(crate) async fn release_with_deadline(mut self, deadline: Instant) -> PoolResult<()> {
        let deadline = self.prepare_release(deadline).await?;
        self.finish_release(deadline)
    }

    pub(crate) async fn prepare_release(&mut self, deadline: Instant) -> PoolResult<Instant> {
        let deadline = deadline.min(self.slot.cleanup_deadline()?);
        if Instant::now() >= deadline {
            return Err(PoolError::CleanupTimeout);
        }
        if self.discard {
            return Ok(deadline);
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
                    let cancel = OperationCancel::new();
                    client
                        .rollback_transaction_with_options(
                            None,
                            None,
                            ExecuteOptions::new().cancel(&cancel.handle),
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
        Ok(deadline)
    }

    pub(crate) fn finish_release(self, deadline: Instant) -> PoolResult<()> {
        if self.discard {
            return Ok(());
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
    let client = connection.client_mut();
    if !quiescent(client) {
        return Err(PoolError::InvalidSession);
    }
    if reuse {
        client.prepare_reset_connection(false);
    }
    isolation.apply(client).await?;
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
                let cancel = OperationCancel::new();
                factory
                    .open(spec, &cancel.handle)
                    .await
                    .map_err(PoolError::from)
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
