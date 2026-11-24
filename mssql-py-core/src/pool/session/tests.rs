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
fn isolation_sql_is_closed_and_no_change_is_rejected() {
    for (level, suffix) in [
        (
            TransactionIsolationLevel::ReadUncommitted,
            "READ UNCOMMITTED",
        ),
        (TransactionIsolationLevel::ReadCommitted, "READ COMMITTED"),
        (TransactionIsolationLevel::RepeatableRead, "REPEATABLE READ"),
        (TransactionIsolationLevel::Serializable, "SERIALIZABLE"),
        (TransactionIsolationLevel::Snapshot, "SNAPSHOT"),
    ] {
        assert_eq!(
            Isolation::try_from(level).unwrap().0,
            format!("SET TRANSACTION ISOLATION LEVEL {suffix}")
        );
    }
    assert!(matches!(
        Isolation::try_from(TransactionIsolationLevel::NoChange),
        Err(PoolError::InvalidRequest {
            field: "isolation_level",
            ..
        })
    ));
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
        cache(&manager, opened(tds_client_from_tokens(tokens))).await;
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
        assert!(quiescent(lease.client_mut()));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn invalid_pooled_options_fail_before_reserving_or_calling_the_factory() {
    use mssql_tds::connection::client_context::{ColumnEncryptionSetting, TdsAuthenticationMethod};

    let manager = manager(1);
    let factory = Factory::new(vec![]);
    for (field, change) in [
        ("connect_retry_count", 0),
        ("authentication", 1),
        ("authentication", 2),
        ("column_encryption", 3),
    ] {
        let mut context = spec().context().clone();
        match change {
            0 => context.connect_retry_count = 1,
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
