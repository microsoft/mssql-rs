// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::async_handle::{CLOSE_TIMEOUT, CloseMode};
use crate::pool::connection::{
    AuthBinding, AuthMetadata, ConnectSpec, OpenedConnection, PoolKey, TokenExpiry,
};
use async_trait::async_trait;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::core::CancelHandle;
use mssql_tds::test_client_support::{
    done_no_more, env_change_reset_connection, tds_client_from_tokens_capturing,
};

struct Factory {
    clients: Mutex<VecDeque<OpenedConnection>>,
    calls: AtomicUsize,
}

#[async_trait]
impl ConnectionFactory for Factory {
    async fn open(&self, _: &ConnectSpec, _: &CancelHandle) -> Result<OpenedConnection, OpenError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.clients.lock().unwrap().pop_front().ok_or_else(|| {
            OpenError::Provider(Box::new(std::io::Error::other("no scripted client")))
        })
    }
}

fn spec() -> ConnectSpec {
    let mut context = ClientContext::with_data_source("tcp:localhost,1433");
    context.connect_retry_count = 0;
    context.user_name = "test-user".into();
    context.password = "test-password".into();
    ConnectSpec::new(context, AuthBinding::new(Box::new([1])).unwrap())
}

fn manager() -> AsyncPoolManager {
    let config = PoolConfig::new(CLOSE_TIMEOUT, CLOSE_TIMEOUT, CLOSE_TIMEOUT)
        .unwrap()
        .with_max_size(1)
        .unwrap();
    let manager = AsyncPoolManager::new(config.clone()).unwrap();
    manager.enable(config).unwrap();
    manager
}

#[tokio::test]
async fn shutdown_rejects_an_expired_original_deadline_when_already_settled() {
    let pool = PoolAdapter {
        manager: manager(),
        config: PoolConfig::new(CLOSE_TIMEOUT, CLOSE_TIMEOUT, CLOSE_TIMEOUT).unwrap(),
        attempts: Mutex::new(Attempts {
            closed: false,
            next_id: 0,
            pending: HashMap::new(),
        }),
        settled: Notify::new(),
    };
    pool.stop().unwrap();
    let error = pool.shutdown(Instant::now()).await.unwrap_err();
    Python::attach(|py| {
        assert!(error.is_instance_of::<OperationalError>(py));
        assert_eq!(
            error
                .value(py)
                .getattr("pool_reason")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "ShutdownTimeout"
        );
    });
}

async fn acquire(manager: &AsyncPoolManager, factory: &Factory) -> crate::pool::session::PoolLease {
    manager
        .acquire(
            PoolKey::new(Box::new([1])).unwrap(),
            &spec(),
            factory,
            isolation(None).unwrap(),
            AcquireControl::from_timeout(Instant::now(), CLOSE_TIMEOUT, CancellationToken::new())
                .unwrap(),
        )
        .await
        .unwrap()
        .unwrap()
}

#[test]
fn logical_close_returns_the_lease_and_revokes_old_owners() {
    pyo3_async_runtimes::tokio::get_runtime().block_on(async {
        let manager = manager();
        let (client, transport) = tds_client_from_tokens_capturing(vec![
            done_no_more(),
            env_change_reset_connection(),
            done_no_more(),
        ]);
        let factory = Factory {
            clients: Mutex::new(VecDeque::from([OpenedConnection::new(
                client,
                AuthMetadata {
                    binding: spec().expected_auth().clone(),
                    expiry: TokenExpiry::NonToken,
                },
            )])),
            calls: AtomicUsize::new(0),
        };
        let mut lease = acquire(&manager, &factory).await;
        let physical_id = lease.client_mut().client_connection_id();
        let old = Arc::new(LogicalConnection::from_handle(
            ConnHandle::Pooled(Box::new(lease)),
            CLOSE_TIMEOUT,
        ));
        let cursor = old.clone();
        let operation = old.state.claim_execute(1).unwrap();
        old.state.finish_execute(operation.operation_id, true);
        LogicalConnection::wait_closed(old.start_close(CloseMode::Rollback, CLOSE_TIMEOUT, None))
            .await
            .unwrap();

        let mut next = acquire(&manager, &factory).await;
        assert_eq!(next.client_mut().client_connection_id(), physical_id);
        assert!(matches!(
            cursor.lock_operation(operation.operation_id).await,
            Err(crate::async_session::ClaimError::Closed)
        ));
        old.state.abandon_cursor(1);
        drop(cursor);
        drop(old);
        assert!(!next.client_mut().is_connection_dead());
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&transport), 2);
        drop(next);
        assert_eq!(Arc::strong_count(&transport), 1);
        manager.shutdown().await.unwrap();
    });
}

