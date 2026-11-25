// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Internal Python adapter; scheduling and physical ownership stay in the Rust pool.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use pyo3::IntoPyObjectExt;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyCFunction, PyDict};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::async_connection::PyAsyncConnection;
use crate::async_errors::{InterfaceError, InternalError, OperationalError};
use crate::async_handle::{ConnHandle, LogicalConnection};
use crate::pool::config::PoolConfig;
use crate::pool::connection::{ConnectionFactory, SqlConnectionFactory};
use crate::pool::control::AcquireControl;
use crate::pool::error::AcquireStage;
use crate::pool::error::{OpenError, PoolError};
use crate::pool::manager::AsyncPoolManager;
use crate::pool::session::Isolation;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;

pub(crate) fn isolation(value: Option<&str>) -> PyResult<Isolation> {
    let level = match value.unwrap_or("READ COMMITTED") {
        "READ UNCOMMITTED" => TransactionIsolationLevel::ReadUncommitted,
        "READ COMMITTED" => TransactionIsolationLevel::ReadCommitted,
        "REPEATABLE READ" => TransactionIsolationLevel::RepeatableRead,
        "SERIALIZABLE" => TransactionIsolationLevel::Serializable,
        "SNAPSHOT" => TransactionIsolationLevel::Snapshot,
        _ => return Err(InterfaceError::new_err("Invalid isolation_level")),
    };
    level.try_into().map_err(map_pool_error)
}

struct DeliveryState {
    connection: Option<Arc<LogicalConnection>>,
    attempt: Option<Arc<ConnectAttempt>>,
}

pub(crate) struct ConnectDelivery {
    pub(crate) cancel: CancellationToken,
    deadline: Option<Instant>,
    inner: Mutex<DeliveryState>,
}

struct PublicationGuard {
    delivery: Arc<ConnectDelivery>,
    armed: bool,
}

impl PublicationGuard {
    fn finish(&mut self, failed: bool) -> PyResult<()> {
        self.delivery.finish(failed)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for PublicationGuard {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self.delivery.finish(true)
        {
            tracing::error!(%error, "Could not discard an unpublished connection");
        }
    }
}

#[pyclass]
struct ConnectionPublication {
    guard: PublicationGuard,
    native: Py<PyAny>,
    target: Py<PyAny>,
    result: Option<PyResult<Py<PyAny>>>,
}

impl ConnectionPublication {
    fn publish(&mut self, py: Python<'_>) -> PyResult<()> {
        // Stop bridge bookkeeping before public completion can close its loop.
        let native = self.native.bind(py).call0()?;
        if !native.is_none() {
            native.call_method0("cancel")?;
        }
        let target = self.target.bind(py);
        let result = self
            .result
            .take()
            .ok_or_else(|| InternalError::new_err("Connection was already published"))?;
        if target.call_method0("cancelled")?.is_truthy()? {
            return self.guard.finish(true);
        }
        if self.guard.delivery.cancel.is_cancelled() {
            let closed = self.guard.delivery.pool_closed()?;
            self.guard.finish(true)?;
            if closed {
                let error = map_pool_error(PoolError::Closed);
                target.call_method1("set_exception", (error.value(py),))?;
            } else {
                target.call_method0("cancel")?;
            }
            return Ok(());
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.guard.finish(true)?;
                target.call_method1("set_exception", (error.value(py),))?;
                return Ok(());
            }
        };
        if self
            .guard
            .delivery
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.guard.finish(true)?;
            let error = map_pool_error(PoolError::AcquireTimeout {
                stage: AcquireStage::Delivery,
            });
            target.call_method1("set_exception", (error.value(py),))?;
            return Ok(());
        }
        target.call_method1("set_result", (result,))?;
        self.guard.finish(false)
    }
}

