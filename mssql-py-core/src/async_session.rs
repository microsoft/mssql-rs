// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use mssql_tds::core::CancelHandle;

pub(crate) type CursorId = u64;
pub(crate) type OperationId = u64;

/// Exclusive execute ownership granted for one cursor operation.
#[derive(Debug)]
pub(crate) struct ExecuteClaim {
    pub(crate) operation_id: OperationId,
    pub(crate) cancel_handle: CancelHandle,
    pub(crate) drain_previous: bool,
}

/// Exclusive cursor-close ownership and its pending result-drain requirement.
#[derive(Debug)]
pub(crate) struct CursorCloseClaim {
    pub(crate) operation_id: OperationId,
    pub(crate) drain_previous: bool,
}

/// Exclusive ownership granted while one cursor row is being read.
#[derive(Debug)]
pub(crate) struct FetchClaim {
    pub(crate) operation_id: OperationId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClaimError {
    Closing,
    Closed,
    Broken,
    Busy,
    NoResultSet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionLifecycle {
    Open,
    Closing,
    Closed,
    Broken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationPhase {
    Executing,
    Fetching,
    FetchingRow,
    Closing,
}

/// The operation that currently owns the shared TDS session.
#[derive(Debug)]
pub(crate) struct ActiveOperation {
    pub(crate) cursor_id: Option<CursorId>,
    pub(crate) operation_id: OperationId,
    pub(crate) phase: OperationPhase,
    pub(crate) cancel_handle: Option<CancelHandle>,
    cancel_requested: bool,
}

/// Mutex-protected connection lifecycle and operation ownership state.
#[derive(Debug)]
struct AsyncSessionState {
    lifecycle: ConnectionLifecycle,
    active_operation: Option<ActiveOperation>,
}

/// Connection-wide coordinator shared by the connection and all its cursors.
#[derive(Debug)]
pub(crate) struct AsyncConnectionState {
    next_cursor_id: AtomicU64,
    next_operation_id: AtomicU64,
    inner: Mutex<AsyncSessionState>,
}

/// Releases an operation claim and poisons the session if interrupted.
pub(crate) struct SessionOperationGuard {
    session_state: Arc<AsyncConnectionState>,
    operation_id: OperationId,
    completed: bool,
}

impl SessionOperationGuard {
    pub(crate) fn new(session_state: Arc<AsyncConnectionState>, operation_id: OperationId) -> Self {
        Self {
            session_state,
            operation_id,
            completed: false,
        }
    }

    pub(crate) fn finish_execute(&mut self, has_open_batch: bool) {
        self.session_state
            .finish_execute(self.operation_id, has_open_batch);
        self.completed = true;
    }

    pub(crate) fn settle(&mut self, has_open_batch: bool) {
        if has_open_batch {
            self.session_state.mark_broken();
        }
        self.session_state.release_operation(self.operation_id);
        self.completed = true;
    }
}

impl Drop for SessionOperationGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.session_state.mark_broken();
            self.session_state.release_operation(self.operation_id);
        }
    }
}

impl AsyncConnectionState {
    pub(crate) fn new() -> Self {
        Self {
            next_cursor_id: AtomicU64::new(1),
            next_operation_id: AtomicU64::new(1),
            inner: Mutex::new(AsyncSessionState {
                lifecycle: ConnectionLifecycle::Open,
                active_operation: None,
            }),
        }
    }