#[test]
fn failed_python_delivery_discards_even_if_a_python_object_retains_the_cell() {
    let runtime = pyo3_async_runtimes::tokio::get_runtime();
    let (manager, logical, transport) = runtime.block_on(async {
        let manager = manager();
        let (client, transport) = tds_client_from_tokens_capturing(vec![done_no_more()]);
        let factory = Factory {
            clients: Mutex::new(VecDeque::from([OpenedConnection::new(
                client,
                AuthMetadata {
                    binding: spec().expected_auth().clone(),
                    expiry: TokenExpiry::NonToken,
                },
            )])),
            calls: AtomicUsize::new(0),
        };
        let lease = acquire(&manager, &factory).await;
        let logical = Arc::new(LogicalConnection::from_handle(
            ConnHandle::Pooled(Box::new(lease)),
            CLOSE_TIMEOUT,
        ));
        (manager, logical, transport)
    });
    let delivery = ConnectDelivery::new(None, None);
    delivery.store(logical.clone()).unwrap();
    Python::attach(|py| {
        let globals = PyDict::new(py);
        globals
            .set_item(
                "retained",
                Py::new(
                    py,
                    PyAsyncConnection::from_logical(logical.clone(), None, false),
                )
                .unwrap(),
            )
            .unwrap();
        let callback = PyCFunction::new_closure(py, None, None, move |args, _| {
            let future = pyo3_async_runtimes::tokio::into_future(args.get_item(0)?)?;
            delivery
                .bind(args.py(), Box::pin(future))
                .map(Bound::unbind)
        })
        .unwrap();
        globals.set_item("bind", callback).unwrap();
        py.run(
            cr"
import asyncio
async def run():
    future = asyncio.get_running_loop().create_future()
    public = bind(future)
    future.cancel()
    try:
        await public
    except asyncio.CancelledError:
        pass
    else:
        raise AssertionError('Cancelled native work was delivered')
    try:
        retained.cursor()
    except Exception as error:
        assert 'broken' in str(error)
    else:
        raise AssertionError('Undelivered connection remained usable')
asyncio.run(run())
",
            Some(&globals),
            None,
        )
        .unwrap();
    });
    assert_eq!(Arc::strong_count(&transport), 1);
    runtime.block_on(manager.shutdown()).unwrap();
}

#[test]
fn failed_result_publication_settles_the_public_future() {
    Python::attach(|py| {
        let delivery = ConnectDelivery::new(None, None);
        let bind = PyCFunction::new_closure(py, None, None, move |args, _| {
            delivery
                .bind(args.py(), Box::pin(async { Ok(()) }))
                .map(Bound::unbind)
        })
        .unwrap();
        let globals = PyDict::new(py);
        globals.set_item("bind", bind).unwrap();
        py.run(
            cr"
import asyncio

class PublicationFailure(asyncio.Future):
    def set_result(self, value):
        raise ValueError('publication refused')

async def run():
    loop = asyncio.get_running_loop()
    create_future = loop.create_future
    first = True
    def create():
        nonlocal first
        if first:
            first = False
            return PublicationFailure(loop=loop)
        return create_future()
    loop.create_future = create
    try:
        public = bind()
    finally:
        loop.create_future = create_future
    try:
        await asyncio.wait_for(public, 1)
    except ValueError as error:
        assert str(error) == 'publication refused'
    else:
        raise AssertionError('Publication failure was lost')
asyncio.run(run())
",
            Some(&globals),
            None,
        )
        .unwrap();
    });
}