#[pymethods]
impl ConnectionPublication {
    fn __call__(&mut self, py: Python<'_>) -> PyResult<()> {
        match self.publish(py) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Err(cleanup) = self.guard.finish(true) {
                    tracing::error!(%cleanup, "Could not discard a failed connection publication");
                }
                let target = self.target.bind(py);
                if target.call_method0("done")?.is_truthy()? {
                    return Err(error);
                }
                target.call_method1("set_exception", (error.value(py),))?;
                Ok(())
            }
        }
    }
}

impl ConnectDelivery {
    pub(crate) fn new(attempt: Option<ConnectAttempt>, deadline: Option<Instant>) -> Arc<Self> {
        Arc::new(Self {
            cancel: attempt
                .as_ref()
                .map(|value| value.cancel.clone())
                .unwrap_or_default(),
            deadline,
            inner: Mutex::new(DeliveryState {
                connection: None,
                attempt: attempt.map(Arc::new),
            }),
        })
    }

    pub(crate) fn store(&self, connection: Arc<LogicalConnection>) -> PyResult<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| InternalError::new_err("Connection delivery tracking was poisoned"))?;
        if self.cancel.is_cancelled() {
            drop(inner);
            connection.discard_undelivered();
            return Err(map_pool_error(PoolError::Cancelled));
        }
        inner.connection = Some(connection);
        Ok(())
    }

    fn finish(&self, failed: bool) -> PyResult<()> {
        if failed {
            self.cancel.cancel();
        }
        let (connection, attempt) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| InternalError::new_err("Connection delivery tracking was poisoned"))?;
            (inner.connection.take(), inner.attempt.take())
        };
        if failed && let Some(connection) = &connection {
            connection.discard_undelivered();
        }
        drop(connection);
        drop(attempt);
        Ok(())
    }

    fn native_attempt(&self) -> PyResult<Option<Arc<ConnectAttempt>>> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| InternalError::new_err("Connection delivery tracking was poisoned"))?
            .attempt
            .clone())
    }

    fn pool_closed(&self) -> PyResult<bool> {
        match self.native_attempt()? {
            Some(attempt) => Ok(attempt.pool.lock()?.closed),
            None => Ok(false),
        }
    }

    // Box before this adapter to keep native open futures out of its stack frame.
    pub(crate) fn bind<'py, F, T>(
        self: &Arc<Self>,
        py: Python<'py>,
        future: Pin<Box<F>>,
    ) -> PyResult<Bound<'py, PyAny>>
    where
        F: Future<Output = PyResult<T>> + Send + 'static,
        T: for<'a> IntoPyObject<'a> + Send + 'static,
    {
        let guard = PublicationGuard {
            delivery: self.clone(),
            armed: true,
        };
        let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
        let result = event_loop.call_method0("create_future")?;
        let target = result.clone().unbind();
        let event_loop = event_loop.unbind();
        let (native_sender, native_receiver) = tokio::sync::oneshot::channel::<Py<PyAny>>();
        let future = pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let native = native_receiver.await.map_err(|_| {
                InternalError::new_err("Connection delivery initialization was abandoned")
            })?;
            let result = future.await;
            Python::attach(|py| {
                let publication = Py::new(
                    py,
                    ConnectionPublication {
                        guard,
                        native,
                        target,
                        result: Some(result.and_then(|result| result.into_py_any(py))),
                    },
                )?;
                if let Err(error) = event_loop
                    .bind(py)
                    .call_method1("call_soon_threadsafe", (publication.bind(py),))
                {
                    publication.borrow_mut(py).guard.finish(true)?;
                    return Err(error);
                }
                Ok(())
            })
        })?;
        // Avoid a public-future -> native-future -> public-future reference cycle.
        let native = py
            .import("weakref")?
            .getattr("ref")?
            .call1((&future,))?
            .unbind();
        let publication_native = native.clone_ref(py);
        let delivery = self.clone();
        let cancellation = PyCFunction::new_closure(py, None, None, move |args, _| {
            if args.get_item(0)?.call_method0("cancelled")?.is_truthy()? {
                delivery.finish(true)?;
                let native = native.bind(args.py()).call0()?;
                if !native.is_none() {
                    native.call_method0("cancel")?;
                }
            }
            Ok::<(), PyErr>(())
        })?;
        result.call_method1("add_done_callback", (cancellation,))?;
        let delivery = self.clone();
        let target = result.clone().unbind();
        let callback = PyCFunction::new_closure(py, None, None, move |args, _| {
            let future = args.get_item(0)?;
            let target = target.bind(args.py());
            if future.call_method0("cancelled")?.is_truthy()? {
                if !target.call_method0("done")?.is_truthy()? {
                    target.call_method0("cancel")?;
                }
                return Ok(());
            }
            // Observe bridge errors, including native task panics.
            let error = future.call_method0("exception")?;
            if !error.is_none() && !target.call_method0("done")?.is_truthy()? {
                delivery.finish(true)?;
                target.call_method1("set_exception", (error,))?;
            }
            Ok::<(), PyErr>(())
        })?;
        if let Err(error) = future.call_method1("add_done_callback", (callback,)) {
            self.finish(true)?;
            future.call_method0("cancel")?;
            return Err(error);
        }
        native_sender
            .send(publication_native)
            .map_err(|_| InternalError::new_err("Native connection delivery was abandoned"))?;
        Ok(result)
    }
}

