// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cancellation recovery and settlement for native async query awaitables.

use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use mssql_tds::connection::tds_client::TdsClient;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::async_cursor::map_claim_error;
use crate::async_errors::map_tds_error;
use crate::async_session::{AsyncConnectionState, FetchId, OperationId};

pub(crate) const SETTLEMENT_TIMEOUT_SECONDS: u32 = 5;
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(SETTLEMENT_TIMEOUT_SECONDS as u64);

#[derive(Clone, Copy)]
enum Target {
    Execute(OperationId),
    Fetch(OperationId, FetchId),
}

struct State {
    target: Option<Target>,
    completed: bool,
}

pub(crate) struct QueryControl {
    session: Arc<AsyncConnectionState>,
    state: Mutex<State>,
    cancelled: CancellationToken,
    settled: watch::Sender<bool>,
    deadline: OnceLock<tokio::time::Instant>,
    operation: &'static str,
}

impl QueryControl {
    pub(crate) fn new(session: Arc<AsyncConnectionState>, operation: &'static str) -> Arc<Self> {
        Arc::new(Self {
            session,
            state: Mutex::new(State {
                target: None,
                completed: false,
            }),
            cancelled: CancellationToken::new(),
            settled: watch::channel(false).0,
            deadline: OnceLock::new(),
            operation,
        })
    }

    pub(crate) fn register_execute(&self, operation_id: OperationId) {
        self.register(Target::Execute(operation_id));
    }

    pub(crate) fn register_fetch(&self, operation_id: OperationId, fetch_id: FetchId) {
        self.register(Target::Fetch(operation_id, fetch_id));
    }

    fn register(&self, target: Target) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.target = Some(target);
        if self.cancelled.is_cancelled() {
            self.cancel_target(target);
        }
    }

    pub(crate) async fn cancelled(&self) {
        self.cancelled.cancelled().await;
    }

    fn cancel_target(&self, target: Target) {
        match target {
            Target::Execute(operation_id) => {
                self.session.cancel_execute(operation_id);
            }
            Target::Fetch(operation_id, fetch_id) => {
                if self.session.cancel_fetch(operation_id, fetch_id) {
                    #[cfg(debug_assertions)]
                    crate::async_fetch::release_fetch_publication_pause();
                }
            }
        }
    }

    pub(crate) fn cancel(&self) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.completed {
            self.deadline
                .get_or_init(|| tokio::time::Instant::now() + SETTLEMENT_TIMEOUT);
            if let Some(target) = state.target {
                self.cancel_target(target);
            }
            self.cancelled.cancel();
        }
    }

    pub(crate) fn complete(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.completed = true;
        state.target = None;
        self.settled.send_replace(true);
    }

    fn is_settled(&self) -> bool {
        *self.settled.borrow()
    }

    pub(crate) fn remaining_settlement_timeout(&self) -> Duration {
        self.deadline.get().map_or(SETTLEMENT_TIMEOUT, |deadline| {
            deadline.saturating_duration_since(tokio::time::Instant::now())
        })
    }

    pub(crate) async fn settle_open_batch(&self, client: &mut TdsClient) -> PyResult<()> {
        if client.has_open_batch() {
            match client
                .send_attention_with_timeout(self.remaining_settlement_timeout())
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::error!(
                        operation = self.operation,
                        "Query cancellation acknowledgement timed out"
                    );
                    return Err(PyRuntimeError::new_err(
                        "Query cancellation acknowledgement timed out; connection is broken",
                    ));
                }
                Err(error) => {
                    tracing::error!(operation = self.operation, %error, "Query cancellation recovery failed");
                    return Err(map_tds_error(self.operation, error, Vec::new()));
                }
            }
        }
        if client.has_open_batch() || client.is_connection_dead() {
            tracing::error!(
                operation = self.operation,
                "Query cancellation did not recover the connection"
            );
            return Err(map_claim_error(crate::async_session::ClaimError::Broken));
        }
        Ok(())
    }

    async fn wait_for_settlement(&self) -> PyResult<()> {
        let mut settled = self.settled.subscribe();
        let wait = async {
            while !*settled.borrow_and_update() {
                settled.changed().await.map_err(|error| {
                    PyRuntimeError::new_err(format!("Query settlement channel failed: {error}"))
                })?;
            }
            PyResult::Ok(())
        };
        let deadline = self
            .deadline
            .get()
            .copied()
            .unwrap_or_else(|| tokio::time::Instant::now() + SETTLEMENT_TIMEOUT);
        match tokio::time::timeout_at(deadline, wait).await {
            Ok(result) => result?,
            Err(_) => {
                self.session.mark_broken();
                tracing::error!(
                    operation = self.operation,
                    "Async query cancellation settlement timed out; connection marked broken"
                );
                return Err(PyRuntimeError::new_err(
                    "Query cancellation settlement timed out; connection is broken",
                ));
            }
        }
        self.session.ensure_open().map_err(map_claim_error)
    }
}

