// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Revocable physical ownership shared by async connections, cursors, and work.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::core::TdsResult;
use mssql_tds::error::{Error, TimeoutErrorType};
use pyo3::prelude::*;
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard, watch};
use tokio::time::{Instant, timeout_at};
use tracing::instrument::WithSubscriber;

use crate::async_errors::{OperationalError, map_tds_error};
use crate::async_pool::map_pool_error;
use crate::async_session::{AsyncConnectionState, ClaimError, OperationId};
use crate::pool::session::PoolLease;

pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(crate) enum CloseMode {
    Rollback,
    Commit,
}

pub(crate) type CloseResult = Result<(), Arc<PyErr>>;
type CloseReceiver = watch::Receiver<Option<CloseResult>>;

pub(crate) fn check_cleanup_deadline(deadline: Instant) -> TdsResult<()> {
    if Instant::now() >= deadline {
        return Err(Error::TimeoutError(TimeoutErrorType::String(
            "Connection cleanup timed out; connection discarded".to_owned(),
        )));
    }
    Ok(())
}

struct CloseCompletion {
    state: Arc<AsyncConnectionState>,
    sender: watch::Sender<Option<CloseResult>>,
    completed: bool,
}

impl Drop for CloseCompletion {
    fn drop(&mut self) {
        if !self.completed {
            self.state.mark_broken();
            self.state.mark_closed();
            self.sender
                .send_replace(Some(Err(Arc::new(OperationalError::new_err(
                    "Connection cleanup was interrupted; connection discarded",
                )))));
        }
    }
}

/// Unique physical ownership behind one revocable logical cell.
pub(crate) enum ConnHandle {
    Direct(Box<TdsClient>),
    Pooled(Box<PoolLease>),
}

impl ConnHandle {
    /// Borrow the physical client without transferring ownership out of its handle.
    pub(crate) fn client_mut(&mut self) -> &mut TdsClient {
        match self {
            Self::Direct(client) => client,
            Self::Pooled(lease) => lease.client_mut(),
        }
    }
}

/// Last-owner destruction discards the client without asynchronous cleanup.
pub(crate) struct LogicalConnection {
    handle: Mutex<Option<ConnHandle>>,
    pub(crate) state: Arc<AsyncConnectionState>,
    close: StdMutex<Option<CloseReceiver>>,
    pub(crate) cleanup_timeout: Duration,
}

impl LogicalConnection {
    /// Create fresh logical state owning one direct client; no pool checkout occurs.
    pub(crate) fn new(client: TdsClient) -> Self {
        Self::from_handle(ConnHandle::Direct(Box::new(client)), CLOSE_TIMEOUT)
    }

    pub(crate) fn from_handle(handle: ConnHandle, cleanup_timeout: Duration) -> Self {
        Self {
            handle: Mutex::new(Some(handle)),
            state: Arc::new(AsyncConnectionState::new()),
            close: StdMutex::new(None),
            cleanup_timeout,
        }
    }

    /// Only used before Python delivery, when no application operation can hold access.
    pub(crate) fn discard_undelivered(&self) {
        self.state.mark_broken();
        match self.handle.try_lock() {
            Ok(mut handle) => {
                drop(handle.take());
            }
            Err(error) => {
                tracing::error!(%error, "Undelivered connection unexpectedly has an active borrower")
            }
        }
    }