#[test]
fn closed_loop_discards_undelivered_connection_with_retained_public_future() {
    let runtime = pyo3_async_runtimes::tokio::get_runtime();
    for close_before_completion in [true, false] {
        let pool = Arc::new(PoolAdapter {
            manager: runtime.block_on(async { manager() }),
            config: PoolConfig::new(CLOSE_TIMEOUT, CLOSE_TIMEOUT, CLOSE_TIMEOUT).unwrap(),
            attempts: Mutex::new(Attempts {
                closed: false,
                next_id: 0,
                pending: HashMap::new(),
            }),
            settled: Notify::new(),
        });
        let (logical, transport) = runtime.block_on(async {
            let (client, transport) = tds_client_from_tokens_capturing(vec![done_no_more()]);
            let factory = Factory {
                clients: Mutex::new(VecDeque::from([OpenedConnection::new(
                    client,
                    AuthMetadata {
                        binding: spec().expected_auth().clone(),
                        expiry: TokenExpiry::NonToken,
                    },
                )])),
                calls: AtomicUsize::new(0),
            };
            let lease = acquire(&pool.manager, &factory).await;
            (
                Arc::new(LogicalConnection::from_handle(
                    ConnHandle::Pooled(Box::new(lease)),
                    CLOSE_TIMEOUT,
                )),
                transport,
            )
        });
        let delivery = ConnectDelivery::new(Some(pool.register().unwrap()), None);
        let (release, wait) = tokio::sync::oneshot::channel();
        Python::attach(|py| {
            let globals = PyDict::new(py);
            globals
                .set_item("close_before_completion", close_before_completion)
                .unwrap();
            let wait = Mutex::new(Some(wait));
            let start = PyCFunction::new_closure(py, None, None, move |args, _| {
                let wait = wait.lock().unwrap().take().unwrap();
                let connection = logical.clone();
                let future_delivery = delivery.clone();
                let future = async move {
                    wait.await.unwrap();
                    future_delivery.store(connection.clone())?;
                    Python::attach(|py| {
                        Py::new(py, PyAsyncConnection::from_logical(connection, None, false))
                    })
                };
                delivery
                    .bind(args.py(), Box::pin(future))
                    .map(Bound::unbind)
            })
            .unwrap();
            let release = Mutex::new(Some(release));
            let release = PyCFunction::new_closure(py, None, None, move |_, _| {
                release.lock().unwrap().take().unwrap().send(()).unwrap();
                Ok::<(), PyErr>(())
            })
            .unwrap();
            globals.set_item("start", start).unwrap();
            globals.set_item("release", release).unwrap();
            py.run(
                cr"
import asyncio
import threading

loop = asyncio.new_event_loop()
publication_attempted = threading.Event()
call_soon_threadsafe = loop.call_soon_threadsafe
def publish(*args, **kwargs):
    try:
        return call_soon_threadsafe(*args, **kwargs)
    finally:
        publication_attempted.set()
loop.call_soon_threadsafe = publish
async def setup():
    return start()
retained_public = loop.run_until_complete(setup())
if close_before_completion:
    loop.close()
release()
assert publication_attempted.wait(5)
if not close_before_completion:
    loop.close()
assert not retained_public.done()
",
                Some(&globals),
                None,
            )
            .unwrap();
            py.detach(|| {
                pool.stop().unwrap();
                runtime.block_on(pool.shutdown(Instant::now() + CLOSE_TIMEOUT))
            })
            .unwrap();
            assert_eq!(Arc::strong_count(&transport), 1);
        });
    }
}

#[test]
fn late_operation_waiter_cancellation_discards_before_pool_return() {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Waker};
    use tracing::{Subscriber, span::Attributes};
    use tracing_subscriber::{Layer, layer::Context as LayerContext, prelude::*};

    type Waiter = Pin<Box<dyn Future<Output = PyResult<()>> + Send>>;
    struct CancelDuringRelease {
        drains: AtomicUsize,
        waiter: Mutex<Option<Waiter>>,
    }
    impl<S: Subscriber> Layer<S> for CancelDuringRelease {
        fn on_new_span(
            &self,
            attributes: &Attributes<'_>,
            _: &tracing::span::Id,
            _: LayerContext<'_, S>,
        ) {
            if attributes.metadata().name() == "close_query"
                && self.drains.fetch_add(1, Ordering::SeqCst) == 1
            {
                drop(self.waiter.lock().unwrap().take().unwrap());
            }
        }
    }

    pyo3_async_runtimes::tokio::get_runtime().block_on(async {
        for mode in [CloseMode::Commit, CloseMode::Rollback] {
            let manager = manager();
            let (client, transport) = tds_client_from_tokens_capturing(vec![
                done_no_more(),
                env_change_reset_connection(),
                done_no_more(),
            ]);
            let (replacement, _) = tds_client_from_tokens_capturing(vec![done_no_more()]);
            let factory = Factory {
                clients: Mutex::new(
                    [client, replacement]
                        .into_iter()
                        .map(|client| {
                            OpenedConnection::new(
                                client,
                                AuthMetadata {
                                    binding: spec().expected_auth().clone(),
                                    expiry: TokenExpiry::NonToken,
                                },
                            )
                        })
                        .collect(),
                ),
                calls: AtomicUsize::new(0),
            };
            let lease = acquire(&manager, &factory).await;
            let logical = Arc::new(LogicalConnection::from_handle(
                ConnHandle::Pooled(Box::new(lease)),
                CLOSE_TIMEOUT,
            ));
            let (release, wait) = tokio::sync::oneshot::channel();
            let (finished, completion) = tokio::sync::oneshot::channel();
            let mut waiter: Waiter = Box::pin(logical.state.clone().run_tracked(async move {
                wait.await.unwrap();
                finished.send(()).unwrap();
                Ok(())
            }));
            assert!(
                waiter
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            release.send(()).unwrap();
            completion.await.unwrap();
            let subscriber = tracing_subscriber::Registry::default().with(CancelDuringRelease {
                drains: AtomicUsize::new(0),
                waiter: Mutex::new(Some(waiter)),
            });
            let result = LogicalConnection::wait_closed(logical.start_close(
                mode,
                CLOSE_TIMEOUT,
                Some(tracing::Dispatch::new(subscriber)),
            ))
            .await;
            assert!(logical.state.is_broken());
            assert_eq!(result.is_err(), matches!(mode, CloseMode::Commit));
            assert_eq!(Arc::strong_count(&transport), 1);
            let next = acquire(&manager, &factory).await;
            assert_eq!(factory.calls.load(Ordering::SeqCst), 2);
            drop(next);
            manager.shutdown().await.unwrap();
        }
    });
}

