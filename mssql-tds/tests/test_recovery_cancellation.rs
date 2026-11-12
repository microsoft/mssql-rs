// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mssql_mock_tds::{MockTdsServer, QueryResponse};
use mssql_tds::connection::client_context::{
    ClientContext, EntraIdTokenFactory, TdsAuthenticationMethod,
};
use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{CancelHandle, EncryptionSetting, TdsResult};
use mssql_tds::error::Error;
use tokio::sync::{Notify, oneshot};

#[derive(Clone, Copy)]
enum RecoveryFailure {
    ProviderCancellation,
    FactoryCancellation,
    CommandCancellation,
    ConnectionError,
}

#[derive(Clone)]
struct FailOnceCredential {
    calls: Arc<AtomicUsize>,
    failure: RecoveryFailure,
    recovery_started: Arc<Notify>,
}

#[async_trait]
impl EntraIdTokenFactory for FailOnceCredential {
    async fn create_token(
        &self,
        _spn: String,
        _sts_url: String,
        _auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            return Err(match self.failure {
                RecoveryFailure::ProviderCancellation => Error::TokenAcquisitionCancelled,
                RecoveryFailure::FactoryCancellation => {
                    Error::OperationCancelledError("credential cancelled".into())
                }
                RecoveryFailure::CommandCancellation => {
                    self.recovery_started.notify_one();
                    return std::future::pending().await;
                }
                RecoveryFailure::ConnectionError => {
                    Error::ConnectionError("retryable recovery failure".into())
                }
            });
        }
        Ok("recovery-token"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect())
    }
}

#[tokio::test]
async fn recovery_stops_on_cancellation_but_retries_connection_errors() {
    for failure in [
        RecoveryFailure::ProviderCancellation,
        RecoveryFailure::FactoryCancellation,
        RecoveryFailure::CommandCancellation,
        RecoveryFailure::ConnectionError,
    ] {
        for retry_count in [1, 3] {
            let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
            let addr = server.local_addr();
            let registry = server.query_registry();
            let connections = server.connection_store();
            let closing_query = "SELECT 1 AS close_session";
            {
                let mut registry = registry.lock().await;
                registry.set_session_recovery(true);
                registry.register(closing_query, QueryResponse::select_one());
                registry.close_connection_after(closing_query);
            }
            let (shutdown, shutdown_rx) = oneshot::channel();
            let handle = tokio::spawn(server.run_with_shutdown(shutdown_rx));

            let calls = Arc::new(AtomicUsize::new(0));
            let recovery_started = Arc::new(Notify::new());
            let mut context = ClientContext::default();
            context.database = "master".into();
            context.connect_retry_count = retry_count;
            context.connect_retry_interval = 0;
            context.encryption_options.mode = EncryptionSetting::PreferOff;
            context.tds_authentication_method =
                TdsAuthenticationMethod::ActiveDirectoryTokenCredential;
            context.auth_method_map.insert(
                TdsAuthenticationMethod::ActiveDirectoryTokenCredential,
                Box::new(FailOnceCredential {
                    calls: calls.clone(),
                    failure,
                    recovery_started: recovery_started.clone(),
                }),
            );
            let datasource = format!("tcp:{},{}", addr.ip(), addr.port());
            let mut client = TdsConnectionProvider {}
                .create_client(context, &datasource, None)
                .await
                .unwrap();
            assert!(client.is_session_recovery_enabled());
            client.execute(closing_query.into(), ()).await.unwrap();
            while client.next_row().await.unwrap().is_some() {}
            client.close_query().await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let store = connections.lock().await;
                    if store.all().keys().any(|id| store.is_transport_closed(*id)) {
                        break;
                    }
                    drop(store);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();

            let cancel = CancelHandle::new();
            let requests_admitted = client.requests_admitted();
            let execute = client.execute(
                "SELECT 2".into(),
                ExecuteOptions {
                    cancel: Some(&cancel),
                    ..Default::default()
                },
            );
            let result = if matches!(failure, RecoveryFailure::CommandCancellation) {
                let (result, ()) = tokio::join!(execute, async {
                    recovery_started.notified().await;
                    cancel.cancel();
                });
                result
            } else {
                execute.await
            };
            let retried = matches!(failure, RecoveryFailure::ConnectionError);
            if retried {
                result.unwrap();
                client.close_query().await.unwrap();
            } else {
                let error = result.unwrap_err();
                if matches!(failure, RecoveryFailure::ProviderCancellation) {
                    assert!(matches!(error, Error::TokenAcquisitionCancelled));
                } else {
                    assert!(matches!(error, Error::OperationCancelledError(_)));
                }
            }

            assert_eq!(calls.load(Ordering::SeqCst), if retried { 3 } else { 2 });
            assert_eq!(client.connection_recovery_count(), u32::from(retried));
            assert_eq!(
                client.requests_admitted(),
                requests_admitted + u64::from(retried)
            );
            client.close_connection().await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let store = connections.lock().await;
                    let requests: usize = store
                        .all()
                        .values()
                        .map(|info| info.transaction_descriptor_headers.len())
                        .sum();
                    if store.count() == (if retried { 3 } else { 2 })
                        && requests == (if retried { 2 } else { 1 })
                    {
                        break;
                    }
                    drop(store);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            {
                let store = connections.lock().await;
                let recovery: Vec<_> = store
                    .all()
                    .iter()
                    .filter(|(id, _)| {
                        store
                            .login_details(**id)
                            .is_some_and(|info| info.recovery_login)
                    })
                    .collect();
                assert_eq!(recovery.len(), if retried { 2 } else { 1 });
                assert!(recovery[0].1.transaction_descriptor_headers.is_empty());
                assert!(!recovery[0].1.authenticated);
            }
            shutdown.send(()).unwrap();
            handle.await.unwrap().unwrap();
        }
    }
}