#[pyclass]
struct QuerySettlement {
    control: Arc<QueryControl>,
}

#[pymethods]
impl QuerySettlement {
    fn cancel_and_settle<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.control.cancel();
        let control = Arc::clone(&self.control);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            control.wait_for_settlement().await
        })
    }
}

struct WaitGuard {
    control: Arc<QueryControl>,
    completed: bool,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.control.cancel();
        }
    }
}

struct CompletionGuard {
    control: Arc<QueryControl>,
    completed: bool,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.completed && !self.control.is_settled() {
            self.control.session.mark_broken();
        }
        self.control.complete();
    }
}

pub(crate) async fn run_query_in_background<F, T>(
    future: F,
    control: Arc<QueryControl>,
) -> PyResult<T>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: Send + 'static,
{
    let mut waiting = WaitGuard {
        control: Arc::clone(&control),
        completed: false,
    };
    let session = Arc::clone(&control.session);
    let worker_control = Arc::clone(&control);
    let worker = async move {
        let mut completion = CompletionGuard {
            control: worker_control,
            completed: false,
        };
        let result = future.await;
        completion.completed = true;
        result
    };
    let receiver = match session.spawn_task(worker) {
        Ok((receiver, _)) => receiver,
        Err(error) => {
            waiting.completed = true;
            control.complete();
            return Err(map_claim_error(error));
        }
    };
    let result = receiver.await;
    waiting.completed = true;
    result
        .map_err(|error| PyRuntimeError::new_err(format!("Async query worker failed: {error}")))?
}