    pub(crate) fn allocate_cursor_id(&self) -> CursorId {
        self.next_cursor_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn allocate_operation_id(&self) -> OperationId {
        self.next_operation_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn ensure_open(&self) -> Result<(), ClaimError> {
        match self.lock().lifecycle {
            ConnectionLifecycle::Open => Ok(()),
            ConnectionLifecycle::Closing => Err(ClaimError::Closing),
            ConnectionLifecycle::Closed => Err(ClaimError::Closed),
            ConnectionLifecycle::Broken => Err(ClaimError::Broken),
        }
    }

    pub(crate) fn claim_execute(&self, cursor_id: CursorId) -> Result<ExecuteClaim, ClaimError> {
        let mut state = self.lock();
        match state.lifecycle {
            ConnectionLifecycle::Open => {}
            ConnectionLifecycle::Closing => return Err(ClaimError::Closing),
            ConnectionLifecycle::Closed => return Err(ClaimError::Closed),
            ConnectionLifecycle::Broken => return Err(ClaimError::Broken),
        }

        let drain_previous = match state.active_operation.as_ref() {
            None => false,
            Some(active)
                if active.cursor_id == Some(cursor_id)
                    && active.phase == OperationPhase::Fetching =>
            {
                true
            }
            Some(_) => return Err(ClaimError::Busy),
        };

        let operation_id = self.allocate_operation_id();
        let cancel_handle = CancelHandle::new();
        state.active_operation = Some(ActiveOperation {
            cursor_id: Some(cursor_id),
            operation_id,
            phase: OperationPhase::Executing,
            cancel_handle: Some(cancel_handle),
            cancel_requested: false,
        });

        let child_handle = state
            .active_operation
            .as_ref()
            .and_then(|active| active.cancel_handle.as_ref())
            .expect("newly claimed operation has a cancel handle")
            .child_handle();
        Ok(ExecuteClaim {
            operation_id,
            cancel_handle: child_handle,
            drain_previous,
        })
    }

    pub(crate) fn claim_connection_operation(&self) -> Result<OperationId, ClaimError> {
        let mut state = self.lock();
        match state.lifecycle {
            ConnectionLifecycle::Open => {}
            ConnectionLifecycle::Closing => return Err(ClaimError::Closing),
            ConnectionLifecycle::Closed => return Err(ClaimError::Closed),
            ConnectionLifecycle::Broken => return Err(ClaimError::Broken),
        }
        if state.active_operation.is_some() {
            return Err(ClaimError::Busy);
        }

        let operation_id = self.allocate_operation_id();
        state.active_operation = Some(ActiveOperation {
            cursor_id: None,
            operation_id,
            phase: OperationPhase::Executing,
            cancel_handle: None,
            cancel_requested: false,
        });
        Ok(operation_id)
    }

    pub(crate) fn claim_fetch(&self, cursor_id: CursorId) -> Result<FetchClaim, ClaimError> {
        let mut state = self.lock();
        match state.lifecycle {
            ConnectionLifecycle::Open => {}
            ConnectionLifecycle::Closing => return Err(ClaimError::Closing),
            ConnectionLifecycle::Closed => return Err(ClaimError::Closed),
            ConnectionLifecycle::Broken => return Err(ClaimError::Broken),
        }

        let Some(active) = state.active_operation.as_mut() else {
            return Err(ClaimError::NoResultSet);
        };
        if active.cursor_id != Some(cursor_id) || active.phase != OperationPhase::Fetching {
            return Err(ClaimError::Busy);
        }
        active.phase = OperationPhase::FetchingRow;
        Ok(FetchClaim {
            operation_id: active.operation_id,
        })
    }

    pub(crate) fn claim_cursor_close(
        &self,
        cursor_id: CursorId,
    ) -> Result<CursorCloseClaim, ClaimError> {
        let mut state = self.lock();
        match state.lifecycle {
            ConnectionLifecycle::Open => {}
            ConnectionLifecycle::Closing => return Err(ClaimError::Closing),
            ConnectionLifecycle::Closed => return Err(ClaimError::Closed),
            ConnectionLifecycle::Broken => return Err(ClaimError::Broken),
        }

        let drain_previous = match state.active_operation.as_ref() {
            None => false,
            Some(active)
                if active.cursor_id == Some(cursor_id)
                    && active.phase == OperationPhase::Fetching =>
            {
                true
            }
            Some(_) => return Err(ClaimError::Busy),
        };
        let operation_id = self.allocate_operation_id();
        state.active_operation = Some(ActiveOperation {
            cursor_id: Some(cursor_id),
            operation_id,
            phase: OperationPhase::Closing,
            cancel_handle: None,
            cancel_requested: false,
        });
        Ok(CursorCloseClaim {
            operation_id,
            drain_previous,
        })
    }

    pub(crate) fn finish_execute(&self, operation_id: OperationId, has_open_batch: bool) {
        let mut state = self.lock();
        let Some(active) = state.active_operation.as_mut() else {
            return;
        };
        if active.operation_id != operation_id {
            return;
        }

        if has_open_batch {
            active.phase = OperationPhase::Fetching;
        } else {
            state.active_operation = None;
        }
    }

    pub(crate) fn finish_fetch(
        &self,
        operation_id: OperationId,
        result_set_exhausted: bool,
        has_open_batch: bool,
    ) -> bool {
        let mut state = self.lock();
        let Some(active) = state.active_operation.as_mut() else {
            return false;
        };
        if active.operation_id != operation_id || active.phase != OperationPhase::FetchingRow {
            return false;
        }
        if active.cancel_requested {
            return false;
        }

        if !result_set_exhausted || has_open_batch {
            active.phase = OperationPhase::Fetching;
        } else {
            state.active_operation = None;
        }
        true
    }

    pub(crate) fn restore_fetch(&self, operation_id: OperationId) {
        let mut state = self.lock();
        if let Some(active) = state.active_operation.as_mut()
            && active.operation_id == operation_id
            && active.phase == OperationPhase::FetchingRow
        {
            active.phase = OperationPhase::Fetching;
        }
    }

    pub(crate) fn cancel_fetch(&self, operation_id: OperationId) -> bool {
        let cancel_handle = {
            let mut state = self.lock();
            let Some(active) = state.active_operation.as_mut() else {
                return false;
            };
            if active.operation_id != operation_id || active.phase != OperationPhase::FetchingRow {
                return false;
            }
            active.cancel_requested = true;
            active.cancel_handle.take()
        };

        match cancel_handle {
            Some(cancel_handle) => {
                cancel_handle.cancel();
                true
            }
            None => false,
        }
    }

    pub(crate) fn release_operation(&self, operation_id: OperationId) {
        let mut state = self.lock();
        if state
            .active_operation
            .as_ref()
            .is_some_and(|active| active.operation_id == operation_id)
        {
            state.active_operation = None;
        }
    }

    pub(crate) fn abandon_cursor(&self, cursor_id: CursorId) {
        let mut state = self.lock();
        if state
            .active_operation
            .as_ref()
            .is_some_and(|active| active.cursor_id == Some(cursor_id))
        {
            state.active_operation = None;
        }
        state.lifecycle = ConnectionLifecycle::Broken;
    }

    pub(crate) fn begin_close(&self) {
        let mut state = self.lock();
        if state.lifecycle == ConnectionLifecycle::Open {
            state.lifecycle = ConnectionLifecycle::Closing;
        }
    }

    pub(crate) fn mark_closed(&self) {
        self.lock().lifecycle = ConnectionLifecycle::Closed;
    }

    pub(crate) fn mark_broken(&self) {
        self.lock().lifecycle = ConnectionLifecycle::Broken;
    }

    #[allow(dead_code)]
    pub(crate) fn lifecycle(&self) -> ConnectionLifecycle {
        self.lock().lifecycle
    }

    fn lock(&self) -> MutexGuard<'_, AsyncSessionState> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{AsyncConnectionState, ClaimError, ConnectionLifecycle, OperationPhase};

    #[test]
    fn allocates_unique_cursor_ids() {
        let state = AsyncConnectionState::new();

        assert_eq!(state.allocate_cursor_id(), 1);
        assert_eq!(state.allocate_cursor_id(), 2);
    }

    #[test]
    fn allocates_unique_operation_ids() {
        let state = AsyncConnectionState::new();

        assert_eq!(state.allocate_operation_id(), 1);
        assert_eq!(state.allocate_operation_id(), 2);
    }

    #[test]
    fn execute_ownership_tracks_results_and_allows_same_cursor_reexecute() {
        let state = AsyncConnectionState::new();

        let first = state.claim_execute(1).unwrap();
        assert!(!first.drain_previous);
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Busy);

        state.finish_execute(first.operation_id, true);
        assert_eq!(
            state.lock().active_operation.as_ref().unwrap().phase,
            OperationPhase::Fetching
        );

        let second = state.claim_execute(1).unwrap();
        assert!(second.drain_previous);
        state.finish_execute(second.operation_id, false);
        assert!(state.lock().active_operation.is_none());
    }

    #[test]
    fn stale_operation_cannot_release_current_owner() {
        let state = AsyncConnectionState::new();
        let first = state.claim_execute(1).unwrap();
        state.finish_execute(first.operation_id, true);
        let second = state.claim_execute(1).unwrap();

        state.release_operation(first.operation_id);
        assert_eq!(
            state.lock().active_operation.as_ref().unwrap().operation_id,
            second.operation_id
        );
    }

    #[test]
    fn connection_operation_rejects_cursor_ownership() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();

        assert_eq!(
            state.claim_connection_operation().unwrap_err(),
            ClaimError::Busy
        );
        state.release_operation(execute.operation_id);

        let connection_operation = state.claim_connection_operation().unwrap();
        assert_eq!(state.claim_execute(1).unwrap_err(), ClaimError::Busy);
        state.release_operation(connection_operation);
    }