#[tokio::test]
async fn request_snapshots_do_not_signal_transport_closure() {
    for with_shutdown in [false, true] {
        let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr();
        let connections = server.connection_store();
        let closing_query = "SELECT 1 AS close_session";
        {
            let registry = server.query_registry();
            let mut registry = registry.lock().await;
            registry.register(closing_query, QueryResponse::select_one());
            registry.close_connection_after(closing_query);
        }
        let (shutdown, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            if with_shutdown {
                server.run_with_shutdown(shutdown_rx).await
            } else {
                server.run().await
            }
        });
        let mut context = ClientContext::default();
        context.user_name = "mock-user".into();
        context.password = "mock-password".into();
        context.encryption_options.mode = EncryptionSetting::PreferOff;
        let datasource = format!("tcp:{},{}", addr.ip(), addr.port());
        let mut client = TdsConnectionProvider {}
            .create_client(context, &datasource, None)
            .await
            .unwrap();
        client.execute("SELECT 1".into(), ()).await.unwrap();
        while client.next_row().await.unwrap().is_some() {}
        client.close_query().await.unwrap();
        let id = {
            let store = connections.lock().await;
            let (&id, info) = store.all().first_key_value().unwrap();
            assert_eq!(info.transaction_descriptor_headers.len(), 1);
            assert!(!store.is_transport_closed(id));
            id
        };
        client.execute(closing_query.into(), ()).await.unwrap();
        while client.next_row().await.unwrap().is_some() {}
        client.close_query().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !connections.lock().await.is_transport_closed(id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        client.close_connection().await.unwrap();
        {
            let mut store = connections.lock().await;
            store.clear();
            assert_eq!(store.count(), 0);
            assert!(!store.is_transport_closed(id));
        }
        if with_shutdown {
            shutdown.send(()).unwrap();
            handle.await.unwrap().unwrap();
        } else {
            handle.abort();
            assert!(handle.await.unwrap_err().is_cancelled());
        }
    }
}
