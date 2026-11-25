// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use mssql_tds::core::CancelHandle;
use pyo3::prelude::*;
use tokio::sync::{oneshot, watch};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;

use crate::async_errors::{OperationalError, map_connection_claim, map_tds_error};
use crate::async_handle::{CloseMode, CloseResult, check_cleanup_deadline};

pub(crate) type CursorId = u64;
pub(crate) type FetchId = u64;
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
    pub(crate) fetch_id: FetchId,
}

#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FetchCompletion {
    Published,
    CancellationRequested,
    NoLongerActive,
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
    fetch_id: Option<FetchId>,
    pub(crate) phase: OperationPhase,
    pub(crate) cancel_handle: Option<CancelHandle>,
    /// Prevents a detached fetch from publishing results after its Python awaitable was cancelled.
    cancel_requested: bool,
}

/// Mutex-protected connection lifecycle and operation ownership state.
#[derive(Debug)]
struct AsyncSessionState {
    lifecycle: ConnectionLifecycle,
    broken: bool,
    active_operation: Option<ActiveOperation>,
    tasks: JoinSet<()>,
}

/// Connection-wide coordinator shared by the connection and all its cursors.
#[derive(Debug)]
pub(crate) struct AsyncConnectionState {
    next_cursor_id: AtomicU64,
    next_fetch_id: AtomicU64,
    next_operation_id: AtomicU64,
    discard: AtomicBool,
    inner: Mutex<AsyncSessionState>,
}

struct TaskCancellation {
    state: Arc<AsyncConnectionState>,
    abort: AbortHandle,
    preflight: Option<Arc<PreflightTask>>,
    completed: bool,
}

impl Drop for TaskCancellation {
    fn drop(&mut self) {
        if !self.completed {
            if let Some(preflight) = &self.preflight {
                preflight.cancel();
            } else {
                self.state.mark_broken();
            }
            self.abort.abort();
        }
    }
}

pub(crate) struct PreflightTask {
    state: Arc<AsyncConnectionState>,
    flags: AtomicU8,
}

impl PreflightTask {
    const CLAIMED: u8 = 1;
    const CANCELLED: u8 = 2;

    pub(crate) fn claim_execute(&self, cursor_id: CursorId) -> Result<ExecuteClaim, ClaimError> {
        let claim = self.state.claim_execute(cursor_id)?;
        // Either side of a claim/cancel race must observe the other before delivery.
        if self.flags.fetch_or(Self::CLAIMED, Ordering::AcqRel) & Self::CANCELLED != 0 {
            self.state.mark_broken();
        }
        Ok(claim)
    }

