// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for `ClientContext::login_server_name`.
//!
//! The override separates *where to connect* from *what name to present at
//! login*, which is what a connection through a tunnel, proxy or port-forward
//! needs. These tests read the ServerName back off the wire from the mock
//! server rather than asserting on the client's own state, because the failure
//! this guards against is a malformed packet: the field is stored as an
//! offset/length pair separate from its payload, so a length taken from the
//! dialled address and bytes taken from the override would corrupt LOGIN7 while
//! still looking correct from the client side.

use mssql_mock_tds::MockTdsServer;
use mssql_tds::connection::client_context::ClientContext;
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionOptions, EncryptionSetting};
use tokio::sync::oneshot;

fn test_context() -> ClientContext {
    let mut context = ClientContext::default();
    context.user_name = "sa".to_string();
    context.password = "TestPassword123!".to_string();
    context.database = "master".to_string();
    context.encryption_options = EncryptionOptions {
        mode: EncryptionSetting::PreferOff,
        trust_server_certificate: true,
        host_name_in_cert: None,
        server_certificate: None,
    };
    context.connect_timeout = 30;
    context
}

/// Connects to a mock server and returns the ServerName it saw in LOGIN7.
///
/// The server is always shut down and its outcome checked, even when the
/// connect or the read fails: those failures are reported only afterwards, so a
/// failing test neither leaves the server running nor hides a server error.
async fn login_server_name_on_the_wire(override_name: Option<&str>) -> String {
    let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().port();
    let store = server.connection_store();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let handle = tokio::spawn(server.run_with_shutdown(shutdown_rx));

    let seen = read_login_server_name(port, &store, override_name).await;

    let _ = shutdown_tx.send(());
    handle
        .await
        .expect("mock server task panicked")
        .expect("mock server failed");
    seen.unwrap_or_else(|failure| panic!("{failure}"))
}

/// Logs in through the mock server on `port` and waits for the ServerName it
/// recorded. Failures are returned rather than asserted, so the caller can stop
/// the server first.
async fn read_login_server_name(
    port: u16,
    store: &std::sync::Arc<tokio::sync::Mutex<mssql_mock_tds::ConnectionStore>>,
    override_name: Option<&str>,
) -> Result<String, String> {
    let mut context = test_context();
    context.login_server_name = override_name.map(str::to_string);

    let datasource = format!("127.0.0.1,{port}");
    // Boxed: connecting holds a large future, and leaving it inline makes this
    // helper's own future big enough to trip the `large_futures` lint.
    let client = Box::pin(TdsConnectionProvider {}.create_client(context, &datasource, None))
        .await
        .map_err(|error| format!("connect failed: {error:?}"))?;

    // Dropping the client lets the server's handler finish, but nothing awaits
    // it: the store is written only when that handler exits, so poll for the
    // record rather than assume a fixed delay outlasts it.
    drop(client);

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
    loop {
        let recorded = {
            let store = store.lock().await;
            let connections = store.all();
            if connections.len() > 1 {
                return Err(format!(
                    "expected at most one connection, got {}",
                    connections.len()
                ));
            }
            connections
                .values()
                .next()
                .and_then(|c| c.received_server_name.clone())
        };
        if let Some(name) = recorded {
            return Ok(name);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("server never recorded a ServerName from LOGIN7".to_string());
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn without_an_override_login_names_the_dialled_address() {
    let seen = login_server_name_on_the_wire(None).await;
    assert!(
        seen.starts_with("127.0.0.1,"),
        "expected the dialled address, got {seen:?}"
    );
}

#[tokio::test]
async fn an_override_replaces_the_name_sent_at_login() {
    let seen = login_server_name_on_the_wire(Some("realserver.database.windows.net")).await;
    assert_eq!(seen, "realserver.database.windows.net");
}

/// The override is longer than the `127.0.0.1,<port>` actually dialled, so a
/// stale length would truncate it or run past the payload. Reading it back
/// intact proves the length and the bytes came from the same value.
#[tokio::test]
async fn an_override_longer_than_the_dialled_address_is_not_truncated() {
    let long = "a-very-long-server-name-that-exceeds-the-loopback-address.example.com";
    let seen = login_server_name_on_the_wire(Some(long)).await;
    assert_eq!(seen, long);
}

/// And the converse: an override shorter than the dialled address must not
/// leave trailing bytes from the longer value behind it.
#[tokio::test]
async fn an_override_shorter_than_the_dialled_address_is_not_padded() {
    let seen = login_server_name_on_the_wire(Some("db")).await;
    assert_eq!(seen, "db");
}
