// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use mssql_tds::core::CancelHandle;

pub(crate) type CursorId = u64;
pub(crate) type OperationId = u64;

#[derive(Debug)]
pub(crate) struct ExecuteClaim {
    pub(crate) operation_id: OperationId,
    pub(crate) cancel_handle: CancelHandle,
    pub(crate) drain_previous: bool,
}

#[derive(Debug)]
pub(crate) struct CursorCloseClaim {
    pub(crate) operation_id: OperationId,
    pub(crate) drain_previous: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClaimError {
    Closing,
    Closed,
    Broken,
    Busy,
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
    #[allow(dead_code)]
    Closing,
}

#[derive(Debug)]
pub(crate) struct ActiveOperation {
    pub(crate) cursor_id: Option<CursorId>,
    pub(crate) operation_id: OperationId,
    pub(crate) phase: OperationPhase,
    pub(crate) cancel_handle: Option<CancelHandle>,
}

#[derive(Debug)]
struct AsyncSessionState {
    lifecycle: ConnectionLifecycle,
    active_operation: Option<ActiveOperation>,
}

#[derive(Debug)]
pub(crate) struct AsyncConnectionState {
    next_cursor_id: AtomicU64,
    next_operation_id: AtomicU64,
    inner: Mutex<AsyncSessionState>,
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
        });
        Ok(operation_id)
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
    fn tracks_connection_lifecycle() {
        let state = AsyncConnectionState::new();

        assert_eq!(state.lifecycle(), ConnectionLifecycle::Open);
        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closing);
        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closing);
        state.mark_closed();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closed);
        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closed);
        state.mark_broken();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Broken);
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
