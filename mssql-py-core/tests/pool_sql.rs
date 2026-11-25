// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// The cdylib's pool remains private; compile its contracts into this native harness.
#[allow(dead_code)]
#[path = "../src/pool"]
mod pool {
    pub(crate) mod auth;
    pub(crate) mod config;
    pub(crate) mod connection;
    pub(crate) mod control;
    pub(crate) mod error;
    mod lifecycle;
    pub(crate) mod manager;
    pub(crate) mod session;
}
#[allow(dead_code)]
#[path = "../src/async_entra_credentials.rs"]
mod async_entra_credentials;
#[allow(dead_code)]
#[path = "../src/async_entra_token_factory.rs"]
mod async_entra_token_factory;
#[allow(dead_code)]
#[path = "../src/async_errors.rs"]
mod async_errors;
#[allow(dead_code)]
#[path = "../src/async_handle.rs"]
mod async_handle;
#[allow(dead_code)]
#[path = "../src/async_session.rs"]
mod async_session;
#[allow(dead_code)]
#[path = "../../mssql-tds/tests/common/mod.rs"]
mod common;

use std::time::Duration;

use async_trait::async_trait;
use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::core::CancelHandle;
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;
use mssql_tds::test_client_support::{ResetAckInjection, inject_missing_reset_ack};
use pool::config::PoolConfig;
use pool::connection::{
    AuthBinding, ConnectSpec, ConnectionFactory, OpenedConnection, PoolKey, SqlConnectionFactory,
};
use pool::control::AcquireControl;
use pool::error::{OpenError, PoolError};
use pool::manager::AsyncPoolManager;
use pool::session::{Isolation, PoolLease};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn spec() -> ConnectSpec {
    let mut context = common::create_context();
    context.data_source = common::build_tcp_datasource();
    context.connect_retry_count = 0;
    ConnectSpec::new(
        context,
        AuthBinding::new(vec![1].into_boxed_slice()).unwrap(),
    )
}

fn manager() -> AsyncPoolManager {
    let budget = Duration::from_secs(30);
    let config = PoolConfig::new(budget, budget, budget)
        .unwrap()
        .with_max_size(1)
        .unwrap();
    let manager = AsyncPoolManager::new(config.clone()).unwrap();
    manager.enable(config).unwrap();
    manager
}

