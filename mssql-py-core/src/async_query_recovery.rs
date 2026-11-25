// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cancellation recovery and settlement for native async query awaitables.

use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use mssql_tds::connection::tds_client::TdsClient;
use pyo3::IntoPyObjectExt;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::async_cursor::map_claim_error;
use crate::async_description::DescriptionState;
use crate::async_errors::map_tds_error;
use crate::async_fetch::{BufferedResults, FetchState, FetchStatus};
use crate::async_handle::LogicalConnection;
use crate::async_session::{AsyncConnectionState, FetchId, OperationId};

pub(crate) const SETTLEMENT_TIMEOUT_SECONDS: u32 = 5;
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(SETTLEMENT_TIMEOUT_SECONDS as u64);

#[derive(Clone, Copy)]
enum Target {
    Execute(OperationId),
    Fetch(OperationId, FetchId),
}

impl Target {
    fn ids(self) -> (OperationId, Option<FetchId>) {
        match self {
            Self::Execute(operation_id) => (operation_id, None),
            Self::Fetch(operation_id, fetch_id) => (operation_id, Some(fetch_id)),
        }
    }
}

struct State {
    target: Option<Target>,
    completed: bool,
    delivered: bool,
    cursor: Option<Py<PyAny>>,
}

struct DeliveryRecovery {
    client: Arc<LogicalConnection>,
    fetch_state: Arc<FetchState>,
    description_state: Arc<DescriptionState>,
    buffered_results: Arc<BufferedResults>,
}

pub(crate) struct QueryControl {
    session: Arc<AsyncConnectionState>,
    state: Mutex<State>,
    cancelled: CancellationToken,
    settled: watch::Sender<bool>,
    deadline: OnceLock<tokio::time::Instant>,
    operation: &'static str,
    delivery_finished: CancellationToken,
    recovery: Option<DeliveryRecovery>,
}

impl QueryControl {
    pub(crate) fn new(session: Arc<AsyncConnectionState>, operation: &'static str) -> Arc<Self> {
        Self::create(session, operation, None, None)
    }

    pub(crate) fn for_delivery(
        session: Arc<AsyncConnectionState>,
        operation: &'static str,
        client: Arc<LogicalConnection>,
        fetch_state: Arc<FetchState>,
        description_state: Arc<DescriptionState>,
        buffered_results: Arc<BufferedResults>,
        cursor: Py<PyAny>,
    ) -> Arc<Self> {
        Self::create(
            session,
            operation,
            Some(DeliveryRecovery {
                client,
                fetch_state,
                description_state,
                buffered_results,
            }),
            Some(cursor),
        )
    }

    fn create(
        session: Arc<AsyncConnectionState>,
        operation: &'static str,
        recovery: Option<DeliveryRecovery>,
        cursor: Option<Py<PyAny>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session,
            state: Mutex::new(State {
                target: None,
                completed: false,
                delivered: false,
                cursor,
            }),
            cancelled: CancellationToken::new(),
            settled: watch::channel(false).0,
            deadline: OnceLock::new(),
            operation,
            delivery_finished: CancellationToken::new(),
            recovery,
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
        if self.recovery.is_some() {
            let (operation_id, fetch_id) = target.ids();
            self.session.defer_query_delivery(operation_id, fetch_id);
        }
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
        if state.delivered {
            return;
        }
        if !state.completed {
            self.deadline
                .get_or_init(|| tokio::time::Instant::now() + SETTLEMENT_TIMEOUT);
            if let Some(target) = state.target {
                self.cancel_target(target);
            }
            self.cancelled.cancel();
        }
        self.delivery_finished.cancel();
    }

    pub(crate) fn complete(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.complete_locked(&mut state);
    }

    pub(crate) fn complete_if_released(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(target) = state.target {
            let (operation_id, fetch_id) = target.ids();
            if !self.session.owns_query(operation_id, fetch_id) {
                self.complete_locked(&mut state);
            }
        }
    }

    fn complete_locked(&self, state: &mut State) {
        state.completed = true;
        state.target = None;
        state.cursor = None;
        self.settled.send_replace(true);
    }

    fn retire_if_unsettled(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.retire_unsettled_locked(&state)
    }

    fn retire_unsettled_locked(&self, state: &State) -> bool {
        !state.completed
            && !state.delivered
            && state.target.is_some_and(|target| {
                let (operation_id, fetch_id) = target.ids();
                self.session.mark_broken_if_owned(operation_id, fetch_id)
            })
    }