impl Drop for ConnectDelivery {
    fn drop(&mut self) {
        self.cancel.cancel();
        match self.inner.get_mut() {
            Ok(inner) => {
                if let Some(connection) = inner.connection.take() {
                    connection.discard_undelivered();
                }
            }
            Err(_) => {
                tracing::error!("Connection delivery tracking was poisoned during destruction")
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn connect<'py>(
    py: Python<'py>,
    pool: &PyAsyncPool,
    options: &Bound<'_, PyDict>,
    timeout: Option<&Bound<'_, PyAny>>,
    level: Option<&str>,
    token_provider: bool,
    autocommit: bool,
    dispatch: Option<tracing::Dispatch>,
    started: Instant,
) -> PyResult<Bound<'py, PyAny>> {
    pool.check_owner(py)?;
    let attempt = pool.inner.register()?;
    let budget = timeout
        .map(|value| duration(value, "acquire_timeout", false))
        .transpose()?
        .unwrap_or(pool.inner.config.acquire_timeout());
    let control = AcquireControl::from_timeout(started, budget, attempt.cancel.clone())
        .map_err(map_pool_error)?;
    let requested = isolation(level)?;
    if token_provider {
        return Err(InterfaceError::new_err(
            "Pooled connections support only SQL password authentication",
        ));
    }
    let options = options.copy()?;
    if let Some(value) = options.get_item("connect_retry_count")? {
        if value.is_instance_of::<PyBool>() || value.extract::<u32>().ok() != Some(0) {
            return Err(InterfaceError::new_err(
                "Pooled connections require connect_retry_count=0",
            ));
        }
    } else {
        options.set_item("connect_retry_count", 0)?;
    }
    // Preserve the async default instead of inheriting the key seam's ODBC default.
    let packet_size = match options.get_item("packet_size")? {
        None => 4096,
        Some(value) => {
            if value.is_instance_of::<PyBool>() {
                return Err(InterfaceError::new_err("Invalid packet_size"));
            }
            match value.extract::<u32>() {
                Ok(0) => 4096,
                Ok(value @ 512..=32768) => value,
                _ => return Err(InterfaceError::new_err("Invalid packet_size")),
            }
        }
    };
    options.set_item("packet_size", packet_size)?;
    let prepared = crate::connection_key::prepare_sql_connection(&options).map_err(|error| {
        if error.is_instance_of::<pyo3::exceptions::PyValueError>(py) {
            InterfaceError::new_err("Invalid or unsupported pooled SQL-password connection options")
        } else {
            InternalError::new_err("Failed to prepare pooled SQL-password connection options")
        }
    })?;
    control
        .check(AcquireStage::Admission)
        .map_err(map_pool_error)?;
    let (key, spec) = prepared.into_parts();
    let pool = pool.inner.clone();
    let delivery = ConnectDelivery::new(Some(attempt), Some(control.deadline()));
    let native_attempt = delivery.native_attempt()?;
    let future_dispatch = dispatch.clone();
    let future_delivery = delivery.clone();
    let future = async move {
        let _attempt = native_attempt;
        let deadline = control.deadline();
        let cancel = control.cancellation().clone();
        let handle = match pool
            .manager
            .acquire(key, &spec, &SqlConnectionFactory, requested, control)
            .await
            .map_err(map_pool_error)?
        {
            Some(lease) => ConnHandle::Pooled(Box::new(lease)),
            None => {
                // This is the sole bypass: the manager was disabled at admission.
                let control = AcquireControl::new(deadline, cancel.clone());
                let opened = control
                    .run(AcquireStage::Open, async {
                        let cancel = mssql_tds::core::CancelHandle::new();
                        SqlConnectionFactory
                            .open(&spec, &cancel)
                            .await
                            .map_err(PoolError::from)
                    })
                    .await
                    .map_err(map_pool_error)?;
                let (mut client, _) = opened.into_parts();
                control
                    .run(AcquireStage::Isolation, async {
                        requested.apply(&mut client).await
                    })
                    .await
                    .map_err(map_pool_error)?;
                ConnHandle::Direct(Box::new(client))
            }
        };
        let connection = Arc::new(LogicalConnection::from_handle(
            handle,
            pool.config.cleanup_timeout(),
        ));
        future_delivery.store(connection.clone())?;
        let result = Python::attach(|py| {
            Py::new(
                py,
                PyAsyncConnection::from_logical(connection, dispatch, autocommit),
            )
        })?;
        AcquireControl::new(deadline, cancel)
            .check(AcquireStage::Delivery)
            .map_err(map_pool_error)?;
        Ok(result)
    };
    delivery.bind(
        py,
        Box::pin(crate::async_connection::with_optional_dispatch(
            future,
            future_dispatch,
        )),
    )
}

pub(crate) fn map_pool_error(error: PoolError) -> PyErr {
    let message = error.to_string();
    let reason = match &error {
        PoolError::WaitQueueFull => "WaitQueueFull",
        PoolError::Closed => "Closed",
        PoolError::AcquireTimeout { .. } => "AcquireTimeout",
        PoolError::CleanupTimeout => "CleanupTimeout",
        PoolError::ShutdownTimeout => "ShutdownTimeout",
        _ => "PoolError",
    };
    let mapped = match error {
        PoolError::InvalidConfig { .. } | PoolError::InvalidRequest { .. } | PoolError::Closed => {
            InterfaceError::new_err(message)
        }
        PoolError::Cancelled => Python::attach(|py| {
            py.import("asyncio")?
                .getattr("CancelledError")?
                .call0()
                .map(PyErr::from_value)
        })
        .unwrap_or_else(|error| error),
        PoolError::Open(OpenError::Tds(error))
        | PoolError::Initialize(error)
        | PoolError::Cleanup(error) => {
            crate::async_errors::map_tds_error("Async pool operation failed", *error, Vec::new())
        }
        PoolError::Internal { .. } | PoolError::InvalidSession => InternalError::new_err(message),
        _ => OperationalError::new_err(message),
    };
    Python::attach(|py| mapped.value(py).setattr("pool_reason", reason))
        .unwrap_or_else(|error| tracing::warn!(%error, "Could not attach pool error reason"));
    mapped
}

pub(crate) fn duration(
    value: &Bound<'_, PyAny>,
    field: &'static str,
    allow_zero: bool,
) -> PyResult<Duration> {
    let invalid = || {
        InterfaceError::new_err(format!(
            "{field} must be a finite {} number of seconds",
            if allow_zero {
                "nonnegative"
            } else {
                "positive"
            }
        ))
    };
    if value.is_instance_of::<PyBool>() {
        return Err(invalid());
    }
    let value = value.extract::<f64>().map_err(|_| invalid())?;
    let value = Duration::try_from_secs_f64(value).map_err(|_| invalid())?;
    if (!allow_zero && value.is_zero()) || Instant::now().checked_add(value).is_none() {
        return Err(invalid());
    }
    Ok(value)
}

fn limit(value: &Bound<'_, PyAny>, field: &str) -> PyResult<usize> {
    if !value.is_instance_of::<PyBool>()
        && let Ok(value) = value.extract::<usize>()
        && value > 0
    {
        return Ok(value);
    }

    Err(InterfaceError::new_err(format!(
        "{field} must be a positive integer"
    )))
}

fn max_size(value: &Bound<'_, PyAny>) -> PyResult<usize> {
    limit(value, "max_size")
}

fn idle_timeout(value: &Bound<'_, PyAny>) -> PyResult<Duration> {
    duration(value, "idle_timeout", true)
}

struct Attempts {
    closed: bool,
    next_id: u64,
    pending: HashMap<u64, CancellationToken>,
}

pub(crate) struct PoolAdapter {
    pub(crate) manager: AsyncPoolManager,
    pub(crate) config: PoolConfig,
    attempts: Mutex<Attempts>,
    settled: Notify,
}

impl PoolAdapter {
    fn lock(&self) -> PyResult<MutexGuard<'_, Attempts>> {
        self.attempts
            .lock()
            .map_err(|_| InternalError::new_err("Pool attempt tracking was poisoned"))
    }

    pub(crate) fn register(self: &Arc<Self>) -> PyResult<ConnectAttempt> {
        let mut attempts = self.lock()?;
        if attempts.closed {
            drop(attempts);
            return Err(map_pool_error(PoolError::Closed));
        }
        let id = attempts.next_id;
        attempts.next_id = id
            .checked_add(1)
            .ok_or_else(|| InternalError::new_err("Pool attempt identifier overflow"))?;
        let cancel = CancellationToken::new();
        attempts.pending.insert(id, cancel.clone());
        Ok(ConnectAttempt {
            pool: self.clone(),
            id,
            cancel,
        })
    }

    fn stop(&self) -> PyResult<()> {
        {
            let mut attempts = self.lock()?;
            attempts.closed = true;
            for cancel in attempts.pending.values() {
                cancel.cancel();
            }
        }
        self.manager.disable();
        Ok(())
    }

    async fn shutdown(&self, deadline: Instant) -> PyResult<()> {
        let shutdown = async {
            self.manager.shutdown().await.map_err(map_pool_error)?;
            loop {
                let settled = self.settled.notified();
                tokio::pin!(settled);
                settled.as_mut().enable();
                let complete = self.lock()?.pending.is_empty();
                if Instant::now() >= deadline {
                    return Err(map_pool_error(PoolError::ShutdownTimeout));
                }
                if complete {
                    return Ok(());
                }
                settled.await;
            }
        };
        tokio::time::timeout_at(deadline, shutdown)
            .await
            .map_err(|_| map_pool_error(PoolError::ShutdownTimeout))?
    }
}

pub(crate) struct ConnectAttempt {
    pool: Arc<PoolAdapter>,
    id: u64,
    pub(crate) cancel: CancellationToken,
}

impl Drop for ConnectAttempt {
    fn drop(&mut self) {
        match self.pool.attempts.lock() {
            Ok(mut attempts) => {
                attempts.pending.remove(&self.id);
            }
            Err(_) => tracing::error!("Pool attempt tracking was poisoned during settlement"),
        }
        self.pool.settled.notify_waiters();
    }
}

/// Private bridge for mssql-python's internal loop-bound pool wrapper.
#[pyclass(name = "_AsyncPool")]
pub(crate) struct PyAsyncPool {
    pub(crate) inner: Arc<PoolAdapter>,
    owner_loop: Py<PyAny>,
    process: u32,
}

impl PyAsyncPool {
    pub(crate) fn check_owner(&self, py: Python<'_>) -> PyResult<()> {
        if self.process != std::process::id() {
            return Err(InterfaceError::new_err(
                "Async pool cannot be used after fork",
            ));
        }

        let current = py
            .import("asyncio")?
            .call_method0("get_running_loop")
            .map_err(|_| InterfaceError::new_err("Async pool requires a running event loop"))?;
        if !current.is(self.owner_loop.bind(py)) {
            return Err(InterfaceError::new_err(
                "Async pool requires its owning event loop",
            ));
        }
        Ok(())
    }
}

impl PyAsyncPool {
    fn close_awaitable<'py>(
        &self,
        py: Python<'py>,
        block_error: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.check_owner(py)?;
        let deadline = Instant::now() + self.inner.config.shutdown_timeout();
        self.inner.stop()?;
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let result = inner.shutdown(deadline).await;
            crate::async_errors::finish_cleanup(
                result,
                block_error,
                "Pool shutdown failed; outstanding connections must still be closed",
            )
        })
    }
}