/// The worker owns protocol futures; cancellation of its Python join only signals it.
pub(crate) fn query_future_into_py<'py, F, T>(
    py: Python<'py>,
    future: F,
    control: Arc<QueryControl>,
) -> PyResult<Bound<'py, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'a> IntoPyObject<'a> + Send + 'static,
{
    let settlement = Py::new(
        py,
        QuerySettlement {
            control: Arc::clone(&control),
        },
    )?;
    let cancel_and_settle = settlement.bind(py).getattr("cancel_and_settle")?;
    let future = run_query_in_background(future, Arc::clone(&control));
    let awaitable = pyo3_async_runtimes::tokio::future_into_py(py, future)?;
    if let Err(error) = awaitable.setattr("_cancel_and_settle", cancel_and_settle) {
        control.cancel();
        control.session.mark_broken();
        tracing::error!(%error, "Failed to attach async query settlement handle");
        return Err(error);
    }
    Ok(awaitable)
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll, Waker};

    use tokio::sync::oneshot;

    use super::*;
    use crate::async_session::{ClaimError, SessionOperationGuard};

    #[tokio::test]
    async fn dropping_the_python_join_keeps_the_worker_and_ownership_until_settled() {
        let session = Arc::new(AsyncConnectionState::new());
        let claim = session.claim_execute(1).unwrap();
        let control = QueryControl::new(Arc::clone(&session), "execute");
        control.register_execute(claim.operation_id);
        let worker_session = Arc::clone(&session);
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let future = async move {
            let mut guard = SessionOperationGuard::new(worker_session, claim.operation_id);
            entered_tx.send(()).unwrap();
            release_rx.await.unwrap();
            assert!(!guard.finish_execute(false));
            guard.settle(false);
            Ok(())
        };
        let mut waiting = Box::pin(run_query_in_background(future, Arc::clone(&control)));
        assert!(matches!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        entered_rx.await.unwrap();
        drop(waiting);

        assert_eq!(session.claim_execute(2).unwrap_err(), ClaimError::Busy);
        assert!(session.ensure_open().is_ok());
        release_tx.send(()).unwrap();
        control.wait_for_settlement().await.unwrap();
        assert!(session.claim_execute(2).is_ok());
    }

    #[tokio::test]
    async fn close_can_abort_and_join_a_query_worker() {
        let session = Arc::new(AsyncConnectionState::new());
        let claim = session.claim_execute(1).unwrap();
        let control = QueryControl::new(Arc::clone(&session), "execute");
        control.register_execute(claim.operation_id);
        let (entered_tx, entered_rx) = oneshot::channel();
        let future = async move {
            entered_tx.send(()).unwrap();
            std::future::pending::<PyResult<()>>().await
        };
        let mut waiting = Box::pin(run_query_in_background(future, Arc::clone(&control)));
        assert!(matches!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        entered_rx.await.unwrap();

        let mut tasks = session.take_close_tasks();
        assert_eq!(tasks.len(), 1);
        tasks.abort_all();
        assert!(tasks.join_next().await.unwrap().unwrap_err().is_cancelled());
        assert!(
            waiting
                .await
                .unwrap_err()
                .to_string()
                .contains("worker failed")
        );
        assert!(control.is_settled());
        assert!(session.is_broken());
    }

    #[test]
    fn cancellation_before_preflight_finishes_reaches_the_registered_operation() {
        let session = Arc::new(AsyncConnectionState::new());
        let control = QueryControl::new(Arc::clone(&session), "executemany");
        control.cancel();
        let claim = session.claim_execute(1).unwrap();
        control.register_execute(claim.operation_id);

        assert!(!session.finish_execute(claim.operation_id, false));
        assert_eq!(session.claim_execute(2).unwrap_err(), ClaimError::Busy);
        session.release_operation(claim.operation_id);
    }

    #[test]
    fn a_stale_handle_cannot_cancel_a_replacement_operation() {
        let session = Arc::new(AsyncConnectionState::new());
        let first = session.claim_execute(1).unwrap();
        let control = QueryControl::new(Arc::clone(&session), "execute");
        control.register_execute(first.operation_id);
        session.release_operation(first.operation_id);
        let second = session.claim_execute(1).unwrap();

        control.cancel();

        assert!(session.finish_execute(second.operation_id, false));
    }

    #[tokio::test(start_paused = true)]
    async fn settlement_timeout_is_bounded_and_retires_the_session() {
        let session = Arc::new(AsyncConnectionState::new());
        let claim = session.claim_execute(1).unwrap();
        let control = QueryControl::new(Arc::clone(&session), "execute");
        control.register_execute(claim.operation_id);
        let started = tokio::time::Instant::now();
        control.cancel();
        tokio::time::advance(Duration::from_secs(4)).await;

        let error = control.wait_for_settlement().await.unwrap_err();

        assert!(error.to_string().contains("settlement timed out"));
        assert_eq!(started.elapsed(), SETTLEMENT_TIMEOUT);
        assert_eq!(session.ensure_open(), Err(ClaimError::Broken));
    }

    #[tokio::test]
    async fn failed_protocol_recovery_is_not_reported_as_successful_settlement() {
        let session = Arc::new(AsyncConnectionState::new());
        let claim = session.claim_execute(1).unwrap();
        let control = QueryControl::new(Arc::clone(&session), "execute");
        control.register_execute(claim.operation_id);
        let worker_session = Arc::clone(&session);
        run_query_in_background(
            async move {
                let mut guard = SessionOperationGuard::new(worker_session, claim.operation_id);
                guard.settle(true);
                Ok(())
            },
            Arc::clone(&control),
        )
        .await
        .unwrap();

        assert!(control.wait_for_settlement().await.is_err());
        assert_eq!(session.ensure_open(), Err(ClaimError::Broken));
    }
}