    async fn recover_undelivered(&self) -> PyResult<()> {
        let target = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .target;
        let Some((target, recovery)) = target.zip(self.recovery.as_ref()) else {
            return Ok(());
        };
        let (operation_id, fetch_id) = target.ids();
        if !self.session.query_delivery_pending(operation_id, fetch_id) {
            return Ok(());
        }
        let result = async {
            let mut client = recovery
                .client
                .lock_operation(operation_id)
                .await
                .map_err(map_claim_error)?;
            self.settle_open_batch(&mut client).await
        }
        .await;
        if result.is_err() {
            self.retire_if_unsettled();
        }
        recovery.fetch_state.set(FetchStatus::NoResultSet);
        recovery.description_state.replace(None);
        recovery.buffered_results.replace(Default::default());
        self.session.release_operation(operation_id);
        self.complete_if_released();
        result
    }

    #[cfg(test)]
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
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if state.completed || state.delivered {
                    return self.session.ensure_open().map_err(map_claim_error);
                }
                let retired = self.retire_unsettled_locked(&state);
                drop(state);
                let message = if retired {
                    "Query cancellation settlement timed out; connection is broken"
                } else {
                    "Query cancellation settlement timed out without owning the connection"
                };
                tracing::error!(operation = self.operation, retired, "{message}");
                return Err(PyRuntimeError::new_err(message));
            }
        }
        self.session.ensure_open().map_err(map_claim_error)
    }
}

#[pyclass]
struct QuerySettlement {
    control: Arc<QueryControl>,
}

#[pyclass]
struct QueryDelivery {
    control: Arc<QueryControl>,
}

#[pymethods]
impl QueryDelivery {
    fn cancelled(&self, future: &Bound<'_, PyAny>) -> PyResult<()> {
        if future.call_method0("cancelled")?.is_truthy()? {
            self.control.cancel();
        }
        Ok(())
    }

