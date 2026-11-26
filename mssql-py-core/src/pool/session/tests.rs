// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags, StreamedSqlType};
use mssql_tds::test_client_support::*;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::super::config::PoolConfig;
use super::super::connection::{AuthBinding, AuthMetadata, TokenExpiry};
use super::super::error::OpenError;
use super::*;

struct Factory {
    clients: Mutex<VecDeque<OpenedConnection>>,
    calls: AtomicUsize,
    delay: Duration,
}

impl Factory {
    fn new(clients: Vec<OpenedConnection>) -> Self {
        Self {
            clients: Mutex::new(clients.into()),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
        }
    }
}

#[async_trait]
impl ConnectionFactory for Factory {
    async fn open(&self, _: &ConnectSpec, _: &CancelHandle) -> Result<OpenedConnection, OpenError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // A partial login owns its client while suspended.
        let client = self.clients.lock().unwrap().pop_front();
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        client.ok_or_else(|| OpenError::Provider(Box::new(std::io::Error::other("open sentinel"))))
    }
}

fn binding() -> AuthBinding {
    AuthBinding::new(vec![1].into_boxed_slice()).unwrap()
}

fn spec() -> ConnectSpec {
    let mut context = ClientContext::with_data_source("tcp:localhost,1433");
    context.connect_retry_count = 0;
    context.user_name = "test-user".into();
    context.password = "test-password".into();
    ConnectSpec::new(context, binding())
}

fn opened(client: TdsClient) -> OpenedConnection {
    OpenedConnection::new(
        client,
        AuthMetadata {
            binding: binding(),
            expiry: TokenExpiry::NonToken,
        },
    )
}

fn key() -> PoolKey {
    PoolKey::new(vec![1].into_boxed_slice()).unwrap()
}

fn control(cancel: CancellationToken) -> AcquireControl {
    AcquireControl::from_timeout(Instant::now(), Duration::from_secs(10), cancel).unwrap()
}

fn isolation() -> Isolation {
    TransactionIsolationLevel::ReadCommitted.try_into().unwrap()
}

fn manager(size: usize) -> AsyncPoolManager {
    let budget = Duration::from_secs(10);
    let config = PoolConfig::new(budget, budget, budget)
        .unwrap()
        .with_max_size(size)
        .unwrap();
    let manager = AsyncPoolManager::new(config.clone()).unwrap();
    manager.enable(config).unwrap();
    manager
}

async fn cache(manager: &AsyncPoolManager, client: OpenedConnection) {
    let mut slot = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    slot.install(client).unwrap();
    slot.return_quiescent().unwrap();
}

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn isolation_no_change_is_rejected() {
    assert!(matches!(
        Isolation::try_from(TransactionIsolationLevel::NoChange),
        Err(PoolError::InvalidRequest {
            field: "isolation_level",
            ..
        })
    ));
}

