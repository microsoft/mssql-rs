// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Revocable physical ownership shared by async connections, cursors, and work.

use std::sync::Arc;

use mssql_tds::connection::tds_client::TdsClient;
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};

use crate::async_session::{AsyncConnectionState, ClaimError, OperationId};

/// Unique physical ownership; pooled handles are a separate implementation slice.
pub(crate) enum ConnHandle {
    Direct(TdsClient),
}

impl ConnHandle {
    /// Borrow the physical client without transferring ownership out of its handle.
    pub(crate) fn client_mut(&mut self) -> &mut TdsClient {
        match self {
            Self::Direct(client) => client,
        }
    }
}

/// Last-owner destruction discards the client without asynchronous cleanup.
pub(crate) struct LogicalConnection {
    handle: Mutex<Option<ConnHandle>>,
    pub(crate) state: Arc<AsyncConnectionState>,
}

impl LogicalConnection {
    /// Create fresh logical state owning one direct client; no pool checkout occurs.
    pub(crate) fn new(client: TdsClient) -> Self {
        Self {
            handle: Mutex::new(Some(ConnHandle::Direct(client))),
            state: Arc::new(AsyncConnectionState::new()),
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

    /// Revoke physical access for surviving owners after the current borrow ends.
    /// This does not join detached operations or coordinate concurrent close callers.
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
        let operation = connection.state.claim_connection_operation().unwrap();
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
        let operation = connection.state.claim_connection_operation().unwrap();
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
        let operation = connection.state.claim_connection_operation().unwrap();
        let held = connection.handle.lock().await;
        let mut queued = Box::pin(connection.lock_operation(operation));
        assert!(matches!(
            queued
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        connection.state.release_operation(operation);
        let replacement = connection.state.claim_connection_operation().unwrap();
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
        let operation = connection.state.claim_connection_operation().unwrap();
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
        let operation = connection.state.claim_connection_operation().unwrap();
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
        let operation = connection.state.claim_connection_operation().unwrap();
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
            connection
                .state
                .cancel_fetch(fetch.operation_id, fetch.fetch_id)
        );
        assert_eq!(
            connection
                .state
                .finish_fetch(fetch.operation_id, fetch.fetch_id, false, true),
            crate::async_session::FetchCompletion::CancellationRequested
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
}
