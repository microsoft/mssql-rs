// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod timeout_and_cancel_tests {
    use crate::common::{
        ExpectedQueryResultType, begin_connection, build_tcp_datasource,
        run_query_and_check_results, trust_server_certificate,
    };
    use mssql_tds::connection::client_context::ClientContext;
    use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
    use mssql_tds::core::{CancelHandle, EncryptionOptions, EncryptionSetting, TdsResult};
    use mssql_tds::error::Error;
    use mssql_tds::error::Error::OperationCancelledError;
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[tokio::test]
    pub async fn timeout_test_no_retry() {
        timeout_test_with_retries(0).await;
    }

    #[tokio::test]
    pub async fn query_timeout_e2e() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let start_time = Instant::now();
        let execute_result = connection
            .execute(
                "WAITFOR DELAY '00:00:05'".to_string(),
                mssql_tds::connection::tds_client::ExecuteOptions {
                    timeout: Some(2),
                    ..Default::default()
                },
            )
            .await;

        // Timeout could happen during send or when getting results.
        verify_duration(execute_result, start_time, 1500, 3000);

        // Re-use the connection just to make sure it still works.
        let expected = [ExpectedQueryResultType::Result(1)];
        run_query_and_check_results(&mut connection, "SELECT 1".to_string(), &expected).await;
    }

    #[tokio::test]
    pub async fn login_cancel() {
        // Setup the cancellation handle.
        let cancel_handle = CancelHandle::new();
        let child_handle = cancel_handle.child_handle();

        // Spawn a local TCP server with a random port
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Use 127.0.0.1 directly to match the listener binding - localhost resolves to hostname
        let datasource = format!("tcp:127.0.0.1,{}", port);

        // Create a client context with a two-second timeout that points to localhost:1433.
        let mut client_context = ClientContext::default();
        client_context.database = "master".to_string();
        client_context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: trust_server_certificate(),
            host_name_in_cert: None,
            server_certificate: None,
        };

        let provider = TdsConnectionProvider {};
        let join_handle = tokio::spawn(async move {
            // Start a timer.
            let start_time = Instant::now();
            let result = provider
                .create_client(client_context, &datasource, Some(&child_handle))
                .await;
            verify_duration(result, start_time, 1000, 2500);
        });

        tokio::time::sleep(Duration::from_millis(1000)).await;
        cancel_handle.cancel();
        join_handle.await.unwrap();
    }

    #[tokio::test]
    pub async fn query_cancel() {
        // Setup the cancellation handle.
        let cancel_handle = CancelHandle::new();
        let child_handle = cancel_handle.child_handle();

        // Create a channel so that we know when we are connected and we don't accidentally
        // cancel the connection process.
        let (tx, rx) = oneshot::channel();

        let join_handle = tokio::spawn(async move {
            let mut connection = begin_connection(&build_tcp_datasource()).await;
            tx.send(true).unwrap();

            let start_time = Instant::now();
            let result = connection
                .execute(
                    "WAITFOR DELAY '00:00:05'".to_string(),
                    mssql_tds::connection::tds_client::ExecuteOptions {
                        timeout: None,
                        cancel: Some(&child_handle),
                        ..Default::default()
                    },
                )
                .await;
            verify_duration(result, start_time, 1000, 2500);

            // Re-use the connection just to make sure it still works.
            let expected = [ExpectedQueryResultType::Result(1)];
            run_query_and_check_results(&mut connection, "SELECT 1".to_string(), &expected).await;
        });

        // Sleep to send the request then cancel the root CancelHandle to terminate the operation.
        rx.await.unwrap();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        cancel_handle.cancel();

        // Validation happens within the closure. Block until the closure finishes.
        join_handle.await.unwrap();
    }

    fn verify_duration<T>(
        result: TdsResult<T>,
        start_time: Instant,
        expected_duration_lower: u64,
        expected_duration_upper: u64,
    ) {
        match result {
            Ok(_) => {
                panic!("Operation should not have succeeded.")
            }
            Err(error) => match error {
                OperationCancelledError(_) | Error::TimeoutError(_) => {
                    let duration = start_time.elapsed();

                    assert!(
                        duration < Duration::from_millis(expected_duration_upper),
                        "Expected duration < {expected_duration_upper:?}ms, got {duration:?}"
                    );
                    assert!(
                        duration > Duration::from_millis(expected_duration_lower),
                        "Expected duration > {expected_duration_lower:?}ms, got {duration:?}"
                    );
                }
                _ => panic!("Expected timeoutError error, got {error:?}"),
            },
        };
    }

    async fn timeout_test_with_retries(retry_count: u8) {
        // Spawn a local TCP server with a random port
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Use 127.0.0.1 directly to match the listener binding - localhost resolves to hostname
        let datasource = format!("tcp:127.0.0.1,{}", port);

        // Create a client context with a two-second timeout that points to localhost:1433.
        let mut client_context = ClientContext::default();
        client_context.database = "master".to_string();
        client_context.encryption_options = EncryptionOptions {
            mode: EncryptionSetting::PreferOff,
            trust_server_certificate: false,
            host_name_in_cert: None,
            server_certificate: None,
        };
        client_context.connect_timeout = 2;
        client_context.connect_retry_count = retry_count as u32;

        // Try to connect.
        let provider = TdsConnectionProvider {};

        // Start a timer.
        let start_time = Instant::now();
        let connection_result = provider
            .create_client(client_context, &datasource, None)
            .await;
        verify_duration(
            connection_result,
            start_time,
            1500 * (1 + retry_count) as u64,
            3000 * (1 + retry_count) as u64,
        );
    }
}