#[tokio::test]
async fn initialize_rejects_unfinished_streamed_write_before_carrier() {
    for reuse in [false, true] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut client =
            tds_client_from_tokens_gated(vec![], Arc::new(Semaphore::new(0)), dropped.clone());
        client
            .begin_sp_executesql(
                "SELECT @v".into(),
                vec![RpcParameter::data_at_exec(
                    Some("@v".into()),
                    StatusFlags::NONE,
                    StreamedSqlType::VarBinaryMax,
                )],
                (),
            )
            .await
            .unwrap();
        client.write_streamed_chunk(&[1; 8192]).await.unwrap();
        assert!(!client.has_open_batch());
        assert!(matches!(
            initialize(opened(client), &spec(), &isolation(), reuse).await,
            Err(PoolError::InvalidSession)
        ));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn fresh_then_cached_checkout_sends_isolation_and_only_reuse_arms_reset() {
    let manager = manager(1);
    let (client, sent) = tds_client_from_tokens_capturing(vec![
        done_no_more(),
        env_change_reset_connection(),
        done_more_with_count(0),
        done_no_more(),
    ]);
    let id = client.client_connection_id();
    let factory = Factory::new(vec![opened(client)]);
    let spec = spec();
    let mut first = manager
        .acquire(
            key(),
            &spec,
            &factory,
            isolation(),
            control(CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.client_mut().client_connection_id(), id);
    assert!(!first.client_mut().has_open_batch());
    assert_eq!(sent.lock().unwrap()[1] & 0x18, 0);
    sent.lock().unwrap().clear();
    first.return_quiescent().unwrap();
    let mut reused = manager
        .acquire(
            key(),
            &spec,
            &factory,
            TransactionIsolationLevel::Serializable.try_into().unwrap(),
            control(CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reused.client_mut().client_connection_id(), id);
    assert!(quiescent(reused.client_mut()));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    let wire = sent.lock().unwrap();
    assert_eq!(wire[1] & 0x18, 0x08);
    let sql: Vec<u8> = "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert!(wire.windows(sql.len()).any(|bytes| bytes == sql));
}

#[tokio::test(start_paused = true)]
async fn interrupted_lease_operations_are_irreversible_and_keep_capacity_until_discard() {
    for mode in 0..3 {
        let manager = manager(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let client =
            tds_client_from_tokens_gated(vec![], Arc::new(Semaphore::new(0)), dropped.clone());
        let mut first = lease(&manager, client).await;
        let cancellation = CancellationToken::new();
        let mut work = Box::pin(first.run(
            Some(Instant::now() + Duration::from_secs(10)),
            &cancellation,
            |_, _| Box::pin(std::future::pending::<TdsResult<()>>()),
        ));
        assert!(poll(work.as_mut()).is_pending());
        let mut waiting = Box::pin(
            manager.reserve(
                key(),
                AcquireControl::from_timeout(
                    Instant::now(),
                    Duration::from_secs(30),
                    CancellationToken::new(),
                )
                .unwrap(),
            ),
        );
        assert!(poll(waiting.as_mut()).is_pending());
        match mode {
            0 => {
                cancellation.cancel();
                assert!(matches!(
                    poll(work.as_mut()),
                    Poll::Ready(Err(PoolError::Cancelled))
                ));
            }
            1 => {
                tokio::time::advance(Duration::from_secs(10)).await;
                assert!(matches!(
                    poll(work.as_mut()),
                    Poll::Ready(Err(PoolError::AcquireTimeout {
                        stage: AcquireStage::Execute
                    }))
                ));
            }
            _ => {}
        }
        drop(work);
        assert!(first.discard);
        assert!(first.client_mut().is_connection_dead());
        assert!(poll(waiting.as_mut()).is_pending());
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert!(matches!(
            first
                .run(None, &CancellationToken::new(), |_, _| Box::pin(async {
                    Ok(())
                }))
                .await,
            Err(PoolError::InvalidSession)
        ));
        first.release().await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        drop(waiting.await.unwrap().unwrap());
    }
}

#[tokio::test]
async fn completed_lease_operations_preserve_clean_sessions_but_dead_failures_discard() {
    for dead in [false, true] {
        let manager = manager(1);
        let mut first = lease(&manager, tds_client_from_tokens(vec![done_no_more()])).await;
        let result = first
            .run(None, &CancellationToken::new(), move |client, options| {
                Box::pin(async move {
                    if dead {
                        client.mark_connection_dead();
                        return Err(mssql_tds::error::Error::ConnectionError(
                            "private failure".into(),
                        ));
                    }
                    client.execute("SELECT 1".into(), options).await?;
                    client.close_query().await
                })
            })
            .await;
        assert_eq!(first.discard, dead);
        if dead {
            assert!(matches!(result, Err(PoolError::Execute(_))));
        } else {
            result.unwrap();
        }
        first.release().await.unwrap();
        let mut slot = manager
            .reserve(key(), control(CancellationToken::new()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slot.take_value().is_none(), dead);
    }
}

#[tokio::test(start_paused = true)]
async fn no_command_deadline_does_not_limit_normal_execution() {
    let manager = manager(1);
    let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
    first
        .run(None, &CancellationToken::new(), |_, _| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(86_400)).await;
                Ok(())
            })
        })
        .await
        .unwrap();
    assert!(!first.discard);
    assert!(!first.client_mut().is_connection_dead());
}

#[tokio::test]
async fn connection_closed_discards_even_without_a_cached_dead_flag() {
    let manager = manager(1);
    let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
    let result = first
        .run(None, &CancellationToken::new(), |client, _| {
            Box::pin(async move {
                assert!(!client.is_connection_dead());
                Err::<(), _>(mssql_tds::error::Error::ConnectionClosed(
                    "idle probe found a closed connection".into(),
                ))
            })
        })
        .await;
    assert!(matches!(result, Err(PoolError::Execute(_))));
    assert!(first.discard);
    assert!(first.client_mut().is_connection_dead());
    first.release().await.unwrap();
    let mut slot = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(slot.take_value().is_none());
}

#[tokio::test]
async fn command_budget_longer_than_login_timeout_allows_slow_recovery() {
    use mssql_mock_tds::{MockTdsServer, QueryResponse};
    use mssql_tds::connection::client_context::{EntraIdTokenFactory, TdsAuthenticationMethod};
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::EncryptionSetting;

    #[derive(Clone)]
    struct SlowRecoveryCredential(Arc<AtomicUsize>);

    #[async_trait]
    impl EntraIdTokenFactory for SlowRecoveryCredential {
        async fn create_token(
            &self,
            _: String,
            _: String,
            _: TdsAuthenticationMethod,
        ) -> TdsResult<Vec<u8>> {
            if self.0.fetch_add(1, Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok("opaque-test-token"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect())
        }
    }

    let server = MockTdsServer::new(
        &std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0).to_string(),
    )
    .await
    .unwrap();
    let address = server.local_addr();
    {
        let registry = server.query_registry();
        let mut registry = registry.lock().await;
        registry.set_session_recovery(true);
        registry.register("SELECT 1", QueryResponse::select_one());
    }
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.run_with_shutdown(shutdown_rx));
    let mut context = ClientContext::default();
    context.connect_timeout = 1;
    context.connect_retry_count = 1;
    context.encryption_options.mode = EncryptionSetting::PreferOff;
    context.tds_authentication_method = TdsAuthenticationMethod::ActiveDirectoryTokenCredential;
    context.auth_method_map.insert(
        TdsAuthenticationMethod::ActiveDirectoryTokenCredential,
        Box::new(SlowRecoveryCredential(Arc::new(AtomicUsize::new(0)))),
    );
    let client = TdsConnectionProvider
        .create_client(
            context,
            &format!("tcp:{},{}", address.ip(), address.port()),
            None,
        )
        .await
        .unwrap();
    let manager = manager(1);
    let mut first = lease(&manager, client).await;
    first.client_mut().mark_connection_dead();
    first
        .run(
            Some(Instant::now() + Duration::from_secs(4)),
            &CancellationToken::new(),
            |client, options| {
                Box::pin(async move {
                    client.execute("SELECT 1".into(), options).await?;
                    client.close_query().await
                })
            },
        )
        .await
        .unwrap();
    assert_eq!(first.client_mut().connection_recovery_count(), 1);
    first.release().await.unwrap();
    manager.shutdown().await.unwrap();
    shutdown.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn operation_options_preserve_remaining_and_subsecond_deadlines() {
    for (budget, expected) in [
        (None, None),
        (Some(Duration::from_secs(60)), Some(59)),
        (Some(Duration::from_millis(500)), Some(1)),
    ] {
        let manager = manager(1);
        let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
        let deadline = budget.map(|budget| Instant::now() + budget);
        let cancellation = CancellationToken::new();
        let work = first.run(deadline, &cancellation, |_, options| {
            assert_eq!(options.timeout, expected);
            assert!(options.cancel.is_some());
            Box::pin(async { Ok(()) })
        });
        if budget == Some(Duration::from_secs(60)) {
            tokio::time::advance(Duration::from_millis(1100)).await;
        }
        work.await.unwrap();
    }
}

#[tokio::test]
async fn provider_cancellation_discards_the_lease_without_caching_its_client() {
    let manager = manager(1);
    let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
    let result = first
        .run(None, &CancellationToken::new(), |_, _| {
            Box::pin(async { Err::<(), _>(mssql_tds::error::Error::TokenAcquisitionCancelled) })
        })
        .await;
    assert!(matches!(
        result,
        Err(PoolError::Execute(error))
            if matches!(error.as_ref(), mssql_tds::error::Error::TokenAcquisitionCancelled)
    ));
    assert!(first.discard);
    assert!(first.client_mut().is_connection_dead());
    first.release().await.unwrap();
    let mut slot = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(slot.take_value().is_none());
}

#[tokio::test(start_paused = true)]
async fn interruption_during_successful_operation_blocks_result_publication() {
    for timeout in [false, true] {
        let manager = manager(1);
        let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let deadline = timeout.then(|| Instant::now() + Duration::from_secs(1));
        let result = first
            .run(deadline, &cancellation, move |_, _| {
                Box::pin(async move {
                    if timeout {
                        tokio::time::advance(Duration::from_secs(2)).await;
                    } else {
                        signal.cancel();
                    }
                    Ok(())
                })
            })
            .await;
        assert!(matches!(
            result,
            Err(PoolError::Cancelled | PoolError::AcquireTimeout { .. })
        ));
        assert!(first.discard);
        assert!(first.client_mut().is_connection_dead());
    }
}

async fn operation_auth_factory() -> (
    crate::async_entra_token_factory::AsyncPythonEntraIdTokenFactory,
    pyo3::Py<pyo3::PyAny>,
) {
    use pyo3::prelude::*;
    use pyo3::types::PyAnyMethods;

    let (locals, provider, event_loop) = Python::attach(|py| {
        let event_loop = py
            .import("asyncio")
            .unwrap()
            .call_method0("new_event_loop")
            .unwrap();
        let locals = pyo3_async_runtimes::TaskLocals::new(event_loop.clone())
            .copy_context(py)
            .unwrap();
        let provider = py
            .import("types")
            .unwrap()
            .getattr("SimpleNamespace")
            .unwrap()
            .call0()
            .unwrap();
        provider
            .setattr(
                "get_token",
                py.import("builtins").unwrap().getattr("str").unwrap(),
            )
            .unwrap();
        (locals, provider.unbind(), event_loop.unbind())
    });
    let factory = pyo3_async_runtimes::tokio::scope(locals, async {
        Python::attach(|py| {
            crate::async_entra_token_factory::AsyncPythonEntraIdTokenFactory::new(py, provider)
        })
    })
    .await
    .unwrap();
    (factory, event_loop)
}

#[test]
fn recovery_binding_change_during_token_acquisition_sends_no_application_batch() {
    use pyo3::prelude::*;
    use pyo3::types::PyModule;

    Python::attach(|py| {
        let event_loop = py
            .import("asyncio")
            .unwrap()
            .call_method0("new_event_loop")
            .unwrap();
        let locals = pyo3_async_runtimes::TaskLocals::new(event_loop.clone())
            .copy_context(py)
            .unwrap();
        let provider = PyModule::from_code(
            py,
            c"import time\nfrom types import SimpleNamespace\nclass Credential:\n    def __init__(self):\n        self.calls = 0\n        self.epoch = 1\n    async def get_token(self, scope):\n        self.calls += 1\n        if self.calls == 2:\n            self.epoch = 2\n        return SimpleNamespace(token='opaque', expires_on=int(time.time()) + 3600)\n",
            c"pool_recovery_credential.py",
            c"pool_recovery_credential",
        )
        .unwrap()
        .getattr("Credential").unwrap().call0().unwrap().unbind();
        let future =
            pyo3_async_runtimes::tokio::future_into_py_with_locals(py, locals, async move {
                use mssql_mock_tds::{MockTdsServer, QueryResponse};
                use mssql_tds::connection::client_context::TdsAuthenticationMethod;
                use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

                let server = MockTdsServer::new(
                    &std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0).to_string(),
                )
                .await
                .unwrap();
                let address = server.local_addr();
                let connections = server.connection_store();
                {
                    let registry = server.query_registry();
                    let mut registry = registry.lock().await;
                    registry.set_session_recovery(true);
                    registry.register("SELECT 1", QueryResponse::select_one());
                }
                let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
                let task = tokio::spawn(server.run_with_shutdown(shutdown_rx));
                let factory = || {
                    Python::attach(|py| {
                        crate::async_entra_token_factory::AsyncPythonEntraIdTokenFactory::new(
                            py,
                            provider.clone_ref(py),
                        )
                        .unwrap()
                    })
                };
                let retained =
                    super::super::auth::RecoveryAuth::new(binding(), "test-tenant".into()).unwrap();
                let reader = || {
                    let provider = Python::attach(|py| provider.clone_ref(py));
                    move || {
                        Python::attach(|py| {
                            let epoch = provider
                                .getattr(py, "epoch")
                                .unwrap()
                                .extract::<u8>(py)
                                .unwrap();
                            Ok(AuthBinding::new(vec![epoch].into_boxed_slice()).unwrap())
                        })
                    }
                };
                let initial = retained
                    .activate(factory(), reader(), Duration::ZERO)
                    .unwrap();
                let mut context = ClientContext::with_data_source(&format!(
                    "tcp:{},{}",
                    address.ip(),
                    address.port()
                ));
                context.connect_retry_count = 1;
                context.tds_authentication_method =
                    TdsAuthenticationMethod::ActiveDirectoryServicePrincipal;
                context.encryption_options = EncryptionOptions {
                    mode: EncryptionSetting::PreferOff,
                    trust_server_certificate: true,
                    ..Default::default()
                };
                let spec = ConnectSpec::new(context, binding());
                let opened = initial.open(&spec, &CancelHandle::new()).await.unwrap();
                initial.complete().unwrap();
                let manager = manager(1);
                let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
                *first.slot.value_mut().unwrap() = opened;
                assert!(first.client_mut().is_session_recovery_enabled());
                first.client_mut().mark_connection_dead();
                let scope = first
                    .activate_auth(factory(), reader(), Duration::ZERO)
                    .unwrap();
                let result = first
                    .run_authenticated(None, &CancellationToken::new(), scope, |client, options| {
                        Box::pin(async move { client.execute("SELECT 1".into(), options).await })
                    })
                    .await;
                assert!(result.is_err());
                assert!(first.discard);
                assert!(first.client_mut().is_connection_dead());
                assert_eq!(first.client_mut().connection_recovery_count(), 0);
                first.release().await.unwrap();
                manager.shutdown().await.unwrap();
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        {
                            let store = connections.lock().await;
                            if store.is_transport_closed(0) && store.is_transport_closed(1) {
                                break;
                            }
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                {
                    let store = connections.lock().await;
                    assert_eq!(store.count(), 2);
                    assert!(
                        store
                            .all()
                            .values()
                            .all(|info| { info.transaction_descriptor_headers.is_empty() }),
                        "account switch sent an application batch"
                    );
                }
                shutdown.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                Ok(())
            })
            .unwrap();
        event_loop
            .call_method1("run_until_complete", (future,))
            .unwrap();
        event_loop.call_method0("close").unwrap();
    });
}

#[tokio::test]
async fn authenticated_operation_rechecks_binding_before_work_and_publication() {
    for mode in 0..3 {
        let switch_before_work = mode == 1;
        let (factory, event_loop) = operation_auth_factory().await;
        let retained = super::super::auth::RecoveryAuth::new(binding(), "tenant".into()).unwrap();
        let manager = manager(1);
        let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
        *first.slot.value_mut().unwrap() = OpenedConnection::with_recovery(
            tds_client_from_tokens(vec![]),
            AuthMetadata {
                binding: binding(),
                expiry: TokenExpiry::NonToken,
            },
            retained,
        );
        let switched = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = switched.clone();
        let scope = if mode == 2 {
            super::super::auth::RecoveryAuth::new(binding(), "tenant".into())
                .unwrap()
                .activate(factory, || Ok(binding()), Duration::ZERO)
                .unwrap()
        } else {
            first
                .activate_auth(
                    factory,
                    move || {
                        Ok(if reader.load(Ordering::Acquire) {
                            AuthBinding::new(vec![2].into_boxed_slice()).unwrap()
                        } else {
                            binding()
                        })
                    },
                    Duration::ZERO,
                )
                .unwrap()
        };
        switched.store(switch_before_work, Ordering::Release);
        let result = first
            .run_authenticated(None, &CancellationToken::new(), scope, move |_, _| {
                Box::pin(async move {
                    assert!(
                        !switch_before_work && mode != 2,
                        "mismatched binding reached application work"
                    );
                    switched.store(true, Ordering::Release);
                    Ok(())
                })
            })
            .await;
        assert!(matches!(result, Err(PoolError::IdentityMismatch)));
        assert!(first.discard);
        assert!(first.client_mut().is_connection_dead());
        pyo3::Python::attach(|py| event_loop.call_method0(py, "close").unwrap());
    }
}

#[tokio::test]
async fn authenticated_binding_change_before_resumption_sends_no_batch() {
    let (factory, event_loop) = operation_auth_factory().await;
    let retained = super::super::auth::RecoveryAuth::new(binding(), "tenant".into()).unwrap();
    let manager = manager(1);
    let (client, sent) = tds_client_from_tokens_capturing(vec![done_no_more()]);
    let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
    *first.slot.value_mut().unwrap() = OpenedConnection::with_recovery(
        client,
        AuthMetadata {
            binding: binding(),
            expiry: TokenExpiry::NonToken,
        },
        retained,
    );
    let switched = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = switched.clone();
    let scope = first
        .activate_auth(
            factory,
            move || {
                Ok(if reader.load(Ordering::Acquire) {
                    AuthBinding::new(vec![2].into_boxed_slice()).unwrap()
                } else {
                    binding()
                })
            },
            Duration::ZERO,
        )
        .unwrap();
    scope
        .record_test_login(chrono::Utc::now() + chrono::Duration::seconds(3600))
        .unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let signal = gate.clone();
    let cancellation = CancellationToken::new();
    let mut work =
        Box::pin(
            first.run_authenticated(None, &cancellation, scope, move |client, options| {
                Box::pin(async move {
                    gate.acquire().await.unwrap().forget();
                    client.execute("SELECT 1".into(), options).await
                })
            }),
        );
    assert!(poll(work.as_mut()).is_pending());
    switched.store(true, Ordering::Release);
    signal.add_permits(1);
    assert!(matches!(
        poll(work.as_mut()),
        Poll::Ready(Err(PoolError::IdentityMismatch))
    ));
    drop(work);
    assert!(sent.lock().unwrap().is_empty());
    assert!(first.discard);
    assert!(first.client_mut().is_connection_dead());
    first.release().await.unwrap();
    manager.shutdown().await.unwrap();
    pyo3::Python::attach(|py| event_loop.call_method0(py, "close").unwrap());
}

#[tokio::test(start_paused = true)]
async fn post_operation_expiry_discards_without_replacing_completed_results() {
    for mode in 0..4 {
        let (factory, event_loop) = operation_auth_factory().await;
        let retained = super::super::auth::RecoveryAuth::new(binding(), "tenant".into()).unwrap();
        let manager = manager(1);
        let mut first = lease(&manager, tds_client_from_tokens(vec![])).await;
        *first.slot.value_mut().unwrap() = OpenedConnection::with_recovery(
            tds_client_from_tokens(vec![]),
            AuthMetadata {
                binding: binding(),
                expiry: TokenExpiry::NonToken,
            },
            retained,
        );
        let switched = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = switched.clone();
        let scope = first
            .activate_auth(
                factory,
                move || {
                    Ok(if reader.load(Ordering::Acquire) {
                        AuthBinding::new(vec![2].into_boxed_slice()).unwrap()
                    } else {
                        binding()
                    })
                },
                Duration::from_secs(5),
            )
            .unwrap();
        scope
            .record_test_login(chrono::Utc::now() + chrono::Duration::seconds(60))
            .unwrap();
        let result = first
            .run_authenticated(None, &CancellationToken::new(), scope, move |_, _| {
                Box::pin(async move {
                    tokio::time::advance(Duration::from_secs(56)).await;
                    switched.store(mode >= 2, Ordering::Release);
                    if mode == 1 || mode == 3 {
                        return Err(mssql_tds::error::Error::ConnectionError(
                            "operation sentinel".into(),
                        ));
                    }
                    Ok(42)
                })
            })
            .await;
        match mode {
            0 => assert_eq!(result.unwrap(), 42),
            1 | 3 => assert!(matches!(result, Err(PoolError::Execute(error))
                if matches!(error.as_ref(), mssql_tds::error::Error::ConnectionError(message)
                    if message == "operation sentinel"))),
            _ => assert!(matches!(result, Err(PoolError::IdentityMismatch))),
        }
        assert!(first.discard);
        assert!(first.client_mut().is_connection_dead());
        first.release().await.unwrap();
        let mut slot = manager
            .reserve(key(), control(CancellationToken::new()))
            .await
            .unwrap()
            .unwrap();
        assert!(slot.take_value().is_none());
        pyo3::Python::attach(|py| event_loop.call_method0(py, "close").unwrap());
    }
}

#[tokio::test]
async fn missing_ack_and_delayed_errors_discard_candidates_before_one_fresh_open() {
    for tokens in [
        vec![done_no_more()],
        vec![
            env_change_reset_connection(),
            done_more_with_count(0),
            sql_error(50001, 16, "private server message"),
            done_no_more(),
        ],
        vec![env_change_reset_connection(), done_more_with_count(0)],
    ] {
        let manager = manager(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let client =
            tds_client_from_tokens_gated(tokens, Arc::new(Semaphore::new(20)), dropped.clone());
        cache(&manager, opened(client)).await;
        let factory = Factory::new(vec![opened(tds_client_from_tokens(vec![done_no_more()]))]);
        let mut lease = manager
            .acquire(
                key(),
                &spec(),
                &factory,
                isolation(),
                control(CancellationToken::new()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(quiescent(lease.client_mut()));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn known_dead_candidate_is_replaced_without_sending_a_reset() {
    let manager = manager(1);
    let (mut client, sent) = tds_client_from_tokens_capturing(vec![]);
    client.mark_connection_dead();
    cache(&manager, opened(client)).await;
    let factory = Factory::new(vec![opened(tds_client_from_tokens(vec![done_no_more()]))]);
    let mut lease = manager
        .acquire(
            key(),
            &spec(),
            &factory,
            isolation(),
            control(CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(sent.lock().unwrap().is_empty());
    assert!(quiescent(lease.client_mut()));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_pooled_options_fail_before_reserving_or_calling_the_factory() {
    use mssql_tds::connection::client_context::{ColumnEncryptionSetting, TdsAuthenticationMethod};

    let manager = manager(1);
    let factory = Factory::new(vec![]);
    for (field, change) in [
        ("authentication", 1),
        ("authentication", 2),
        ("column_encryption", 3),
    ] {
        let mut context = spec().context().clone();
        match change {
            1 => context.tds_authentication_method = TdsAuthenticationMethod::SSPI,
            2 => context.access_token = Some("test-token".into()),
            _ => context.column_encryption_setting = ColumnEncryptionSetting::Enabled,
        }
        let spec = ConnectSpec::new(context, binding());
        assert!(matches!(
            manager.acquire(key(), &spec, &factory, isolation(), control(CancellationToken::new())).await,
            Err(PoolError::InvalidRequest { field: actual, .. }) if actual == field
        ));
    }
    assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn disabled_at_admission_does_not_open_and_fresh_open_failure_is_not_retried() {
    let manager = manager(1);
    let factory = Factory::new(vec![]);
    assert!(matches!(
        manager
            .acquire(
                key(),
                &spec(),
                &factory,
                isolation(),
                control(CancellationToken::new())
            )
            .await,
        Err(PoolError::Open(OpenError::Provider(_)))
    ));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    manager.disable();
    assert!(
        manager
            .acquire(
                key(),
                &spec(),
                &factory,
                isolation(),
                control(CancellationToken::new()),
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn queue_open_and_isolation_share_the_original_budget() {
    let manager = manager(1);
    let held = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let client = tds_client_from_tokens_gated(
        vec![done_no_more()],
        Arc::new(Semaphore::new(0)),
        dropped.clone(),
    );
    let mut factory = Factory::new(vec![opened(client)]);
    factory.delay = Duration::from_secs(3);
    let spec = spec();
    let control = control(CancellationToken::new());
    let deadline = control.deadline();
    let mut acquire = Box::pin(manager.acquire(key(), &spec, &factory, isolation(), control));
    assert!(poll(acquire.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(6)).await;
    drop(held);
    assert!(poll(acquire.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(poll(acquire.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        poll(acquire.as_mut()),
        Poll::Ready(Err(PoolError::AcquireTimeout {
            stage: AcquireStage::Isolation
        }))
    ));
    assert_eq!(Instant::now(), deadline);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fresh_server_error_is_preserved_and_not_retried() {
    let manager = manager(1);
    let factory = Factory::new(vec![opened(tds_client_from_tokens(vec![
        sql_error(50002, 16, "private server message"),
        done_no_more(),
    ]))]);
    let result = manager
        .acquire(
            key(),
            &spec(),
            &factory,
            isolation(),
            control(CancellationToken::new()),
        )
        .await;
    assert!(matches!(result, Err(PoolError::Initialize(error))
        if matches!(*error, mssql_tds::error::Error::SqlServerError { .. })));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    assert!(
        manager
            .reserve(key(), control(CancellationToken::new()))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn wrong_binding_fails_without_reopening() {
    let manager = manager(1);
    let client = OpenedConnection::new(
        tds_client_from_tokens(vec![]),
        AuthMetadata {
            binding: AuthBinding::new(vec![2].into_boxed_slice()).unwrap(),
            expiry: TokenExpiry::NonToken,
        },
    );
    cache(&manager, client).await;
    let factory = Factory::new(vec![]);
    assert!(matches!(
        manager
            .acquire(
                key(),
                &spec(),
                &factory,
                isolation(),
                control(CancellationToken::new())
            )
            .await,
        Err(PoolError::IdentityMismatch)
    ));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn interruption_destroys_partial_login_before_the_next_capacity_owner() {
    for cancel_explicitly in [false, true] {
        let manager = manager(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let client =
            tds_client_from_tokens_gated(vec![], Arc::new(Semaphore::new(0)), dropped.clone());
        let mut factory = Factory::new(vec![opened(client)]);
        factory.delay = Duration::from_secs(20);
        let token = CancellationToken::new();
        let spec = spec();
        let mut acquire =
            Box::pin(manager.acquire(key(), &spec, &factory, isolation(), control(token.clone())));
        assert!(poll(acquire.as_mut()).is_pending());
        let mut waiting = Box::pin(manager.reserve(key(), control(CancellationToken::new())));
        assert!(poll(waiting.as_mut()).is_pending());
        if cancel_explicitly {
            token.cancel();
        } else {
            tokio::time::advance(Duration::from_secs(10)).await;
        }
        let result = poll(acquire.as_mut());
        assert!(
            matches!(result, Poll::Ready(Err(PoolError::Cancelled)))
                || matches!(
                    result,
                    Poll::Ready(Err(PoolError::AcquireTimeout {
                        stage: AcquireStage::Open
                    }))
                )
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        // A later waiter uses its own budget, not the expired caller's.
        drop(waiting);
        assert!(
            manager
                .reserve(key(), control(CancellationToken::new()))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn reset_and_fresh_drain_interruptions_discard_without_retries() {
    for reuse in [false, true] {
        for interrupt in 0..4 {
            let manager = manager(1);
            let dropped = Arc::new(AtomicUsize::new(0));
            let gate = Arc::new(Semaphore::new(if reuse { 2 } else { 1 }));
            let mut tokens = Vec::new();
            if reuse {
                tokens.push(env_change_reset_connection());
            }

            tokens.extend([done_more_with_count(0), done_no_more()]);
            let client = opened(tds_client_from_tokens_gated(tokens, gate, dropped.clone()));
            let factory = if reuse {
                cache(&manager, client).await;
                Factory::new(vec![])
            } else {
                Factory::new(vec![client])
            };
            let token = CancellationToken::new();
            let spec = spec();
            let mut acquire = Box::pin(manager.acquire(
                key(),
                &spec,
                &factory,
                isolation(),
                control(token.clone()),
            ));
            assert!(poll(acquire.as_mut()).is_pending());
            match interrupt {
                0 => token.cancel(),
                1 => tokio::time::advance(Duration::from_secs(10)).await,
                2 => manager.disable(),
                _ => {}
            }
            if interrupt != 3 {
                let result = poll(acquire.as_mut());
                assert!(matches!(
                    result,
                    Poll::Ready(Err(PoolError::Cancelled
                        | PoolError::Closed
                        | PoolError::AcquireTimeout { .. }))
                ));
            }
            drop(acquire);
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert_eq!(factory.calls.load(Ordering::SeqCst), usize::from(!reuse));
            manager.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn retry_budget_does_not_chase_newly_cached_candidates() {
    let manager = manager(2);
    let gate = Arc::new(Semaphore::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    cache(
        &manager,
        opened(tds_client_from_tokens_gated(
            vec![done_no_more()],
            gate.clone(),
            dropped.clone(),
        )),
    )
    .await;
    let factory = Factory::new(vec![opened(tds_client_from_tokens(vec![done_no_more()]))]);
    let spec = spec();
    let mut acquire = Box::pin(manager.acquire(
        key(),
        &spec,
        &factory,
        isolation(),
        control(CancellationToken::new()),
    ));
    assert!(poll(acquire.as_mut()).is_pending());
    let (later, sent) =
        tds_client_from_tokens_capturing(vec![env_change_reset_connection(), done_no_more()]);
    cache(&manager, opened(later)).await;
    gate.add_permits(1);
    let lease = acquire.await.unwrap().unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    assert!(
        sent.lock().unwrap().is_empty(),
        "new arrivals cannot extend retry work"
    );
    drop(lease);
}

#[tokio::test]
async fn surrendered_candidate_retry_queues_behind_older_waiters() {
    let manager = manager(2);
    let mut first = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    let mut second = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    let gate = Arc::new(Semaphore::new(0));
    first
        .install(opened(tds_client_from_tokens_gated(
            vec![done_no_more()],
            gate.clone(),
            Arc::new(AtomicUsize::new(0)),
        )))
        .unwrap();
    second
        .install(opened(tds_client_from_tokens(vec![])))
        .unwrap();
    first.return_quiescent().unwrap();
    second.return_quiescent().unwrap();
    let factory = Factory::new(vec![opened(tds_client_from_tokens(vec![done_no_more()]))]);
    let spec = spec();
    let mut acquire = Box::pin(manager.acquire(
        key(),
        &spec,
        &factory,
        isolation(),
        control(CancellationToken::new()),
    ));
    assert!(poll(acquire.as_mut()).is_pending());
    let held = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    let mut older = Box::pin(manager.reserve(key(), control(CancellationToken::new())));
    assert!(poll(older.as_mut()).is_pending());
    gate.add_permits(1);
    assert!(poll(acquire.as_mut()).is_pending());
    let assigned = match poll(older.as_mut()) {
        Poll::Ready(Ok(Some(slot))) => slot,
        _ => panic!("older waiter must receive surrendered capacity"),
    };
    assert!(poll(acquire.as_mut()).is_pending());
    assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
    drop(assigned);
    assert!(acquire.await.unwrap().is_some());
    drop(held);
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn preparation_reset_retry_wait_open_and_isolation_share_the_original_deadline() {
    let manager = manager(2);
    let mut first = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    let mut second = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let reset_gate = Arc::new(Semaphore::new(0));
    first
        .install(opened(tds_client_from_tokens_gated(
            vec![done_no_more()],
            reset_gate.clone(),
            dropped.clone(),
        )))
        .unwrap();
    second
        .install(opened(tds_client_from_tokens(vec![])))
        .unwrap();
    first.return_quiescent().unwrap();
    second.return_quiescent().unwrap();
    let mut factory = Factory::new(vec![opened(tds_client_from_tokens_gated(
        vec![done_no_more()],
        Arc::new(Semaphore::new(0)),
        dropped.clone(),
    ))]);
    factory.delay = Duration::from_secs(3);
    let spec = spec();
    let acquire_control = control(CancellationToken::new());
    let deadline = acquire_control.deadline();
    tokio::time::advance(Duration::from_secs(1)).await;
    let mut acquire =
        Box::pin(manager.acquire(key(), &spec, &factory, isolation(), acquire_control));
    assert!(poll(acquire.as_mut()).is_pending());
    let held = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    let mut older = Box::pin(manager.reserve(key(), control(CancellationToken::new())));
    assert!(poll(older.as_mut()).is_pending());

    tokio::time::advance(Duration::from_secs(3)).await;
    reset_gate.add_permits(1);
    assert!(poll(acquire.as_mut()).is_pending());
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let assigned = match poll(older.as_mut()) {
        Poll::Ready(Ok(Some(slot))) => slot,
        _ => panic!("older waiter must receive surrendered capacity"),
    };
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(poll(acquire.as_mut()).is_pending());
    assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
    drop(assigned);
    assert!(poll(acquire.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(poll(acquire.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        poll(acquire.as_mut()),
        Poll::Ready(Err(PoolError::AcquireTimeout {
            stage: AcquireStage::Isolation
        }))
    ));
    assert_eq!(Instant::now(), deadline);
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    drop(held);
    manager.shutdown().await.unwrap();
}

async fn lease(manager: &AsyncPoolManager, client: TdsClient) -> PoolLease {
    let mut slot = manager
        .reserve(key(), control(CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    slot.install(opened(client)).unwrap();
    slot.publish().unwrap();
    PoolLease {
        slot,
        discard: false,
    }
}

#[tokio::test]
async fn release_drains_all_results_before_discovering_and_rolling_back_raw_transaction() {
    let manager = manager(1);
    let (client, sent) = tds_client_from_tokens_capturing(vec![
        col_metadata_empty(),
        env_change_begin_transaction(42),
        done_more_with_count(0),
        col_metadata_empty(),
        done_no_more(),
        env_change_rollback_transaction(),
        done_no_more(),
        env_change_reset_connection(),
        done_no_more(),
    ]);
    let id = client.client_connection_id();
    let mut first = lease(&manager, client).await;
    first
        .client_mut()
        .execute("BEGIN TRANSACTION; SELECT 1; SELECT 2".into(), ())
        .await
        .unwrap();
    assert!(first.client_mut().has_open_batch());
    assert!(!first.client_mut().has_active_transaction());
    sent.lock().unwrap().clear();
    first.release().await.unwrap();
    assert_eq!(sent.lock().unwrap()[0], 14, "transaction manager request");
    let factory = Factory::new(vec![]);
    let mut reused = manager
        .acquire(
            key(),
            &spec(),
            &factory,
            isolation(),
            control(CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reused.client_mut().client_connection_id(), id);
    assert!(quiescent(reused.client_mut()));
    assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn release_rejects_unfinished_streamed_write_before_capacity_handoff() {
    for in_transaction in [false, true] {
        let manager = manager(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let tokens = if in_transaction {
            vec![env_change_begin_transaction(42), done_no_more()]
        } else {
            vec![]
        };
        let client =
            tds_client_from_tokens_gated(tokens, Arc::new(Semaphore::new(2)), dropped.clone());
        let mut first = lease(&manager, client).await;
        if in_transaction {
            first
                .client_mut()
                .execute("BEGIN TRANSACTION".into(), ())
                .await
                .unwrap();
        }
        first
            .client_mut()
            .begin_sp_executesql(
                "SELECT @v".into(),
                vec![RpcParameter::data_at_exec(
                    Some("@v".into()),
                    StatusFlags::NONE,
                    StreamedSqlType::VarBinaryMax,
                )],
                (),
            )
            .await
            .unwrap();
        first
            .client_mut()
            .write_streamed_chunk(&[1; 8192])
            .await
            .unwrap();
        assert!(!first.client_mut().has_open_batch());
        assert!(first.client_mut().command_is_busy());
        let mut waiting = Box::pin(manager.reserve(key(), control(CancellationToken::new())));
        assert!(poll(waiting.as_mut()).is_pending());

        assert!(
            matches!(first.release().await, Err(PoolError::Cleanup(ref cause))
            if matches!(cause.as_ref(), mssql_tds::error::Error::ProtocolError(_)))
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let slot = waiting.await.unwrap().unwrap();
        drop(slot);
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn release_failures_preserve_errors_and_destroy_before_capacity_handoff() {
    for (tokens, sql_error_expected) in [
        (
            vec![
                col_metadata_empty(),
                sql_error(50003, 16, "private drain message"),
                done_no_more(),
            ],
            true,
        ),
        (vec![col_metadata_empty()], false),
        (
            vec![
                col_metadata_empty(),
                env_change_begin_transaction(42),
                done_no_more(),
                sql_error(50004, 16, "private rollback message"),
                done_no_more(),
            ],
            true,
        ),
        (
            vec![
                col_metadata_empty(),
                env_change_begin_transaction(42),
                done_no_more(),
                done_no_more(),
            ],
            false,
        ),
    ] {
        let manager = manager(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let client =
            tds_client_from_tokens_gated(tokens, Arc::new(Semaphore::new(20)), dropped.clone());
        let mut first = lease(&manager, client).await;
        first
            .client_mut()
            .execute("SELECT 1".into(), ())
            .await
            .unwrap();
        let mut waiting = Box::pin(manager.reserve(key(), control(CancellationToken::new())));
        assert!(poll(waiting.as_mut()).is_pending());
        let error = first.release().await.unwrap_err();
        if sql_error_expected {
            assert!(matches!(error, PoolError::Cleanup(ref cause)
                if matches!(cause.as_ref(), mssql_tds::error::Error::SqlServerError { .. })));
        } else {
            assert!(matches!(error, PoolError::Cleanup(_)));
        }
        assert!(!format!("{error:?}").contains("private"));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let slot = waiting.await.unwrap().unwrap();
        drop(slot);
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn interrupted_release_discards_during_drain_and_rollback_before_waking_waiters() {
    for during_rollback in [false, true] {
        for timeout in [false, true] {
            let manager = manager(1);
            let dropped = Arc::new(AtomicUsize::new(0));
            let gate = Arc::new(Semaphore::new(if during_rollback { 3 } else { 1 }));
            let client = tds_client_from_tokens_gated(
                vec![
                    col_metadata_empty(),
                    env_change_begin_transaction(42),
                    done_no_more(),
                    env_change_rollback_transaction(),
                    done_no_more(),
                ],
                gate,
                dropped.clone(),
            );
            let mut first = lease(&manager, client).await;
            first
                .client_mut()
                .execute("BEGIN TRANSACTION; SELECT 1".into(), ())
                .await
                .unwrap();
            let mut release = Box::pin(first.release());
            assert!(poll(release.as_mut()).is_pending());
            let mut waiting = Box::pin(
                manager.reserve(
                    key(),
                    AcquireControl::from_timeout(
                        Instant::now(),
                        Duration::from_secs(30),
                        CancellationToken::new(),
                    )
                    .unwrap(),
                ),
            );
            assert!(poll(waiting.as_mut()).is_pending());
            assert_eq!(dropped.load(Ordering::SeqCst), 0);
            if timeout {
                tokio::time::advance(Duration::from_secs(10)).await;
                assert!(matches!(
                    poll(release.as_mut()),
                    Poll::Ready(Err(PoolError::CleanupTimeout))
                ));
            }
            drop(release);
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            drop(waiting.await.unwrap().unwrap());
            manager.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn release_shares_one_budget_and_caps_a_callers_deadline() {
    for caller_budget in [Duration::from_secs(8), Duration::from_secs(20)] {
        let manager = manager(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(1));
        let client = tds_client_from_tokens_gated(
            vec![
                col_metadata_empty(),
                env_change_begin_transaction(42),
                done_no_more(),
                env_change_rollback_transaction(),
                done_no_more(),
            ],
            gate.clone(),
            dropped.clone(),
        );
        let mut first = lease(&manager, client).await;
        first
            .client_mut()
            .execute("SELECT 1".into(), ())
            .await
            .unwrap();
        let start = Instant::now();
        let mut release = Box::pin(first.release_with_deadline(start + caller_budget));
        assert!(poll(release.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(6)).await;
        gate.add_permits(2);
        assert!(poll(release.as_mut()).is_pending());
        let total = caller_budget.min(Duration::from_secs(10));
        tokio::time::advance(total - Duration::from_secs(6)).await;
        assert!(matches!(
            poll(release.as_mut()),
            Poll::Ready(Err(PoolError::CleanupTimeout))
        ));
        assert_eq!(Instant::now() - start, total);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn discard_expired_deadline_and_unpolled_release_never_clean_or_cache() {
    for mode in 0..4 {
        let manager = manager(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let client =
            tds_client_from_tokens_gated(vec![], Arc::new(Semaphore::new(0)), dropped.clone());
        let mut first = lease(&manager, client).await;
        match mode {
            0 => {
                first.mark_discard();
                first.release().await.unwrap();
            }
            1 => assert!(matches!(
                first.release_with_deadline(Instant::now()).await,
                Err(PoolError::CleanupTimeout)
            )),
            2 => drop(first.release()),
            _ => drop(first),
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let mut slot = manager
            .reserve(key(), control(CancellationToken::new()))
            .await
            .unwrap()
            .unwrap();
        assert!(slot.take_value().is_none());
    }
}

#[tokio::test]
async fn release_across_disable_reenable_discards_old_generation() {
    let manager = manager(1);
    let dropped = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Semaphore::new(1));
    let client = tds_client_from_tokens_gated(
        vec![col_metadata_empty(), done_no_more()],
        gate.clone(),
        dropped.clone(),
    );
    let mut first = lease(&manager, client).await;
    first
        .client_mut()
        .execute("SELECT 1".into(), ())
        .await
        .unwrap();
    let mut release = Box::pin(first.release());
    assert!(poll(release.as_mut()).is_pending());
    manager.disable();
    let budget = Duration::from_secs(10);
    manager
        .enable(
            PoolConfig::new(budget, budget, budget)
                .unwrap()
                .with_max_size(1)
                .unwrap(),
        )
        .unwrap();
    let mut waiting = Box::pin(manager.reserve(key(), control(CancellationToken::new())));
    assert!(poll(waiting.as_mut()).is_pending());
    gate.add_permits(1);
    release.await.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let mut slot = waiting.await.unwrap().unwrap();
    assert!(slot.take_value().is_none());
}

#[tokio::test]
async fn cancelling_completed_release_cannot_reclaim_the_next_owners_client() {
    let manager = manager(1);
    let dropped = Arc::new(AtomicUsize::new(0));
    let client = tds_client_from_tokens_gated(vec![], Arc::new(Semaphore::new(0)), dropped.clone());
    let first = lease(&manager, client).await;
    let mut waiting = Box::pin(manager.reserve(key(), control(CancellationToken::new())));
    assert!(poll(waiting.as_mut()).is_pending());
    let mut release = Box::pin(first.release());
    assert!(matches!(poll(release.as_mut()), Poll::Ready(Ok(()))));
    drop(release);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    let mut assigned = waiting.await.unwrap().unwrap();
    assert!(assigned.value_mut().is_some());
    drop(assigned);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn release_rejects_foreign_runtime_before_start_and_before_resuming_io() {
    for start_on_owner in [false, true] {
        let manager = manager(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(1));
        let client = tds_client_from_tokens_gated(
            vec![col_metadata_empty(), done_no_more()],
            gate.clone(),
            dropped.clone(),
        );
        let mut first = lease(&manager, client).await;
        first
            .client_mut()
            .execute("SELECT 1".into(), ())
            .await
            .unwrap();
        let mut release = Box::pin(first.release());
        if start_on_owner {
            assert!(poll(release.as_mut()).is_pending());
        }
        gate.add_permits(1);
        let (release, outcome) = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let outcome = poll(release.as_mut());
                (release, outcome)
            })
        })
        .join()
        .unwrap();
        assert!(matches!(
            outcome,
            Poll::Ready(Err(PoolError::InvalidRequest {
                field: "runtime",
                ..
            }))
        ));
        assert_eq!(gate.available_permits(), 1, "no foreign-runtime I/O");
        drop(release);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        manager.shutdown().await.unwrap();
    }
}