    #[test]
    fn fetch_requires_owned_results_and_rejects_concurrent_work() {
        let state = AsyncConnectionState::new();
        assert_eq!(state.claim_fetch(1).unwrap_err(), ClaimError::NoResultSet);

        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();

        assert_eq!(state.claim_fetch(1).unwrap_err(), ClaimError::Busy);
        assert_eq!(state.claim_fetch(2).unwrap_err(), ClaimError::Busy);
        assert_eq!(state.claim_execute(1).unwrap_err(), ClaimError::Busy);

        state.finish_fetch(fetch.operation_id, false, true);
        assert_eq!(
            state.lock().active_operation.as_ref().unwrap().phase,
            OperationPhase::Fetching
        );
    }

    #[test]
    fn fetch_exhaustion_releases_only_a_finished_batch() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);

        let current_result_end = state.claim_fetch(1).unwrap();
        state.finish_fetch(current_result_end.operation_id, true, true);
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Busy);

        let batch_end = state.claim_fetch(1).unwrap();
        state.finish_fetch(batch_end.operation_id, true, false);
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn failed_awaitable_construction_restores_fetch_ownership() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();

        state.restore_fetch(fetch.operation_id);

        assert!(state.claim_fetch(1).is_ok());
    }

    #[test]
    fn fetch_cancellation_keeps_ownership_until_settlement() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();

        assert!(state.cancel_fetch(fetch.operation_id));
        assert!(!state.cancel_fetch(fetch.operation_id));
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Busy);

        assert!(!state.finish_fetch(fetch.operation_id, true, false));
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Busy);
        state.release_operation(fetch.operation_id);
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn cursor_close_can_drain_its_results_but_not_another_cursor() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);

        assert_eq!(state.claim_cursor_close(2).unwrap_err(), ClaimError::Busy);
        let close = state.claim_cursor_close(1).unwrap();
        assert!(close.drain_previous);
        assert_eq!(
            state.lock().active_operation.as_ref().unwrap().phase,
            OperationPhase::Closing
        );
        state.release_operation(close.operation_id);
    }

    #[test]
    fn abandoning_cursor_breaks_session_and_only_releases_its_ownership() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();

        state.abandon_cursor(2);
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
        assert_eq!(
            state.lock().active_operation.as_ref().unwrap().operation_id,
            execute.operation_id
        );

        state.abandon_cursor(1);
        assert!(state.lock().active_operation.is_none());
    }

    #[test]
    fn tracks_connection_lifecycle() {
        let state = AsyncConnectionState::new();

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Open);
        assert_eq!(state.ensure_open(), Ok(()));
        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closing);
        assert_eq!(state.ensure_open(), Err(ClaimError::Closing));
        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closing);
        state.mark_closed();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closed);
        assert_eq!(state.ensure_open(), Err(ClaimError::Closed));
        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closed);
        state.mark_broken();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
        assert_eq!(state.ensure_open(), Err(ClaimError::Broken));
        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
    }

    #[test]
    fn recovers_from_poisoned_state_mutex() {
        let state = Arc::new(AsyncConnectionState::new());
        let state_to_poison = Arc::clone(&state);

        assert!(
            std::thread::spawn(move || {
                let _guard = state_to_poison.inner.lock().unwrap();
                panic!("poison session state mutex");
            })
            .join()
            .is_err()
        );

        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closing);
    }
}
