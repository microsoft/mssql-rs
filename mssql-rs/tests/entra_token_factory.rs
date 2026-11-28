// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_mock_tds::MockTdsServer;
use mssql_rs::{
    AuthMethod, Client, Config, EncryptionLevel, EntraIdTokenFactory, Error,
    TdsAuthenticationMethod, TdsError, TdsResult,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

const CHALLENGED_SPN: &str = "https://database.windows.net/";
const FAKE_TOKEN: &str = "mock-fedauth-token";
const REJECTED_SPN: &str = "unexpected FedAuth SPN";

struct TestFactory {
    expected_spn: &'static str,
    call_count: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl EntraIdTokenFactory for TestFactory {
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>> {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(spn, CHALLENGED_SPN);
        assert_eq!(sts_url, "https://login.microsoftonline.com/test-tenant/");
        assert_eq!(
            auth_method,
            TdsAuthenticationMethod::ActiveDirectoryTokenCredential
        );
        if spn != self.expected_spn {
            return Err(TdsError::UsageError(REJECTED_SPN.to_string()));
        }
        Ok(FAKE_TOKEN
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect())
    }
}

async fn connect_with_factory(expected_spn: &'static str) {
    let server = MockTdsServer::new("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = server.local_addr();
    let connections = server.connection_store();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server_task = tokio::spawn(server.run_with_shutdown(shutdown_rx));

    let call_count = Arc::new(AtomicUsize::new(0));
    let mut cfg = Config::new();
    cfg.host(addr.ip().to_string())
        .port(addr.port())
        .encryption(EncryptionLevel::Off)
        .connect_retry_count(0)
        .login_timeout(Some(5))
        .authentication(AuthMethod::entra_token_factory(TestFactory {
            expected_spn,
            call_count: Arc::clone(&call_count),
        }));
    let cfg = cfg.clone();
    assert!(cfg.to_client_context().access_token.is_none());
    assert_eq!(call_count.load(Ordering::Relaxed), 0);

    let should_accept = expected_spn == CHALLENGED_SPN;
    let result = Client::connect(&cfg).await.map(drop);
    if should_accept {
        result.expect("accepted challenge completes login");
    } else {
        let err = result.expect_err("rejected challenge must not complete login");
        assert!(
            matches!(err, Error::Tds(TdsError::UsageError(ref message)) if message == REJECTED_SPN),
            "unexpected login error: {err}"
        );
    }
    assert_eq!(call_count.load(Ordering::Relaxed), 1);

    tokio::time::timeout(Duration::from_secs(5), async {
        while !connections.lock().await.is_transport_closed(0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("mock server observed transport closure");
    {
        let store = connections.lock().await;
        assert_eq!(store.count(), 1);
        let connection = store.get(0).expect("mock server captured Login7");
        assert!(connection.fedauth_workflow.is_some());
        assert_eq!(connection.authenticated, should_accept);
        if should_accept {
            assert_eq!(
                connection.received_token_as_string().as_deref(),
                Some(FAKE_TOKEN)
            );
        } else {
            assert!(connection.received_token.is_none());
        }
    }
    shutdown_tx.send(()).expect("stop mock server");
    server_task
        .await
        .expect("join mock server")
        .expect("mock server ran");
}

#[tokio::test]
async fn factory_can_supply_token_for_expected_spn() {
    connect_with_factory(CHALLENGED_SPN).await;
}

#[tokio::test]
async fn factory_can_reject_spn_without_sending_token() {
    connect_with_factory("https://different-audience.invalid/").await;
}