    fn deliver(
        &self,
        future: &Bound<'_, PyAny>,
        complete: &Bound<'_, PyAny>,
        value: &Bound<'_, PyAny>,
        failed: bool,
    ) -> PyResult<()> {
        let mut state = self
            .control
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if future.call_method0("done")?.is_truthy()? {
            drop(state);
            self.cancelled(future)?;
            return Ok(());
        }
        if self.control.cancelled.is_cancelled() {
            drop(state);
            if failed {
                complete.call1((value,))?;
            } else {
                future.call_method1(
                    "set_exception",
                    (map_tds_error(
                        self.control.operation,
                        mssql_tds::error::Error::OperationCancelledError(
                            "Async query was cancelled before result delivery".to_owned(),
                        ),
                        Vec::new(),
                    ),),
                )?;
            }
            return Ok(());
        }
        if !failed && let Err(error) = self.control.session.ensure_open() {
            future.call_method1("set_exception", (map_claim_error(error),))?;
            state.delivered = true;
            self.control.delivery_finished.cancel();
            return Ok(());
        }
        // The event-loop callback arbitrates delivery and cancellation under the GIL.
        if let Err(error) = complete.call1((value,)) {
            drop(state);
            self.control.retire_if_unsettled();
            self.control.cancel();
            return Err(error);
        }
        if let Some(target) = state.target {
            let (operation_id, fetch_id) = target.ids();
            self.control
                .session
                .publish_query_delivery(operation_id, fetch_id);
        }
        state.delivered = true;
        self.control.delivery_finished.cancel();
        Ok(())
    }
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
        if !self.completed {
            self.control.retire_if_unsettled();
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
        if result.is_err() {
            completion.control.retire_if_unsettled();
        }
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

async fn schedule_delivery<T>(
    event_loop: Py<PyAny>,
    future: Py<PyAny>,
    delivery: Py<QueryDelivery>,
    result: PyResult<T>,
) -> PyResult<()>
where
    T: for<'a> IntoPyObject<'a> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        Python::attach(move |py| {
            let future = future.bind(py);
            if future.call_method0("cancelled")?.is_truthy()? {
                delivery.borrow(py).control.cancel();
                return Ok(());
            }
            let (complete, value, failed) = match result.and_then(|value| value.into_py_any(py)) {
                Ok(value) => (future.getattr("set_result")?, value, false),
                Err(error) => (
                    future.getattr("set_exception")?,
                    error.into_py_any(py)?,
                    true,
                ),
            };
            event_loop.bind(py).call_method1(
                "call_soon_threadsafe",
                (
                    delivery.bind(py).getattr("deliver")?,
                    future,
                    complete,
                    value,
                    failed,
                ),
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|error| PyRuntimeError::new_err(format!("Query result delivery failed: {error}")))?
}

/// Registered workers retain recovery ownership until Python accepts their result.
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
    let locals = pyo3_async_runtimes::tokio::get_current_locals(py)?;
    let event_loop = locals.event_loop(py).unbind();
    let awaitable = event_loop.bind(py).call_method0("create_future")?;
    let delivery = Py::new(
        py,
        QueryDelivery {
            control: Arc::clone(&control),
        },
    )?;
    awaitable.call_method1(
        "add_done_callback",
        (delivery.bind(py).getattr("cancelled")?,),
    )?;
    if let Err(error) = awaitable.setattr("_cancel_and_settle", cancel_and_settle) {
        control.cancel();
        control.retire_if_unsettled();
        tracing::error!(%error, "Failed to attach async query settlement handle");
        return Err(error);
    }
    let failure_loop = event_loop.clone_ref(py);
    let failure_future = awaitable.clone().unbind();
    let failure_delivery = delivery.clone_ref(py);
    let python_future = awaitable.clone().unbind();
    let worker_control = Arc::clone(&control);
    let worker = async move {
        let result = future.await;
        schedule_delivery(event_loop, python_future, delivery, result).await?;
        worker_control.delivery_finished.cancelled().await;
        if worker_control.cancelled.is_cancelled() {
            worker_control.recover_undelivered().await?;
        }
        Ok(())
    };
    pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
        let result = run_query_in_background(
            pyo3_async_runtimes::tokio::scope(locals, worker),
            Arc::clone(&control),
        )
        .await;
        if let Err(error) = result {
            tracing::error!(%error, "Async query delivery or recovery failed");
            if let Err(error) =
                schedule_delivery::<()>(failure_loop, failure_future, failure_delivery, Err(error))
                    .await
            {
                tracing::error!(%error, "Failed to report async query delivery error");
            }
        } else if let Err(error) = tokio::task::spawn_blocking(move || {
            Python::attach(move |_| drop((failure_loop, failure_future, failure_delivery)));
        })
        .await
        {
            control.retire_if_unsettled();
            tracing::error!(%error, "Failed to release async query delivery references");
        }
    });
    Ok(awaitable)
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll, Waker};

    use tokio::sync::oneshot;

    use super::*;
    use crate::async_session::{ClaimError, SessionOperationGuard};

    #[test]
    fn interrupted_worker_cannot_deliver_a_queued_success() {
        let session = Arc::new(AsyncConnectionState::new());
        let control = QueryControl::new(Arc::clone(&session), "execute");
        session.mark_broken();
        Python::attach(|py| {
            let event_loop = py
                .import("asyncio")
                .unwrap()
                .call_method0("new_event_loop")
                .unwrap();
            let future = event_loop.call_method0("create_future").unwrap();
            QueryDelivery { control }
                .deliver(
                    &future,
                    &future.getattr("set_result").unwrap(),
                    &py.None().into_bound(py),
                    false,
                )
                .unwrap();
            assert!(
                future
                    .call_method0("result")
                    .unwrap_err()
                    .to_string()
                    .contains("broken")
            );
            event_loop.call_method0("close").unwrap();
        });
    }

    #[tokio::test]
    async fn closed_loop_delivery_failure_retires_an_undelivered_batch() {
        let session = Arc::new(AsyncConnectionState::new());
        let claim = session.claim_execute(1).unwrap();
        let control = QueryControl::new(Arc::clone(&session), "execute");
        control.register_execute(claim.operation_id);
        session.defer_query_delivery(claim.operation_id, None);
        assert!(session.finish_execute(claim.operation_id, true));
        let (event_loop, future, delivery) = Python::attach(|py| {
            let event_loop = py
                .import("asyncio")
                .unwrap()
                .call_method0("new_event_loop")
                .unwrap();
            let future = event_loop.call_method0("create_future").unwrap().unbind();
            let delivery = Py::new(
                py,
                QueryDelivery {
                    control: Arc::clone(&control),
                },
            )
            .unwrap();
            event_loop.call_method0("close").unwrap();
            (event_loop.unbind(), future, delivery)
        });

        let error = run_query_in_background(
            schedule_delivery(event_loop, future, delivery, Ok(())),
            control,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("Event loop is closed"));
        assert_eq!(session.ensure_open(), Err(ClaimError::Broken));
        assert!(session.must_discard());
    }

    #[tokio::test]
    async fn worker_failure_only_retires_unsettled_matching_ownership() {
        for phase in [
            "unsettled",
            "released",
            "settled",
            "delivered",
            "fetch-replaced",
        ] {
            let session = Arc::new(AsyncConnectionState::new());
            let original = session.claim_execute(1).unwrap();
            let control = QueryControl::new(Arc::clone(&session), "execute");
            control.register_execute(original.operation_id);
            let current_id = match phase {
                "released" | "settled" => {
                    session.release_operation(original.operation_id);
                    if phase == "settled" {
                        control.complete();
                    }
                    session.claim_execute(2).unwrap().operation_id
                }
                "delivered" => {
                    assert!(session.finish_execute(original.operation_id, true));
                    control.state.lock().unwrap().delivered = true;
                    original.operation_id
                }
                "fetch-replaced" => {
                    assert!(session.finish_execute(original.operation_id, true));
                    let fetch = session.claim_fetch(1).unwrap();
                    control.register_fetch(fetch.operation_id, fetch.fetch_id);
                    assert_eq!(
                        session.finish_fetch(fetch.operation_id, fetch.fetch_id, false, true),
                        crate::async_session::FetchCompletion::Published
                    );
                    session.claim_fetch(1).unwrap().operation_id
                }
                _ => original.operation_id,
            };
            let error = run_query_in_background(
                async { Err::<(), _>(PyRuntimeError::new_err("Result delivery failed")) },
                Arc::clone(&control),
            )
            .await
            .unwrap_err();

            assert!(error.to_string().contains("Result delivery failed"));
            assert_eq!(session.is_broken(), phase == "unsettled", "{phase}");
            if phase != "unsettled" {
                assert_eq!(session.validate_operation(current_id), Ok(()), "{phase}");
                assert!(!session.must_discard(), "{phase}");
            }
        }
    }

    #[test]
    fn interrupted_released_worker_cannot_retire_replacement() {
        let session = Arc::new(AsyncConnectionState::new());
        let original = session.claim_execute(1).unwrap();
        let control = QueryControl::new(Arc::clone(&session), "execute");
        control.register_execute(original.operation_id);
        session.release_operation(original.operation_id);
        let replacement = session.claim_execute(2).unwrap();

        drop(CompletionGuard {
            control,
            completed: false,
        });

        assert!(!session.is_broken());
        assert!(!session.must_discard());
        assert_eq!(session.validate_operation(replacement.operation_id), Ok(()));
    }

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
    async fn stale_settlement_timeouts_cannot_retire_replacement_generations() {
        for fetching in [false, true] {
            let session = Arc::new(AsyncConnectionState::new());
            let original = session.claim_execute(1).unwrap();
            let control = QueryControl::new(Arc::clone(&session), "execute");
            let replacement_id = if fetching {
                assert!(session.finish_execute(original.operation_id, true));
                let fetch = session.claim_fetch(1).unwrap();
                control.register_fetch(fetch.operation_id, fetch.fetch_id);
                assert_eq!(
                    session.finish_fetch(fetch.operation_id, fetch.fetch_id, false, true),
                    crate::async_session::FetchCompletion::Published
                );
                session.claim_fetch(1).unwrap().operation_id
            } else {
                control.register_execute(original.operation_id);
                session.release_operation(original.operation_id);
                session.claim_execute(2).unwrap().operation_id
            };
            control.cancel();

            let error = control.wait_for_settlement().await.unwrap_err();

            assert!(error.to_string().contains("timed out"));
            assert!(!session.is_broken());
            assert!(!session.must_discard());
            assert_eq!(session.validate_operation(replacement_id), Ok(()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_delivery_does_not_require_worker_completion_before_settlement() {
        let session = Arc::new(AsyncConnectionState::new());
        let claim = session.claim_execute(1).unwrap();
        let control = QueryControl::new(Arc::clone(&session), "execute");
        control.register_execute(claim.operation_id);
        assert!(session.finish_execute(claim.operation_id, true));
        control.state.lock().unwrap().delivered = true;
        control.cancel();

        control.wait_for_settlement().await.unwrap();

        assert!(!session.is_broken());
        assert!(session.claim_fetch(1).is_ok());
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