#[pymethods]
impl PyAsyncPool {
    #[new]
    #[pyo3(signature = (*, acquire_timeout, cleanup_timeout, shutdown_timeout, max_size=100, max_waiting=None, idle_timeout=Duration::from_secs(600)))]
    fn new(
        py: Python<'_>,
        acquire_timeout: &Bound<'_, PyAny>,
        cleanup_timeout: &Bound<'_, PyAny>,
        shutdown_timeout: &Bound<'_, PyAny>,
        #[pyo3(from_py_with = max_size)] max_size: usize,
        max_waiting: Option<&Bound<'_, PyAny>>,
        #[pyo3(from_py_with = idle_timeout)] idle_timeout: Duration,
    ) -> PyResult<Self> {
        let owner_loop = py
            .import("asyncio")?
            .call_method0("get_running_loop")
            .map_err(|_| InterfaceError::new_err("Async pool requires a running event loop"))?
            .unbind();
        let mut config = PoolConfig::new(
            duration(acquire_timeout, "acquire_timeout", false)?,
            duration(cleanup_timeout, "cleanup_timeout", false)?,
            duration(shutdown_timeout, "shutdown_timeout", false)?,
        )
        .map_err(map_pool_error)?;
        config = config.with_max_size(max_size).map_err(map_pool_error)?;
        if let Some(value) = max_waiting {
            config = config
                .with_max_waiting(Some(limit(value, "max_waiting")?))
                .map_err(map_pool_error)?;
        }
        config = config
            .with_idle_timeout(idle_timeout)
            .map_err(map_pool_error)?;
        let runtime = pyo3_async_runtimes::tokio::get_runtime();
        let _entered = runtime.enter();
        let manager = AsyncPoolManager::new(config.clone()).map_err(map_pool_error)?;
        manager.enable(config.clone()).map_err(map_pool_error)?;
        Ok(Self {
            inner: Arc::new(PoolAdapter {
                manager,
                config,
                attempts: Mutex::new(Attempts {
                    closed: false,
                    next_id: 0,
                    pending: HashMap::new(),
                }),
                settled: Notify::new(),
            }),
            owner_loop,
            process: std::process::id(),
        })
    }

    fn enable(&self, py: Python<'_>) -> PyResult<()> {
        self.check_owner(py)?;
        if self.inner.lock()?.closed {
            return Err(map_pool_error(PoolError::Closed));
        }
        self.inner
            .manager
            .enable(self.inner.config.clone())
            .map_err(map_pool_error)
    }

    fn disable(&self, py: Python<'_>) -> PyResult<()> {
        self.check_owner(py)?;
        self.inner.manager.disable();
        Ok(())
    }

    fn aclose<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.close_awaitable(py, None)
    }

    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        {
            let pool = slf.borrow(py);
            pool.check_owner(py)?;
            if pool.inner.lock()?.closed {
                return Err(map_pool_error(PoolError::Closed));
            }
        }
        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(slf) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        exc_type: &Bound<'_, PyAny>,
        exc_val: &Bound<'_, PyAny>,
        _exc_tb: &Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close_awaitable(py, (!exc_type.is_none()).then(|| exc_val.clone().unbind()))
    }
}

#[cfg(test)]
mod tests;