#[tokio::test]
async fn close_waiters_preserve_pool_error_reason() {
    let error = map_pool_error(PoolError::CleanupTimeout);
    let (_sender, receiver) = tokio::sync::watch::channel(Some(Err(Arc::new(error))));
    for _ in 0..2 {
        let error = LogicalConnection::wait_closed(receiver.clone())
            .await
            .unwrap_err();
        Python::attach(|py| {
            assert!(error.is_instance_of::<OperationalError>(py));
            assert_eq!(
                error
                    .value(py)
                    .getattr("pool_reason")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "CleanupTimeout",
            );
        });
    }
}

#[test]
fn delivery_after_cancellation_cannot_install_a_client() {
    let (client, transport) = tds_client_from_tokens_capturing(vec![]);
    let logical = Arc::new(LogicalConnection::new(client));
    let delivery = ConnectDelivery::new(None, None);
    delivery.finish(true).unwrap();
    assert!(delivery.store(logical.clone()).is_err());
    assert_eq!(Arc::strong_count(&transport), 1);
}

#[test]
fn pool_errors_keep_dbapi_categories_and_native_reasons() {
    Python::attach(|py| {
        for error in [
            PoolError::WaitQueueFull,
            PoolError::CleanupTimeout,
            PoolError::ShutdownTimeout,
            PoolError::AcquireTimeout {
                stage: AcquireStage::Delivery,
            },
        ] {
            let mapped = map_pool_error(error);
            assert!(mapped.is_instance_of::<OperationalError>(py));
            assert!(mapped.value(py).hasattr("pool_reason").unwrap());
        }

        let invalid = map_pool_error(PoolError::InvalidRequest {
            field: "auth",
            reason: "unsupported",
        });
        assert!(invalid.is_instance_of::<InterfaceError>(py));
        assert!(map_pool_error(PoolError::Closed).is_instance_of::<InterfaceError>(py));
        let cancelled = map_pool_error(PoolError::Cancelled);
        assert!(
            cancelled.is_instance(
                py,
                &py.import("asyncio")
                    .unwrap()
                    .getattr("CancelledError")
                    .unwrap()
            )
        );
        let internal = map_pool_error(PoolError::Internal { reason: "sentinel" });
        assert!(internal.is_instance_of::<InternalError>(py));
    });
}

#[test]
fn original_deadline_is_checked_before_public_python_delivery() {
    let (client, transport) = tds_client_from_tokens_capturing(vec![]);
    let logical = Arc::new(LogicalConnection::new(client));
    let delivery = ConnectDelivery::new(None, Some(Instant::now()));
    delivery.store(logical.clone()).unwrap();
    Python::attach(|py| {
        let globals = PyDict::new(py);
        globals
            .set_item("OperationalError", py.get_type::<OperationalError>())
            .unwrap();
        let bind = PyCFunction::new_closure(py, None, None, move |args, _| {
            let future = pyo3_async_runtimes::tokio::into_future(args.get_item(0)?)?;
            delivery
                .bind(args.py(), Box::pin(future))
                .map(Bound::unbind)
        })
        .unwrap();
        globals.set_item("bind", bind).unwrap();
        py.run(
            cr"
import asyncio
async def run():
    native = asyncio.get_running_loop().create_future()
    public = bind(native)
    native.set_result(object())
    try:
        await public
    except OperationalError as error:
        assert error.pool_reason == 'AcquireTimeout'
        assert 'delivery' in str(error).lower()
    else:
        raise AssertionError('Expired acquisition was delivered')
asyncio.run(run())
",
            Some(&globals),
            None,
        )
        .unwrap();
    });
    assert_eq!(Arc::strong_count(&transport), 1);
}