    fn cancel(&self) {
        if self.flags.fetch_or(Self::CANCELLED, Ordering::AcqRel) & Self::CLAIMED != 0 {
            self.state.mark_broken();
        }
    }
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
            next_fetch_id: AtomicU64::new(1),
            next_operation_id: AtomicU64::new(1),
            discard: AtomicBool::new(false),
            inner: Mutex::new(AsyncSessionState {
                lifecycle: ConnectionLifecycle::Open,
                broken: false,
                active_operation: None,
                tasks: JoinSet::new(),
            }),
        }
    }

    /// Registration and close admission share the same lock, before work can run.
    pub(crate) fn spawn_task<F, T>(
        self: &Arc<Self>,
        future: F,
    ) -> Result<(oneshot::Receiver<T>, AbortHandle), ClaimError>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let mut state = self.lock();
        match state.lifecycle {
            ConnectionLifecycle::Open => {}
            ConnectionLifecycle::Closing => return Err(ClaimError::Closing),
            ConnectionLifecycle::Closed => return Err(ClaimError::Closed),
            ConnectionLifecycle::Broken => return Err(ClaimError::Broken),
        }
        let mut task_errors = Vec::new();
        while let Some(result) = state.tasks.try_join_next() {
            if let Err(error) = result {
                self.discard.store(true, Ordering::Release);
                task_errors.push(error);
            }
        }
        let (sender, receiver) = oneshot::channel();
        let task_state = self.clone();
        let abort = state.tasks.spawn_on(
            async move {
                let result = future.await;
                if sender.send(result).is_err() {
                    task_state.mark_for_discard();
                    tracing::debug!(
                        "Async operation result was not delivered; connection will be discarded"
                    );
                }
            },
            pyo3_async_runtimes::tokio::get_runtime().handle(),
        );
        // Python logger callbacks can re-enter this connection.
        drop(state);
        for error in task_errors {
            tracing::warn!(%error, "Async connection task did not complete");
        }
        Ok((receiver, abort))
    }

    pub(crate) async fn run_tracked<F, T>(self: Arc<Self>, future: F) -> PyResult<T>
    where
        F: Future<Output = PyResult<T>> + Send + 'static,
        T: Send + 'static,
    {
        self.run_tracked_inner(future, None).await
    }

    /// The preflight exemption ends at this task's claim, not native completion.
    pub(crate) async fn run_tracked_preflight<G, F, T>(self: Arc<Self>, future: G) -> PyResult<T>
    where
        G: FnOnce(Arc<PreflightTask>) -> F + Send,
        F: Future<Output = PyResult<T>> + Send + 'static,
        T: Send + 'static,
    {
        let preflight = Arc::new(PreflightTask {
            state: self.clone(),
            flags: AtomicU8::new(0),
        });
        self.run_tracked_inner(future(preflight.clone()), Some(preflight))
            .await
    }

    async fn run_tracked_inner<F, T>(
        self: Arc<Self>,
        future: F,
        preflight: Option<Arc<PreflightTask>>,
    ) -> PyResult<T>
    where
        F: Future<Output = PyResult<T>> + Send + 'static,
        T: Send + 'static,
    {
        let (receiver, abort) = self.spawn_task(future).map_err(|error| {
            map_connection_claim(error, "Connection is busy with another operation")
        })?;
        let mut cancellation = TaskCancellation {
            state: self,
            abort,
            preflight,
            completed: false,
        };
        let result = receiver.await.map_err(|_| {
            cancellation.state.mark_broken();
            OperationalError::new_err("Connection operation was interrupted")
        });
        cancellation.completed = true;
        result?
    }

    pub(crate) fn must_discard(&self) -> bool {
        self.discard.load(Ordering::Acquire)
    }

    pub(crate) fn is_broken(&self) -> bool {
        self.lock().broken
    }

    pub(crate) fn is_broken_before_deadline(
        &self,
        deadline: Instant,
    ) -> mssql_tds::core::TdsResult<bool> {
        let state = self.lock();
        check_cleanup_deadline(deadline)?;
        Ok(state.broken)
    }

    pub(crate) fn mark_for_discard(&self) {
        self.discard.store(true, Ordering::Release);
    }

    pub(crate) fn take_close_tasks(&self) -> JoinSet<()> {
        self.begin_close();
        std::mem::take(&mut self.lock().tasks)
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

    /// Reject a queued claim if close or another operation won before physical access.
    /// Call while holding the logical handle mutex, without holding this state lock across I/O.
    pub(crate) fn validate_operation(&self, operation_id: OperationId) -> Result<(), ClaimError> {
        let state = self.lock();
        match state.lifecycle {
            ConnectionLifecycle::Open => {}
            ConnectionLifecycle::Closing => return Err(ClaimError::Closing),
            ConnectionLifecycle::Closed => return Err(ClaimError::Closed),
            ConnectionLifecycle::Broken => return Err(ClaimError::Broken),
        }
        if state
            .active_operation
            .as_ref()
            .is_some_and(|active| active.operation_id == operation_id)
        {
            Ok(())
        } else {
            Err(ClaimError::Busy)
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
            fetch_id: None,
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

    pub(crate) fn claim_connection_operation(
        &self,
    ) -> Result<(OperationId, Option<ActiveOperation>), ClaimError> {
        let mut state = self.lock();
        match state.lifecycle {
            ConnectionLifecycle::Open => {}
            ConnectionLifecycle::Closing => return Err(ClaimError::Closing),
            ConnectionLifecycle::Closed => return Err(ClaimError::Closed),
            ConnectionLifecycle::Broken => return Err(ClaimError::Broken),
        }
        if state
            .active_operation
            .as_ref()
            .is_some_and(|active| active.phase != OperationPhase::Fetching)
        {
            return Err(ClaimError::Busy);
        }

        let operation_id = self.allocate_operation_id();
        let previous = state.active_operation.replace(ActiveOperation {
            cursor_id: None,
            operation_id,
            fetch_id: None,
            phase: OperationPhase::Executing,
            cancel_handle: None,
            cancel_requested: false,
        });
        Ok((operation_id, previous))
    }

    pub(crate) fn restore_connection_operation(
        &self,
        operation_id: OperationId,
        previous: Option<ActiveOperation>,
    ) {
        let mut state = self.lock();
        if state.lifecycle == ConnectionLifecycle::Open
            && state
                .active_operation
                .as_ref()
                .is_some_and(|active| active.operation_id == operation_id)
        {
            state.active_operation = previous;
        }
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
        let fetch_id = self.next_fetch_id.fetch_add(1, Ordering::Relaxed);
        active.phase = OperationPhase::FetchingRow;
        active.fetch_id = Some(fetch_id);
        Ok(FetchClaim {
            operation_id: active.operation_id,
            fetch_id,
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
            fetch_id: None,
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

    /// Publishes a completed fetch only while it still owns an uncancelled read.
    ///
    /// Cancellation keeps ownership with the detached worker so it can settle
    /// ATTENTION. A missing or mismatched operation has already been released and
    /// needs no further protocol cleanup.
    pub(crate) fn finish_fetch(
        &self,
        operation_id: OperationId,
        fetch_id: FetchId,
        result_set_exhausted: bool,
        has_open_batch: bool,
    ) -> FetchCompletion {
        let mut state = self.lock();
        let Some(active) = state.active_operation.as_mut() else {
            return FetchCompletion::NoLongerActive;
        };
        if active.operation_id != operation_id
            || active.fetch_id != Some(fetch_id)
            || active.phase != OperationPhase::FetchingRow
        {
            return FetchCompletion::NoLongerActive;
        }
        if active.cancel_requested {
            return FetchCompletion::CancellationRequested;
        }

        active.fetch_id = None;
        if !result_set_exhausted || has_open_batch {
            active.phase = OperationPhase::Fetching;
        } else {
            state.active_operation = None;
        }
        FetchCompletion::Published
    }

    pub(crate) fn restore_fetch(&self, operation_id: OperationId, fetch_id: FetchId) {
        let mut state = self.lock();
        if let Some(active) = state.active_operation.as_mut()
            && active.operation_id == operation_id
            && active.fetch_id == Some(fetch_id)
            && active.phase == OperationPhase::FetchingRow
        {
            active.fetch_id = None;
            active.phase = OperationPhase::Fetching;
        }
    }

    /// Requests cancellation for the matching in-flight fetch task exactly once.
    ///
    /// Session ownership intentionally remains in `FetchingRow`; releasing it
    /// here would let another command race with the background ATTENTION drain.
    pub(crate) fn cancel_fetch(&self, operation_id: OperationId, fetch_id: FetchId) -> bool {
        let cancel_handle = {
            let mut state = self.lock();
            let Some(active) = state.active_operation.as_mut() else {
                return false;
            };
            if active.operation_id != operation_id
                || active.fetch_id != Some(fetch_id)
                || active.phase != OperationPhase::FetchingRow
            {
                return false;
            }
            let Some(cancel_handle) = active.cancel_handle.take() else {
                return false;
            };
            active.cancel_requested = true;
            self.mark_for_discard();
            cancel_handle
        };

        cancel_handle.cancel();
        true
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

    pub(crate) fn complete_connection_operation(&self, operation_id: OperationId) -> PyResult<()> {
        let mut state = self.lock();
        let owns_operation = state
            .active_operation
            .as_ref()
            .is_some_and(|active| active.operation_id == operation_id);
        if owns_operation {
            state.active_operation = None;
        }
        if state.broken {
            return Err(OperationalError::new_err(
                "Transaction completion was interrupted; outcome may be unknown; connection discarded",
            ));
        }
        Ok(())
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
        self.mark_for_discard();
        state.broken = true;
        if state.lifecycle == ConnectionLifecycle::Open {
            state.lifecycle = ConnectionLifecycle::Broken;
        }
    }

    pub(crate) fn begin_close(&self) {
        let mut state = self.lock();
        if matches!(
            state.lifecycle,
            ConnectionLifecycle::Open | ConnectionLifecycle::Broken
        ) {
            state.lifecycle = ConnectionLifecycle::Closing;
        }
        if let Some(active) = state.active_operation.as_mut()
            && !matches!(
                active.phase,
                OperationPhase::Fetching | OperationPhase::Closing
            )
        {
            self.mark_for_discard();
            if active.phase == OperationPhase::FetchingRow {
                active.cancel_requested = true;
            }
            if let Some(cancel) = &active.cancel_handle {
                cancel.cancel();
            }
            state.broken = true;
        }
    }

    pub(crate) fn mark_closed(&self) {
        let mut state = self.lock();
        state.lifecycle = ConnectionLifecycle::Closed;
        state.active_operation = None;
    }

    pub(crate) fn complete_close<T>(
        &self,
        mode: CloseMode,
        deadline: Instant,
        result: PyResult<T>,
        finish: impl FnOnce(T, bool) -> PyResult<()>,
        sender: &watch::Sender<Option<CloseResult>>,
    ) {
        let mut state = self.lock();
        let result = result.and_then(|value| {
            check_cleanup_deadline(deadline)
                .map_err(|error| map_tds_error("Connection cleanup failed", error, Vec::new()))?;
            if matches!(mode, CloseMode::Commit) && state.broken {
                return Err(OperationalError::new_err(
                    "Connection was interrupted during commit cleanup; outcome may be unknown; connection discarded",
                ));
            }
            finish(value, self.must_discard())
        });
        if result.is_err() {
            self.mark_for_discard();
            state.broken = true;
        }
        state.lifecycle = ConnectionLifecycle::Closed;
        state.active_operation = None;
        // Final return and publication share the cancellation lock.
        sender.send_replace(Some(result.map_err(Arc::new)));
    }

    pub(crate) fn mark_broken(&self) {
        let mut state = self.lock();
        self.mark_for_discard();
        state.broken = true;
        if state.lifecycle == ConnectionLifecycle::Open {
            state.lifecycle = ConnectionLifecycle::Broken;
        }
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
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

    use tracing::{Event, Subscriber};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    use super::{
        AsyncConnectionState, ClaimError, ConnectionLifecycle, FetchCompletion, OperationPhase,
        PreflightTask, SessionOperationGuard,
    };

    #[tokio::test(start_paused = true)]
    #[allow(clippy::await_holding_lock)] // Expire the mock deadline while authorization is blocked.
    async fn close_authorization_rechecks_deadline_after_waiting_for_state_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        use tokio::time::Instant;

        let state = Arc::new(AsyncConnectionState::new());
        state.begin_close();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(!state.is_broken_before_deadline(deadline).unwrap());
        let held = state.lock();
        let (started, waiting) = mpsc::channel();
        let (completed, outcome) = mpsc::channel();
        let worker_state = state.clone();
        let runtime = tokio::runtime::Handle::current();
        let worker = std::thread::spawn(move || {
            let _runtime = runtime.enter();
            started.send(()).unwrap();
            completed
                .send(worker_state.is_broken_before_deadline(deadline))
                .unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(outcome.try_recv(), Err(mpsc::TryRecvError::Empty)));
        tokio::time::advance(Duration::from_secs(1)).await;
        drop(held);
        let result = outcome.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        let error = result.expect_err("expired cleanup was authorized after the state lock wait");
        assert!(matches!(error, mssql_tds::error::Error::TimeoutError(_)));
        assert!(error.to_string().contains("Connection cleanup timed out"));
    }

    #[test]
    fn allocates_unique_cursor_ids() {
        let state = AsyncConnectionState::new();

        assert_eq!(state.allocate_cursor_id(), 1);
        assert_eq!(state.allocate_cursor_id(), 2);
    }

    #[tokio::test]
    async fn cancelled_task_diagnostics_allow_session_reentry() {
        struct CheckUnlocked {
            state: Arc<AsyncConnectionState>,
            observed: Arc<AtomicBool>,
        }

        impl<S: Subscriber> Layer<S> for CheckUnlocked {
            fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
                if *event.metadata().level() == tracing::Level::WARN {
                    assert!(self.state.inner.try_lock().is_ok());
                    self.observed.store(true, Ordering::Release);
                }
            }
        }

        let state = Arc::new(AsyncConnectionState::new());
        let (receiver, abort) = state.spawn_task(std::future::pending::<()>()).unwrap();
        abort.abort();
        assert!(receiver.await.is_err());
        while !abort.is_finished() {
            tokio::task::yield_now().await;
        }
        let observed = Arc::new(AtomicBool::new(false));
        let subscriber = tracing_subscriber::Registry::default().with(CheckUnlocked {
            state: state.clone(),
            observed: observed.clone(),
        });
        let (receiver, _) =
            tracing::subscriber::with_default(subscriber, || state.spawn_task(async {})).unwrap();
        receiver.await.unwrap();
        assert!(observed.load(Ordering::Acquire));
        assert!(state.must_discard());
        assert_eq!(state.ensure_open(), Ok(()));
    }

    #[tokio::test]
    async fn preflight_cancellation_only_breaks_after_its_own_claim() {
        for (claim_before_cancel, unrelated_operation) in
            [(false, false), (false, true), (true, false)]
        {
            let state = Arc::new(AsyncConnectionState::new());
            let previous = unrelated_operation.then(|| state.claim_execute(2).unwrap());
            let (started, ready) = tokio::sync::oneshot::channel();
            let task_state = state.clone();
            let task = tokio::spawn(state.clone().run_tracked_preflight(
                move |preflight| async move {
                    let guard = claim_before_cancel.then(|| {
                        let claim = preflight.claim_execute(1).unwrap();
                        SessionOperationGuard::new(task_state, claim.operation_id)
                    });
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                    drop(guard);
                    Ok(())
                },
            ));
            ready.await.unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());

            let mut tasks = std::mem::take(&mut state.lock().tasks);
            while let Some(result) = tasks.join_next().await {
                assert!(result.unwrap_err().is_cancelled());
            }
            assert_eq!(state.is_broken(), claim_before_cancel);
            assert_eq!(state.must_discard(), claim_before_cancel);
            if let Some(previous) = previous {
                assert_eq!(state.validate_operation(previous.operation_id), Ok(()));
            } else if !claim_before_cancel {
                assert!(state.claim_execute(1).is_ok());
            }
        }
    }

    #[tokio::test]
    async fn preflight_waiter_cancellation_after_execution_prevents_commit() {
        for (has_open_batch, delivered) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let state = Arc::new(AsyncConnectionState::new());
            let (release, wait) = tokio::sync::oneshot::channel();
            let task_state = state.clone();
            let mut waiter = Box::pin(state.clone().run_tracked_preflight(
                move |preflight| async move {
                    wait.await.unwrap();
                    let claim = preflight.claim_execute(1).unwrap();
                    let mut guard = SessionOperationGuard::new(task_state, claim.operation_id);
                    guard.finish_execute(has_open_batch);
                    Ok(())
                },
            ));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(waiter.as_mut().poll(&mut context).is_pending());
            release.send(()).unwrap();
            let mut tasks = std::mem::take(&mut state.lock().tasks);
            assert!(tasks.join_next().await.unwrap().is_ok());
            assert!(!state.is_broken());

            if delivered {
                waiter.await.unwrap();
            } else {
                drop(waiter);
            }

            assert_eq!(state.is_broken(), !delivered);
            assert_eq!(state.must_discard(), !delivered);
        }
    }

    #[test]
    fn preflight_claim_cancel_race_only_breaks_successfully_claimed_tasks() {
        for (cancel_first, busy) in [(false, false), (true, false), (false, true), (true, true)] {
            let state = Arc::new(AsyncConnectionState::new());
            let previous = busy.then(|| state.claim_execute(2).unwrap());
            let preflight = PreflightTask {
                state: state.clone(),
                flags: AtomicU8::new(0),
            };
            if cancel_first {
                preflight.cancel();
            }
            assert_eq!(preflight.claim_execute(1).is_ok(), !busy);
            if !cancel_first {
                preflight.cancel();
            }
            assert_eq!(state.is_broken(), !busy);
            assert_eq!(state.must_discard(), !busy);
            if let Some(previous) = previous {
                assert_eq!(state.validate_operation(previous.operation_id), Ok(()));
            }
        }
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

        let (connection_operation, _) = state.claim_connection_operation().unwrap();
        assert_eq!(state.claim_execute(1).unwrap_err(), ClaimError::Busy);
        state.release_operation(connection_operation);
    }

    #[test]
    fn failed_connection_construction_restores_only_its_displaced_owner() {
        for interrupted in [false, true] {
            let state = AsyncConnectionState::new();
            let execute = state.claim_execute(1).unwrap();
            state.finish_execute(execute.operation_id, true);
            let (operation, previous) = state.claim_connection_operation().unwrap();
            let replacement = if interrupted {
                state.release_operation(operation);
                Some(state.claim_execute(2).unwrap())
            } else {
                None
            };
            state.restore_connection_operation(operation, previous);

            if let Some(replacement) = replacement {
                assert!(state.validate_operation(replacement.operation_id).is_ok());
                assert_eq!(state.claim_fetch(1).unwrap_err(), ClaimError::Busy);
            } else {
                let fetch = state.claim_fetch(1).unwrap();
                assert_eq!(fetch.operation_id, execute.operation_id);
            }
        }

        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let (operation, previous) = state.claim_connection_operation().unwrap();
        state.begin_close();
        state.restore_connection_operation(operation, previous);
        assert_eq!(state.claim_fetch(1).unwrap_err(), ClaimError::Closing);
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

        assert_eq!(
            state.finish_fetch(fetch.operation_id, fetch.fetch_id, false, true),
            FetchCompletion::Published
        );
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
        assert_eq!(
            state.finish_fetch(
                current_result_end.operation_id,
                current_result_end.fetch_id,
                true,
                true,
            ),
            FetchCompletion::Published
        );
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Busy);

        let batch_end = state.claim_fetch(1).unwrap();
        assert_eq!(
            state.finish_fetch(batch_end.operation_id, batch_end.fetch_id, true, false,),
            FetchCompletion::Published
        );
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn failed_awaitable_construction_restores_fetch_ownership() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();

        state.restore_fetch(fetch.operation_id, fetch.fetch_id);

        assert!(state.claim_fetch(1).is_ok());
    }

    #[test]
    fn stale_fetch_cancellation_cannot_cancel_the_next_fetch() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let first = state.claim_fetch(1).unwrap();
        assert_eq!(
            state.finish_fetch(first.operation_id, first.fetch_id, false, true),
            FetchCompletion::Published
        );
        let second = state.claim_fetch(1).unwrap();

        assert_eq!(first.operation_id, second.operation_id);
        assert_ne!(first.fetch_id, second.fetch_id);
        assert!(!state.cancel_fetch(first.operation_id, first.fetch_id));
        assert!(state.cancel_fetch(second.operation_id, second.fetch_id));
    }

    #[test]
    fn fetch_without_a_cancel_handle_is_not_marked_cancelled() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();
        state
            .lock()
            .active_operation
            .as_mut()
            .unwrap()
            .cancel_handle = None;

        assert!(!state.cancel_fetch(fetch.operation_id, fetch.fetch_id));
        assert_eq!(
            state.finish_fetch(fetch.operation_id, fetch.fetch_id, false, true),
            FetchCompletion::Published
        );
    }

    /// Verifies that cancellation does not release the shared session before
    /// the detached protocol task reports settlement.
    #[test]
    fn fetch_cancellation_keeps_ownership_until_settlement() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();

        assert!(state.cancel_fetch(fetch.operation_id, fetch.fetch_id));
        assert!(!state.cancel_fetch(fetch.operation_id, fetch.fetch_id));
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Busy);

        assert_eq!(
            state.finish_fetch(fetch.operation_id, fetch.fetch_id, true, false),
            FetchCompletion::CancellationRequested
        );
        assert_eq!(state.claim_execute(2).unwrap_err(), ClaimError::Busy);
        state.release_operation(fetch.operation_id);
        assert!(state.claim_execute(2).is_ok());
    }

    #[test]
    fn finished_fetch_distinguishes_an_already_released_operation() {
        let state = AsyncConnectionState::new();
        let execute = state.claim_execute(1).unwrap();
        state.finish_execute(execute.operation_id, true);
        let fetch = state.claim_fetch(1).unwrap();
        state.abandon_cursor(1);

        assert_eq!(
            state.finish_fetch(fetch.operation_id, fetch.fetch_id, true, false),
            FetchCompletion::NoLongerActive
        );
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
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closed);
        assert_eq!(state.ensure_open(), Err(ClaimError::Closed));
        assert!(state.must_discard());
        state.begin_close();
        assert_eq!(state.lifecycle(), ConnectionLifecycle::Closed);
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
