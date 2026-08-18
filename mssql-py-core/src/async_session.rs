// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use mssql_tds::core::CancelHandle;

pub(crate) type CursorId = u64;
pub(crate) type OperationId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionLifecycle {
    Open,
    Closing,
    Closed,
    Broken,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationPhase {
    Executing,
    Fetching,
    Closing,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ActiveOperation {
    pub(crate) cursor_id: CursorId,
    pub(crate) operation_id: OperationId,
    pub(crate) phase: OperationPhase,
    pub(crate) cancel_handle: Option<CancelHandle>,
}

#[derive(Debug)]
struct AsyncSessionState {
    lifecycle: ConnectionLifecycle,
    #[allow(dead_code)]
    active_operation: Option<ActiveOperation>,
}

#[derive(Debug)]
pub(crate) struct AsyncConnectionState {
    next_cursor_id: AtomicU64,
    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub(crate) fn allocate_operation_id(&self) -> OperationId {
        self.next_operation_id.fetch_add(1, Ordering::Relaxed)
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

    use super::{AsyncConnectionState, ConnectionLifecycle};

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