async fn acquire(
    manager: &AsyncPoolManager,
    spec: &ConnectSpec,
    isolation: Isolation,
) -> PoolLease {
    manager
        .acquire(
            PoolKey::new(vec![1].into_boxed_slice()).unwrap(),
            spec,
            &SqlConnectionFactory,
            isolation,
            AcquireControl::from_timeout(
                Instant::now(),
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap()
}

async fn scalar(client: &mut TdsClient, sql: &str) -> i32 {
    client.execute(sql.into(), ()).await.unwrap();
    match common::get_scalar_value(client).await.unwrap() {
        Some(ColumnValues::Int(value)) => value,
        Some(ColumnValues::SmallInt(value)) => i32::from(value),
        other => panic!("expected int scalar, got {other:?}"),
    }
}

async fn exec(client: &mut TdsClient, sql: &str) {
    client.execute(sql.into(), ()).await.unwrap();
    client.close_query().await.unwrap();
}

#[tokio::test]
async fn mock_recovery_keeps_the_lease_and_sends_the_application_batch_once() {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use mssql_mock_tds::{MockTdsServer, QueryResponse};
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::core::{EncryptionOptions, EncryptionSetting};

    let server = MockTdsServer::new(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).to_string())
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
    let mut context =
        ClientContext::with_data_source(&format!("tcp:{},{}", address.ip(), address.port()));
    context.user_name = "test-user".into();
    context.password = address.to_string();
    context.connect_retry_count = 1;
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        ..Default::default()
    };
    let spec = ConnectSpec::new(
        context,
        AuthBinding::new(vec![1].into_boxed_slice()).unwrap(),
    );
    let manager = manager();
    let mut first = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    assert!(first.client_mut().is_session_recovery_enabled());
    let old_id = first.client_mut().client_connection_id();
    first.client_mut().mark_connection_dead();
    let mut waiting = Box::pin(acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    ));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(waiting.as_mut(), &mut cx).is_pending());
    first
        .run(
            Some(Instant::now() + Duration::from_secs(10)),
            &CancellationToken::new(),
            |client, cancel| {
                Box::pin(async move {
                    client
                        .execute(
                            "SELECT 1".into(),
                            mssql_tds::connection::tds_client::ExecuteOptions::new().cancel(cancel),
                        )
                        .await?;
                    client.close_query().await
                })
            },
        )
        .await
        .unwrap();
    let recovered_id = first.client_mut().client_connection_id();
    assert_ne!(old_id, recovered_id);
    assert_eq!(first.client_mut().connection_recovery_count(), 1);
    assert!(std::future::Future::poll(waiting.as_mut(), &mut cx).is_pending());
    {
        let store = connections.lock().await;
        assert_eq!(store.count(), 2);
        assert_eq!(
            store
                .all()
                .first_key_value()
                .unwrap()
                .1
                .transaction_descriptor_headers
                .len(),
            1,
        );
        let (&id, info) = store.all().last_key_value().unwrap();
        assert!(store.login_details(id).unwrap().recovery_login);
        assert_eq!(info.transaction_descriptor_headers.len(), 1);
    }
    drop(waiting);
    first.release().await.unwrap();
    let mut next = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    // The mock omits RESETCONNECTION ACK: checkout must replace, not recover.
    assert_ne!(next.client_mut().client_connection_id(), recovered_id);
    {
        let store = connections.lock().await;
        assert_eq!(store.count(), 3);
        assert!(
            !store
                .login_details(*store.all().last_key_value().unwrap().0)
                .unwrap()
                .recovery_login
        );
    }
    drop(next);
    drop(manager);
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_recovery_retains_capacity_and_session_settings_then_allows_reuse() {
    let manager = manager();
    let mut context = spec().context().clone();
    context.connect_retry_count = 1;
    let spec = ConnectSpec::new(
        context,
        AuthBinding::new(vec![1].into_boxed_slice()).unwrap(),
    );
    let mut first = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::Serializable.try_into().unwrap(),
    )
    .await;
    assert!(first.client_mut().is_session_recovery_enabled());
    let old_id = first.client_mut().client_connection_id();
    exec(first.client_mut(), "SET LOCK_TIMEOUT 1234").await;
    first.client_mut().mark_connection_dead();
    let mut waiting = Box::pin(acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::Serializable.try_into().unwrap(),
    ));
    assert!(
        std::future::Future::poll(
            waiting.as_mut(),
            &mut std::task::Context::from_waker(std::task::Waker::noop()),
        )
        .is_pending()
    );
    first
        .run(
            Some(Instant::now() + Duration::from_secs(30)),
            &CancellationToken::new(),
            |client, cancel| {
                Box::pin(async move {
                    client
                        .execute(
                            "SELECT 1".into(),
                            mssql_tds::connection::tds_client::ExecuteOptions::new().cancel(cancel),
                        )
                        .await?;
                    client.close_query().await
                })
            },
        )
        .await
        .unwrap();
    assert_ne!(first.client_mut().client_connection_id(), old_id);
    assert_eq!(first.client_mut().connection_recovery_count(), 1);
    assert!(
        std::future::Future::poll(
            waiting.as_mut(),
            &mut std::task::Context::from_waker(std::task::Waker::noop()),
        )
        .is_pending()
    );
    drop(waiting);
    assert_eq!(
        scalar(first.client_mut(), "SELECT @@LOCK_TIMEOUT").await,
        1234
    );
    assert_eq!(scalar(first.client_mut(),
        "SELECT CAST(transaction_isolation_level AS int) FROM sys.dm_exec_sessions WHERE session_id = @@SPID",
    ).await, 4);
    let recovered_id = first.client_mut().client_connection_id();
    first.release().await.unwrap();
    let mut reused = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    assert_eq!(reused.client_mut().client_connection_id(), recovered_id);
    reused.release().await.unwrap();
    manager.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_fresh_and_reused_sessions_apply_every_isolation_level() {
    let spec = spec();
    for (level, expected) in [
        (TransactionIsolationLevel::ReadUncommitted, 1),
        (TransactionIsolationLevel::ReadCommitted, 2),
        (TransactionIsolationLevel::RepeatableRead, 3),
        (TransactionIsolationLevel::Serializable, 4),
        (TransactionIsolationLevel::Snapshot, 5),
    ] {
        let manager = manager();
        let mut fresh = acquire(&manager, &spec, level.clone().try_into().unwrap()).await;
        let spid = scalar(fresh.client_mut(), "SELECT @@SPID").await;
        let query = "SELECT CAST(transaction_isolation_level AS int) \
                     FROM sys.dm_exec_sessions WHERE session_id = @@SPID";
        assert_eq!(scalar(fresh.client_mut(), query).await, expected);
        exec(
            fresh.client_mut(),
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        )
        .await;
        fresh.release().await.unwrap();
        let mut reused = acquire(&manager, &spec, level.try_into().unwrap()).await;
        assert_eq!(scalar(reused.client_mut(), "SELECT @@SPID").await, spid);
        assert_eq!(scalar(reused.client_mut(), query).await, expected);
        assert!(!reused.client_mut().reset_pending());
        assert!(!reused.client_mut().has_open_batch());
        drop(reused);
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_checkout_resets_session_state_and_discards_dirty_candidates() {
    let manager = manager();
    let spec = spec();
    let mut first = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    let spid = scalar(first.client_mut(), "SELECT @@SPID").await;
    let lock_timeout = scalar(first.client_mut(), "SELECT @@LOCK_TIMEOUT").await;
    exec(
        first.client_mut(),
        "SET LOCK_TIMEOUT 1234; CREATE TABLE #pool_probe (n int)",
    )
    .await;
    first.release().await.unwrap();
    let mut reused = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::Serializable.try_into().unwrap(),
    )
    .await;
    assert_eq!(scalar(reused.client_mut(), "SELECT @@SPID").await, spid);
    assert_eq!(
        scalar(reused.client_mut(), "SELECT @@LOCK_TIMEOUT").await,
        lock_timeout
    );
    assert_eq!(
        scalar(
            reused.client_mut(),
            "SELECT CASE WHEN OBJECT_ID('tempdb..#pool_probe') IS NULL THEN 0 ELSE 1 END"
        )
        .await,
        0
    );
    exec(reused.client_mut(), "BEGIN TRANSACTION").await;
    assert!(reused.client_mut().has_active_transaction());
    let discarded_id = reused.client_mut().client_connection_id();
    // Fault injection, not a production return path: checkout must reject this client.
    reused.return_quiescent().unwrap();
    let mut replacement = acquire(
        &manager,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    assert_ne!(
        replacement.client_mut().client_connection_id(),
        discarded_id
    );
    assert_eq!(
        scalar(replacement.client_mut(), "SELECT @@TRANCOUNT").await,
        0
    );
    assert_eq!(
        scalar(
            replacement.client_mut(),
            "SELECT CAST(transaction_isolation_level AS int) \
         FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
        )
        .await,
        2
    );
    drop(replacement);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_release_drains_and_rolls_back_before_same_spid_checkout() {
    let owner = manager();
    let observer_pool = manager();
    let spec = spec();
    let mut first = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    let spid = scalar(first.client_mut(), "SELECT @@SPID").await;
    let resource = format!("pool_return_{}", uuid::Uuid::new_v4().simple());
    first
        .client_mut()
        .execute(
            format!(
                "BEGIN TRANSACTION; \
                 EXEC sys.sp_getapplock @Resource='{resource}', @LockMode='Exclusive', \
                 @LockOwner='Transaction', @LockTimeout=0; SELECT 1; SELECT 2"
            ),
            (),
        )
        .await
        .unwrap();
    assert!(first.client_mut().has_open_batch());
    assert!(first.client_mut().has_active_transaction());
    let mut observer = acquire(
        &observer_pool,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    let try_lock = format!(
        "DECLARE @result int; \
         EXEC @result = sys.sp_getapplock @Resource='{resource}', @LockMode='Exclusive', \
         @LockOwner='Session', @LockTimeout=0; SELECT @result"
    );
    assert_eq!(scalar(observer.client_mut(), &try_lock).await, -1);
    first.release().await.unwrap();
    // Observe rollback before any checkout RESETCONNECTION can mask a dirty return.
    assert!(scalar(observer.client_mut(), &try_lock).await >= 0);
    exec(
        observer.client_mut(),
        &format!("EXEC sys.sp_releaseapplock @Resource='{resource}', @LockOwner='Session'"),
    )
    .await;
    let mut reused = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::Serializable.try_into().unwrap(),
    )
    .await;
    assert_eq!(scalar(reused.client_mut(), "SELECT @@SPID").await, spid);
    assert_eq!(scalar(reused.client_mut(), "SELECT @@TRANCOUNT").await, 0);
    assert_eq!(
        scalar(
            reused.client_mut(),
            "SELECT CAST(transaction_isolation_level AS int) \
             FROM sys.dm_exec_sessions WHERE session_id = @@SPID",
        )
        .await,
        4
    );
    reused.release().await.unwrap();
    observer.release().await.unwrap();
    owner.shutdown().await.unwrap();
    observer_pool.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_release_delayed_error_discards_instead_of_caching() {
    let owner = manager();
    let spec = spec();
    let mut first = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    let id = first.client_mut().client_connection_id();
    first
        .client_mut()
        .execute(
            "SELECT 1; RAISERROR('safe return delayed error', 16, 1)".into(),
            (),
        )
        .await
        .unwrap();
    assert!(matches!(first.release().await, Err(PoolError::Cleanup(_))));
    let mut replacement = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    assert_ne!(replacement.client_mut().client_connection_id(), id);
    replacement.release().await.unwrap();
    owner.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live SQL Server, DB_HOST/DB_USERNAME/SQL_PASSWORD and permission to KILL/view sessions"]
async fn live_dead_idle_socket_is_replaced_and_replacement_is_reusable() {
    let owner = manager();
    let observer_pool = manager();
    let spec = spec();
    let mut first = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    let spid = scalar(first.client_mut(), "SELECT @@SPID").await;
    let rejected_id = first.client_mut().client_connection_id();
    let marker = uuid::Uuid::new_v4().simple().to_string();
    exec(first.client_mut(), &format!("SET CONTEXT_INFO 0x{marker}")).await;
    first.release().await.unwrap();

    let mut observer = acquire(
        &observer_pool,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    // Guard against killing an unrelated session if SQL Server recycles the SPID.
    exec(
        observer.client_mut(),
        &format!(
            "IF EXISTS (SELECT 1 FROM sys.dm_exec_sessions \
             WHERE session_id = {spid} AND context_info = 0x{marker}) \
             KILL {spid}; \
             ELSE THROW 50005, 'pool socket test session is no longer present', 1"
        ),
    )
    .await;

    let mut replacement = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::Serializable.try_into().unwrap(),
    )
    .await;
    let replacement_id = replacement.client_mut().client_connection_id();
    assert_ne!(replacement_id, rejected_id);
    assert!(!replacement.client_mut().reset_pending());
    assert!(!replacement.client_mut().has_open_batch());
    assert_eq!(
        scalar(replacement.client_mut(), "SELECT @@TRANCOUNT").await,
        0
    );
    assert_eq!(
        scalar(
            replacement.client_mut(),
            "SELECT CAST(transaction_isolation_level AS int) \
             FROM sys.dm_exec_sessions WHERE session_id = @@SPID",
        )
        .await,
        4
    );
    replacement.release().await.unwrap();
    let mut reused = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    assert_eq!(reused.client_mut().client_connection_id(), replacement_id);
    assert_eq!(scalar(reused.client_mut(), "SELECT 1").await, 1);
    reused.release().await.unwrap();
    observer.release().await.unwrap();
    owner.shutdown().await.unwrap();
    observer_pool.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_injected_missing_reset_ack_reports_exact_error_and_retires_client() {
    let owner = manager();
    let spec = spec();
    let mut first = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    let injection = inject_missing_reset_ack(first.client_mut()).unwrap();
    assert_eq!(scalar(first.client_mut(), "SELECT 37").await, 37);
    assert_eq!(injection.suppressed(), 0);
    first.client_mut().prepare_reset_connection(false);
    let error = first
        .client_mut()
        .execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE".into(), ())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        mssql_tds::error::Error::ConnectionResetNotAcknowledged
    ));
    assert_eq!(injection.suppressed(), 1);
    assert!(first.client_mut().is_connection_dead());
    assert_eq!(injection.clients_dropped(), 0);
    drop(first);
    assert_eq!(injection.clients_dropped(), 1);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live SQL Server and DB_HOST/DB_USERNAME/SQL_PASSWORD"]
async fn live_injected_missing_reset_ack_discards_before_one_replacement_open() {
    struct ReplacementFactory<'a> {
        injection: &'a ResetAckInjection,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ConnectionFactory for ReplacementFactory<'_> {
        async fn open(
            &self,
            spec: &ConnectSpec,
            cancel: &CancelHandle,
        ) -> Result<OpenedConnection, OpenError> {
            assert_eq!(self.injection.suppressed(), 1);
            assert_eq!(self.injection.clients_dropped(), 1);
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            SqlConnectionFactory.open(spec, cancel).await
        }
    }

    let owner = manager();
    let spec = spec();
    let mut first = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    let spid = scalar(first.client_mut(), "SELECT @@SPID").await;
    first.release().await.unwrap();
    let mut healthy = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    assert_eq!(scalar(healthy.client_mut(), "SELECT @@SPID").await, spid);
    let rejected_id = healthy.client_mut().client_connection_id();
    let injection = inject_missing_reset_ack(healthy.client_mut()).unwrap();
    healthy.release().await.unwrap();
    assert_eq!(injection.suppressed(), 0);
    assert_eq!(injection.clients_dropped(), 0);
    let factory = ReplacementFactory {
        injection: &injection,
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let mut replacement = owner
        .acquire(
            PoolKey::new(vec![1].into_boxed_slice()).unwrap(),
            &spec,
            &factory,
            TransactionIsolationLevel::Serializable.try_into().unwrap(),
            AcquireControl::from_timeout(
                Instant::now(),
                Duration::from_secs(30),
                CancellationToken::new(),
            )
            .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(factory.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let replacement_id = replacement.client_mut().client_connection_id();
    assert_ne!(replacement_id, rejected_id);
    assert!(!replacement.client_mut().reset_pending());
    assert!(!replacement.client_mut().has_open_batch());
    assert_eq!(
        scalar(
            replacement.client_mut(),
            "SELECT CAST(transaction_isolation_level AS int) \
             FROM sys.dm_exec_sessions WHERE session_id = @@SPID",
        )
        .await,
        4
    );
    replacement.release().await.unwrap();
    let mut reused = acquire(
        &owner,
        &spec,
        TransactionIsolationLevel::ReadCommitted.try_into().unwrap(),
    )
    .await;
    assert_eq!(reused.client_mut().client_connection_id(), replacement_id);
    assert_eq!(scalar(reused.client_mut(), "SELECT @@TRANCOUNT").await, 0);
    assert_eq!(injection.suppressed(), 1);
    assert_eq!(injection.clients_dropped(), 1);
    reused.release().await.unwrap();
    owner.shutdown().await.unwrap();
}