    /// The first close owns teardown; cancelling a waiter does not cancel that owner.
    pub(crate) fn start_close(
        self: &Arc<Self>,
        mode: CloseMode,
        budget: Duration,
        dispatch: Option<tracing::Dispatch>,
    ) -> CloseReceiver {
        let mut close = self.close.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(receiver) = close.as_ref() {
            return receiver.clone();
        }
        let deadline = Instant::now() + budget;
        let mut tasks = self.state.take_close_tasks();
        let (sender, receiver) = watch::channel(None);
        *close = Some(receiver.clone());
        let connection = self.clone();
        let completion = CloseCompletion {
            state: self.state.clone(),
            sender,
            completed: false,
        };
        let future = async move {
            let mut completion = completion;
            let settled = timeout_at(deadline, async {
                while let Some(result) = tasks.join_next().await {
                    if let Err(error) = result {
                        if error.is_cancelled() {
                            tracing::debug!(%error, "Joining cancelled async operation during close");
                        } else {
                            connection.state.mark_broken();
                            tracing::warn!(%error, "Async operation interrupted during close");
                        }
                    }
                }
            })
            .await;
            let result = if settled.is_err() {
                connection.state.mark_broken();
                tasks.abort_all();
                // Join even after the deadline: an abort request is not destruction.
                while tasks.join_next().await.is_some() {}
                drop(connection.handle.lock().await.take());
                Err(OperationalError::new_err(
                    "Connection cleanup timed out while settling operations; connection discarded",
                ))
            } else {
                let handle = connection.handle.lock().await.take();
                match handle {
                    None => Ok(None),
                    Some(handle) if connection.state.is_broken() => {
                        drop(handle);
                        match mode {
                            CloseMode::Commit => Err(OperationalError::new_err(
                                "Cannot commit a broken connection; connection discarded",
                            )),
                            CloseMode::Rollback => Ok(None),
                        }
                    }
                    Some(handle) => {
                        match timeout_at(
                            deadline,
                            Self::finalize_handle(handle, &connection.state, mode, deadline),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(OperationalError::new_err(
                                "Connection cleanup timed out; connection discarded",
                            )),
                        }
                    }
                }
            };
            connection.state.complete_close(
                mode,
                deadline,
                result,
                |returned, discard| {
                    if let Some((mut lease, deadline)) = returned {
                        if discard {
                            lease.mark_discard();
                        }
                        lease.finish_release(deadline).map_err(map_pool_error)?;
                    }
                    Ok(())
                },
                &completion.sender,
            );
            completion.completed = true;
            tracing::info!(
                discard = connection.state.must_discard(),
                "PyAsyncConnection::close: connection closed"
            );
        };
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            match dispatch {
                Some(dispatch) => future.with_subscriber(dispatch).await,
                None => future.await,
            }
        });
        receiver
    }

    pub(crate) async fn wait_closed(mut receiver: CloseReceiver) -> PyResult<()> {
        loop {
            let result = receiver.borrow_and_update().clone();
            if let Some(result) = result {
                return result.map_err(|error| {
                    Python::attach(|py| {
                        let original = error.value(py);
                        let value = original.get_type().call1((original.str()?,))?;
                        for attribute in ["sql_errors", "info_messages", "pool_reason"] {
                            if original.hasattr(attribute)? {
                                value.setattr(attribute, original.getattr(attribute)?)?;
                            }
                        }
                        Ok::<_, PyErr>(PyErr::from_value(value))
                    })
                    .unwrap_or_else(|error| error)
                });
            }
            receiver.changed().await.map_err(|_| {
                OperationalError::new_err("Connection cleanup ended without an outcome")
            })?;
        }
    }

    /// Validate queued claims only after gaining exclusive physical access.
    pub(crate) async fn lock_operation(
        &self,
        operation_id: OperationId,
    ) -> Result<MappedMutexGuard<'_, TdsClient>, ClaimError> {
        let handle = self.handle.lock().await;
        let client =
            MutexGuard::try_map(handle, |handle| handle.as_mut().map(ConnHandle::client_mut))
                .map_err(|_| ClaimError::Closed)?;
        self.state.validate_operation(operation_id)?;
        Ok(client)
    }

    async fn finalize_handle(
        mut handle: ConnHandle,
        state: &AsyncConnectionState,
        mode: CloseMode,
        deadline: Instant,
    ) -> PyResult<Option<(Box<PoolLease>, Instant)>> {
        let client = handle.client_mut();
        let result = async {
            check_cleanup_deadline(deadline)?;
            client.close_query().await?;
            check_cleanup_deadline(deadline)
        }
        .await;
        result.map_err(|error| {
            map_tds_error(
                "Connection cleanup failed",
                error,
                client.take_info_messages(),
            )
        })?;
        // This mutex-protected check authorizes finalization after draining.
        if state.is_broken_before_deadline(deadline).map_err(|error| {
            map_tds_error(
                "Connection cleanup failed",
                error,
                client.take_info_messages(),
            )
        })? {
            return match mode {
                CloseMode::Commit => Err(OperationalError::new_err(
                    "Cannot commit a broken connection; connection discarded",
                )),
                CloseMode::Rollback => Ok(None),
            };
        }
        let result = async {
            if client.has_active_transaction() {
                match mode {
                    CloseMode::Commit => client.commit_transaction(None, None).await?,
                    CloseMode::Rollback => client.rollback_transaction(None, None).await?,
                }
            }
            check_cleanup_deadline(deadline)
        }
        .await;
        result.map_err(|error| {
            map_tds_error(
                "Connection cleanup failed",
                error,
                client.take_info_messages(),
            )
        })?;
        match handle {
            ConnHandle::Direct(mut client) => {
                client.close_connection().await.map_err(|error| {
                    map_tds_error(
                        "Connection cleanup failed",
                        error,
                        client.take_info_messages(),
                    )
                })?;
                check_cleanup_deadline(deadline)
                    .map(|()| None)
                    .map_err(|error| {
                        map_tds_error(
                            "Connection cleanup failed",
                            error,
                            client.take_info_messages(),
                        )
                    })
            }
            ConnHandle::Pooled(mut lease) => {
                if state.must_discard() {
                    lease.mark_discard();
                }
                let deadline = lease
                    .prepare_release(deadline)
                    .await
                    .map_err(map_pool_error)?;
                Ok(Some((lease, deadline)))
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn take_for_close(&self) -> Option<ConnHandle> {
        self.state.begin_close();
        self.handle.lock().await.take()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use mssql_tds::test_client_support::tds_client_from_tokens_capturing;

    use super::*;

    fn connection() -> (Arc<LogicalConnection>, Arc<std::sync::Mutex<Vec<u8>>>) {
        let (client, transport) = tds_client_from_tokens_capturing(Vec::new());
        (Arc::new(LogicalConnection::new(client)), transport)
    }

    #[tokio::test]
    async fn valid_operation_borrows_without_transferring_ownership() {
        let (connection, transport) = connection();
        let (operation, _) = connection.state.claim_connection_operation().unwrap();
        let client = connection.lock_operation(operation).await.unwrap();
        assert!(!client.has_open_batch());
        assert_eq!(Arc::strong_count(&transport), 2);
        drop(client);
        connection.state.release_operation(operation);
        assert!(connection.handle.lock().await.is_some());
    }

    #[tokio::test]
    async fn queued_operation_is_rejected_after_close_begins() {
        let (connection, _) = connection();
        let (operation, _) = connection.state.claim_connection_operation().unwrap();
        let held = connection.handle.lock().await;
        let mut queued = Box::pin(connection.lock_operation(operation));
        assert!(matches!(
            queued
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        connection.state.begin_close();
        drop(held);
        assert!(matches!(queued.await, Err(ClaimError::Closing)));
    }

    #[tokio::test]
    async fn queued_operation_is_rejected_after_ownership_changes() {
        let (connection, _) = connection();
        let (operation, _) = connection.state.claim_connection_operation().unwrap();
        let held = connection.handle.lock().await;
        let mut queued = Box::pin(connection.lock_operation(operation));
        assert!(matches!(
            queued
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        connection.state.release_operation(operation);
        let (replacement, _) = connection.state.claim_connection_operation().unwrap();
        drop(held);
        assert!(matches!(queued.await, Err(ClaimError::Busy)));
        assert!(connection.lock_operation(replacement).await.is_ok());
    }

    #[tokio::test]
    async fn missing_claim_and_terminal_lifecycle_cannot_borrow() {
        let (connection, _) = connection();
        assert!(matches!(
            connection.lock_operation(0).await,
            Err(ClaimError::Busy)
        ));
        let (operation, _) = connection.state.claim_connection_operation().unwrap();
        connection.state.mark_broken();
        assert!(matches!(
            connection.lock_operation(operation).await,
            Err(ClaimError::Broken)
        ));
        connection.state.mark_closed();
        assert!(matches!(
            connection.lock_operation(operation).await,
            Err(ClaimError::Closed)
        ));
    }

    #[tokio::test]
    async fn close_waits_for_the_current_borrow_before_removing_the_handle() {
        let (connection, transport) = connection();
        let (operation, _) = connection.state.claim_connection_operation().unwrap();
        let held = connection.lock_operation(operation).await.unwrap();
        let mut closing = Box::pin(connection.take_for_close());
        assert!(matches!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        assert_eq!(connection.state.ensure_open(), Err(ClaimError::Closing));
        assert_eq!(Arc::strong_count(&transport), 2);
        drop(held);
        let handle = closing.await.unwrap();
        assert!(connection.handle.lock().await.is_none());
        drop(handle);
        assert_eq!(Arc::strong_count(&transport), 1);
    }

    #[tokio::test]
    async fn closed_cell_cannot_lend_the_removed_client() {
        let (connection, transport) = connection();
        let (operation, _) = connection.state.claim_connection_operation().unwrap();
        let cursor = connection.clone();
        let handle = connection.take_for_close().await.unwrap();
        connection.state.mark_closed();
        assert!(matches!(
            cursor.lock_operation(operation).await,
            Err(ClaimError::Closed)
        ));
        assert_eq!(Arc::strong_count(&transport), 2);
        drop(handle);
        assert_eq!(Arc::strong_count(&transport), 1);
        assert!(connection.take_for_close().await.is_none());
    }

    #[tokio::test]
    async fn late_fetch_cancellation_cannot_reacquire_a_revoked_client() {
        let (connection, transport) = connection();
        let execute = connection.state.claim_execute(1).unwrap();
        connection.state.finish_execute(execute.operation_id, true);
        let fetch = connection.state.claim_fetch(1).unwrap();
        drop(connection.lock_operation(fetch.operation_id).await.unwrap());

        let handle = connection.take_for_close().await.unwrap();
        connection.state.mark_closed();
        drop(handle);
        assert_eq!(Arc::strong_count(&transport), 1);
        assert!(
            !connection
                .state
                .cancel_fetch(fetch.operation_id, fetch.fetch_id)
        );
        assert_eq!(
            connection
                .state
                .finish_fetch(fetch.operation_id, fetch.fetch_id, false, true),
            crate::async_session::FetchCompletion::NoLongerActive
        );
        assert!(matches!(
            connection.lock_operation(fetch.operation_id).await,
            Err(ClaimError::Closed)
        ));
        connection.state.release_operation(fetch.operation_id);
        assert_eq!(connection.state.ensure_open(), Err(ClaimError::Closed));
        assert!(connection.handle.lock().await.is_none());
        assert!(transport.lock().unwrap().is_empty());
    }

    #[test]
    fn retained_cursor_keeps_client_until_last_owner_drops() {
        let (connection, transport) = connection();
        let cursor = connection.clone();
        let sibling = connection.clone();
        let weak = Arc::downgrade(&connection);
        drop(connection);
        assert_eq!(Arc::strong_count(&transport), 2);
        drop(cursor);
        assert_eq!(Arc::strong_count(&transport), 2);
        assert!(sibling.state.ensure_open().is_ok());
        drop(sibling);
        assert!(weak.upgrade().is_none());
        assert_eq!(Arc::strong_count(&transport), 1);
    }

    #[tokio::test]
    async fn concurrent_close_waits_for_registered_work_even_after_waiter_is_dropped() {
        let (connection, transport) = connection();
        let (operation, _) = connection.state.claim_connection_operation().unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let worker = connection.clone();
        let (result, _) = connection
            .state
            .spawn_task(async move {
                let _client = worker.lock_operation(operation).await.unwrap();
                started.send(()).unwrap();
                wait.await.unwrap();
            })
            .unwrap();
        ready.await.unwrap();
        let first = connection.start_close(CloseMode::Rollback, CLOSE_TIMEOUT, None);
        assert!(first.borrow().is_none());
        drop(first);
        let second = connection.start_close(CloseMode::Commit, Duration::ZERO, None);
        assert!(second.borrow().is_none());
        assert_eq!(connection.state.ensure_open(), Err(ClaimError::Closing));
        release.send(()).unwrap();
        LogicalConnection::wait_closed(second).await.unwrap();
        result.await.unwrap();
        assert_eq!(Arc::strong_count(&transport), 1);
        assert!(connection.handle.lock().await.is_none());
        assert!(transport.lock().unwrap().is_empty());
        connection.state.mark_broken();
        assert_eq!(connection.state.ensure_open(), Err(ClaimError::Closed));
    }

    #[tokio::test]
    async fn cancelled_preflight_does_not_break_clean_close() {
        let (connection, transport) = connection();
        let (started, ready) = tokio::sync::oneshot::channel();
        let pending = tokio::spawn(connection.state.clone().run_tracked_preflight(
            move |_| async move {
                started.send(()).unwrap();
                std::future::pending::<PyResult<()>>().await
            },
        ));
        ready.await.unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());

        LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Commit,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap();
        assert!(!connection.state.is_broken());
        assert!(connection.handle.lock().await.is_none());
        assert_eq!(Arc::strong_count(&transport), 1);
    }

    #[tokio::test]
    async fn cancelled_completed_execution_never_commits_on_clean_exit() {
        use crate::async_session::SessionOperationGuard;
        use mssql_tds::connection::tds_client::ExecuteOptions;
        use mssql_tds::test_client_support::{
            done_no_more, env_change_begin_transaction, env_change_commit_transaction,
        };

        let (client, transport) = tds_client_from_tokens_capturing(vec![
            env_change_begin_transaction(7),
            done_no_more(),
            env_change_commit_transaction(),
            done_no_more(),
        ]);
        let connection = Arc::new(LogicalConnection::new(client));
        let worker = connection.clone();
        let (release, wait) = tokio::sync::oneshot::channel();
        let (completed, completion) = tokio::sync::oneshot::channel();
        let mut waiter = Box::pin(connection.state.clone().run_tracked_preflight(
            move |preflight| async move {
                wait.await.unwrap();
                let claim = preflight.claim_execute(1).unwrap();
                let mut guard =
                    SessionOperationGuard::new(worker.state.clone(), claim.operation_id);
                {
                    let mut client = worker.lock_operation(claim.operation_id).await.unwrap();
                    client
                        .execute(
                            "INSERT INTO test VALUES (1)".into(),
                            ExecuteOptions::default(),
                        )
                        .await
                        .unwrap();
                    client.close_query().await.unwrap();
                    assert!(client.has_active_transaction());
                }
                guard.finish_execute(false);
                completed.send(()).unwrap();
                Ok(())
            },
        ));
        assert!(
            waiter
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        release.send(()).unwrap();
        completion.await.unwrap();
        let execution_bytes = transport.lock().unwrap().len();
        drop(waiter);

        let error = LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Commit,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Cannot commit a broken connection")
        );
        assert_eq!(transport.lock().unwrap().len(), execution_bytes);
        assert_eq!(Arc::strong_count(&transport), 1);
        assert!(connection.handle.lock().await.is_none());
    }

    #[tokio::test]
    async fn late_waiter_cancellation_cannot_commit_or_publish_success() {
        use std::pin::Pin;
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::async_session::SessionOperationGuard;
        use mssql_tds::connection::tds_client::ExecuteOptions;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_no_more, env_change_begin_transaction,
            env_change_commit_transaction,
        };
        use tracing::{Subscriber, span::Attributes};
        use tracing_subscriber::{Layer, layer::Context as LayerContext, prelude::*};

        type Waiter = Pin<Box<dyn Future<Output = PyResult<()>> + Send>>;
        struct CancelAtPhase {
            phase: &'static str,
            waiter: Arc<StdMutex<Option<Waiter>>>,
            cancelled: Arc<AtomicBool>,
        }
        impl<S: Subscriber> Layer<S> for CancelAtPhase {
            fn on_new_span(
                &self,
                attributes: &Attributes<'_>,
                _id: &tracing::span::Id,
                _context: LayerContext<'_, S>,
            ) {
                if attributes.metadata().name() == self.phase {
                    let waiter = self.waiter.lock().unwrap().take().unwrap();
                    drop(waiter);
                    self.cancelled.store(true, Ordering::Release);
                }
            }
        }

        for (phase, mode) in [
            ("close_query", CloseMode::Commit),
            ("commit_transaction", CloseMode::Commit),
            ("close_connection", CloseMode::Commit),
            ("close_query", CloseMode::Rollback),
        ] {
            let (mut client, transport) = tds_client_from_tokens_capturing(vec![
                env_change_begin_transaction(7),
                col_metadata_empty(),
                done_no_more(),
                env_change_commit_transaction(),
                done_no_more(),
            ]);
            client
                .execute("SELECT 1".into(), ExecuteOptions::default())
                .await
                .unwrap();
            assert!(client.has_active_transaction());
            assert!(client.has_open_batch());
            let initial_bytes = transport.lock().unwrap().len();
            let connection = Arc::new(LogicalConnection::new(client));
            let state = connection.state.clone();
            let (release, wait) = tokio::sync::oneshot::channel();
            let (completed, completion) = tokio::sync::oneshot::channel();
            let mut waiter: Waiter = Box::pin(state.clone().run_tracked_preflight(
                move |preflight| async move {
                    wait.await.unwrap();
                    let claim = preflight.claim_execute(1).unwrap();
                    let mut guard = SessionOperationGuard::new(state, claim.operation_id);
                    guard.finish_execute(true);
                    completed.send(()).unwrap();
                    Ok(())
                },
            ));
            assert!(
                waiter
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            release.send(()).unwrap();
            completion.await.unwrap();
            let cancelled = Arc::new(AtomicBool::new(false));
            let subscriber = tracing_subscriber::Registry::default().with(CancelAtPhase {
                phase,
                waiter: Arc::new(StdMutex::new(Some(waiter))),
                cancelled: cancelled.clone(),
            });
            let result = LogicalConnection::wait_closed(connection.start_close(
                mode,
                CLOSE_TIMEOUT,
                Some(tracing::Dispatch::new(subscriber)),
            ))
            .await;
            assert!(cancelled.load(Ordering::Acquire), "{phase}");
            match mode {
                CloseMode::Commit => {
                    let error = result.unwrap_err();
                    let expected = if phase == "close_query" {
                        "Cannot commit a broken connection"
                    } else {
                        "outcome may be unknown"
                    };
                    assert!(error.to_string().contains(expected), "{phase}: {error}");
                    Python::attach(|py| assert!(error.is_instance_of::<OperationalError>(py)));
                    let repeated = LogicalConnection::wait_closed(connection.start_close(
                        CloseMode::Rollback,
                        Duration::ZERO,
                        None,
                    ))
                    .await
                    .unwrap_err();
                    assert_eq!(repeated.to_string(), error.to_string());
                }
                CloseMode::Rollback => result.unwrap(),
            }
            assert!(connection.state.is_broken());
            if phase == "close_query" {
                assert_eq!(transport.lock().unwrap().len(), initial_bytes);
            } else {
                assert!(transport.lock().unwrap().len() > initial_bytes);
            }
            assert_eq!(Arc::strong_count(&transport), 1);
            assert!(connection.handle.lock().await.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn close_publication_rechecks_cancellation_and_original_deadline() {
        use crate::async_session::ConnectionLifecycle;

        for (mode, cancelled, expired, failed) in [
            (CloseMode::Commit, true, false, false),
            (CloseMode::Rollback, true, false, false),
            (CloseMode::Commit, false, true, false),
            (CloseMode::Commit, false, false, false),
            (CloseMode::Commit, true, true, true),
        ] {
            let state = AsyncConnectionState::new();
            state.begin_close();
            let deadline = Instant::now() + Duration::from_secs(1);
            if cancelled {
                state.mark_broken();
            }
            if expired {
                tokio::time::advance(Duration::from_secs(1)).await;
            }
            let result = if failed {
                Err(OperationalError::new_err("original cleanup failure"))
            } else {
                Ok(())
            };
            let (sender, receiver) = watch::channel(None);
            state.complete_close(mode, deadline, result, |(), _| Ok(()), &sender);
            assert_eq!(state.lifecycle(), ConnectionLifecycle::Closed);
            let outcome = receiver.borrow().clone().unwrap();
            if failed || expired || cancelled && matches!(mode, CloseMode::Commit) {
                let error = outcome.unwrap_err();
                let expected = if failed {
                    "original cleanup failure"
                } else if expired {
                    "timed out"
                } else {
                    "outcome may be unknown"
                };
                assert!(error.to_string().contains(expected));
                assert!(state.is_broken());
                assert!(state.must_discard());
            } else {
                outcome.unwrap();
                state.mark_broken();
                assert!(receiver.borrow().as_ref().unwrap().is_ok());
            }
        }
    }

    #[tokio::test]
    async fn close_timeout_joins_task_destruction_before_reporting_outcome() {
        let (connection, transport) = connection();
        let (operation, _) = connection.state.claim_connection_operation().unwrap();
        let worker = connection.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (result, _) = connection
            .state
            .spawn_task(async move {
                let _client = worker.lock_operation(operation).await.unwrap();
                started.send(()).unwrap();
                std::future::pending::<()>().await;
            })
            .unwrap();
        ready.await.unwrap();
        let receiver = connection.start_close(CloseMode::Commit, Duration::from_millis(10), None);
        let error = LogicalConnection::wait_closed(receiver).await.unwrap_err();
        Python::attach(|py| assert!(error.is_instance_of::<OperationalError>(py)));
        assert!(result.await.is_err());
        assert_eq!(Arc::strong_count(&transport), 1);
        assert!(connection.handle.lock().await.is_none());
        let error = LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Rollback,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn expired_cleanup_budget_cannot_send_a_commit_or_report_success() {
        use mssql_tds::connection::tds_client::ExecuteOptions;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_no_more, env_change_begin_transaction,
            env_change_commit_transaction,
        };
        let (mut client, transport) = tds_client_from_tokens_capturing(vec![
            env_change_begin_transaction(7),
            col_metadata_empty(),
            done_no_more(),
            env_change_commit_transaction(),
            done_no_more(),
        ]);
        client
            .execute("SELECT 1".into(), ExecuteOptions::default())
            .await
            .unwrap();
        let initial_bytes = transport.lock().unwrap().len();
        let connection = Arc::new(LogicalConnection::new(client));
        let error = LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Commit,
            Duration::from_nanos(1),
            None,
        ))
        .await
        .unwrap_err();
        Python::attach(|py| assert!(error.is_instance_of::<OperationalError>(py)));
        assert!(error.to_string().contains("timed out"));
        assert_eq!(transport.lock().unwrap().len(), initial_bytes);
        assert_eq!(Arc::strong_count(&transport), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn drained_client_keeps_the_original_cleanup_deadline() {
        use mssql_tds::connection::tds_client::ExecuteOptions;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_no_more, env_change_begin_transaction,
            env_change_commit_transaction,
        };
        let (mut client, transport) = tds_client_from_tokens_capturing(vec![
            env_change_begin_transaction(7),
            col_metadata_empty(),
            done_no_more(),
            env_change_commit_transaction(),
            done_no_more(),
        ]);
        client
            .execute("SELECT 1".into(), ExecuteOptions::default())
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        client.close_query().await.unwrap();
        let initial_bytes = transport.lock().unwrap().len();
        tokio::time::advance(Duration::from_secs(2)).await;
        let error = LogicalConnection::finalize_handle(
            ConnHandle::Direct(Box::new(client)),
            &AsyncConnectionState::new(),
            CloseMode::Commit,
            deadline,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(transport.lock().unwrap().len(), initial_bytes);
        assert_eq!(Arc::strong_count(&transport), 1);
    }

    #[tokio::test]
    async fn broken_clean_exit_reports_failure_without_sending_a_commit() {
        for mode in [CloseMode::Commit, CloseMode::Rollback] {
            let (connection, transport) = connection();
            connection.state.mark_broken();
            let result =
                LogicalConnection::wait_closed(connection.start_close(mode, CLOSE_TIMEOUT, None))
                    .await;
            match mode {
                CloseMode::Commit => {
                    let error = result.unwrap_err();
                    Python::attach(|py| assert!(error.is_instance_of::<OperationalError>(py)));
                    assert!(
                        error
                            .to_string()
                            .contains("Cannot commit a broken connection")
                    );
                }
                CloseMode::Rollback => result.unwrap(),
            }
            assert_eq!(Arc::strong_count(&transport), 1);
            assert!(transport.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn close_takes_dormant_results_and_drains_before_transaction_completion() {
        use mssql_tds::connection::tds_client::ExecuteOptions;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_no_more, env_change_begin_transaction,
            env_change_commit_transaction, env_change_rollback_transaction,
        };
        for mode in [CloseMode::Commit, CloseMode::Rollback] {
            let ack = match mode {
                CloseMode::Commit => env_change_commit_transaction(),
                CloseMode::Rollback => env_change_rollback_transaction(),
            };
            let (mut client, transport) = tds_client_from_tokens_capturing(vec![
                env_change_begin_transaction(7),
                col_metadata_empty(),
                done_no_more(),
                ack,
                done_no_more(),
            ]);
            client
                .execute("SELECT 1".into(), ExecuteOptions::default())
                .await
                .unwrap();
            assert!(client.has_open_batch());
            assert!(client.has_active_transaction());
            let connection = Arc::new(LogicalConnection::new(client));
            let claim = connection.state.claim_execute(1).unwrap();
            connection.state.finish_execute(claim.operation_id, true);
            LogicalConnection::wait_closed(connection.start_close(mode, CLOSE_TIMEOUT, None))
                .await
                .unwrap();
            assert!(!connection.state.must_discard());
            assert_eq!(Arc::strong_count(&transport), 1);
            let bytes = transport.lock().unwrap();
            let first_packet_length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
            assert_eq!(
                bytes[first_packet_length], 14,
                "transaction request follows drain"
            );
        }
    }

    #[tokio::test]
    async fn failed_commit_is_not_retried_or_followed_by_rollback() {
        use mssql_tds::test_client_support::tds_client_from_tokens_in_transaction;
        let client = tds_client_from_tokens_in_transaction(Vec::new(), 7);
        let connection = Arc::new(LogicalConnection::new(client));
        let error = LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Commit,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Connection cleanup failed"));
        assert!(connection.state.must_discard());
        assert!(connection.handle.lock().await.is_none());
        let repeated = LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Rollback,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap_err();
        assert_eq!(repeated.to_string(), error.to_string());
    }

    #[tokio::test]
    async fn delayed_drain_error_never_sends_a_transaction_request() {
        use mssql_tds::connection::tds_client::ExecuteOptions;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_no_more, env_change_begin_transaction, sql_error,
        };
        let (mut client, transport) = tds_client_from_tokens_capturing(vec![
            env_change_begin_transaction(7),
            col_metadata_empty(),
            sql_error(50000, 16, "delayed failure"),
            done_no_more(),
        ]);
        client
            .execute("SELECT 1".into(), ExecuteOptions::default())
            .await
            .unwrap();
        let connection = Arc::new(LogicalConnection::new(client));
        let error = LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Commit,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap_err();
        assert!(error.to_string().contains("delayed failure"));
        let bytes = transport.lock().unwrap();
        let first_packet_length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        assert_eq!(
            bytes.len(),
            first_packet_length,
            "only the original batch was sent"
        );
        assert_eq!(Arc::strong_count(&transport), 1);
    }

    #[tokio::test]
    async fn settled_cancellation_does_not_silently_replace_a_clean_exit_commit() {
        use mssql_tds::connection::tds_client::ExecuteOptions;
        use mssql_tds::test_client_support::{
            col_metadata_empty, done_no_more, env_change_begin_transaction,
            env_change_commit_transaction,
        };
        let (mut client, transport) = tds_client_from_tokens_capturing(vec![
            env_change_begin_transaction(7),
            col_metadata_empty(),
            done_no_more(),
            env_change_commit_transaction(),
            done_no_more(),
        ]);
        client
            .execute("SELECT 1".into(), ExecuteOptions::default())
            .await
            .unwrap();
        let connection = Arc::new(LogicalConnection::new(client));
        let execute = connection.state.claim_execute(1).unwrap();
        connection.state.finish_execute(execute.operation_id, true);
        let fetch = connection.state.claim_fetch(1).unwrap();
        assert!(
            connection
                .state
                .cancel_fetch(fetch.operation_id, fetch.fetch_id)
        );
        connection.state.release_operation(fetch.operation_id);
        assert!(connection.state.must_discard());
        assert!(!connection.state.is_broken());
        LogicalConnection::wait_closed(connection.start_close(
            CloseMode::Commit,
            CLOSE_TIMEOUT,
            None,
        ))
        .await
        .unwrap();
        assert!(!connection.state.is_broken());
        assert!(connection.state.must_discard());
        let bytes = transport.lock().unwrap();
        let first_packet_length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        assert_eq!(
            bytes[first_packet_length], 14,
            "clean exit still sends its transaction request"
        );
    }
}
