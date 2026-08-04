// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod common;

#[cfg(test)]
mod transactions {
    use crate::common::{
        ExpectedQueryResultType, begin_connection, build_tcp_datasource,
        run_query_and_check_results, validate_results,
    };
    use mssql_tds::connection::tds_client::StatementResult;
    use mssql_tds::message::transaction_management::{CreateTxnParams, TransactionIsolationLevel};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_sql_rollback_transaction() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        run_query_and_check_results(
            &mut connection,
            "SET IMPLICIT_TRANSACTIONS ON".to_string(),
            &expected,
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int(col int)".to_string(),
            &expected,
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "INSERT INTO #dummy_int VALUES(1)".to_string(),
            &[ExpectedQueryResultType::Update(1)],
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "ROLLBACK transaction".to_string(),
            &expected,
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int(col int)".to_string(),
            &expected,
        )
        .await; // Should be able to create this table if rolled back.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_sql_commit_transaction() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        run_query_and_check_results(
            &mut connection,
            "SET IMPLICIT_TRANSACTIONS ON".to_string(),
            &expected,
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "INSERT INTO #dummy_int2 VALUES(1)".to_string(),
            &[ExpectedQueryResultType::Update(1)],
        )
        .await;
        run_query_and_check_results(&mut connection, "COMMIT transaction".to_string(), &expected)
            .await;
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(1)],
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_sql_switch_back_to_autocommit() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        run_query_and_check_results(
            &mut connection,
            "SET IMPLICIT_TRANSACTIONS ON".to_string(),
            &expected,
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "INSERT INTO #dummy_int2 VALUES(1)".to_string(),
            &[ExpectedQueryResultType::Update(1)],
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "SET IMPLICIT_TRANSACTIONS OFF".to_string(),
            &expected,
        )
        .await;
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(1)],
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_getdtc() {
        let expected = [ExpectedQueryResultType::Result(1)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        connection.get_dtc_address().await.unwrap();
        // `get_dtc_address` positions on its result set exactly like `execute`;
        // reconstruct the first `StatementResult` for statement-wise validation.
        let first = if connection.on_rows() {
            StatementResult::Rows
        } else {
            StatementResult::End
        };
        validate_results(&mut connection, first, &expected)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_begin_named_rollback() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test01".to_string()),
            )
            .await
            .unwrap();

        // Rollback transaction with name
        connection
            .rollback_transaction(Some("test01".to_string()), None)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_begin_unnamed_rollback() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction without name
        connection
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .unwrap();

        // Rollback transaction without name
        connection.rollback_transaction(None, None).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_begin_named_no_rollback() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name (no rollback)
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test01".to_string()),
            )
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_begin_unnamed_no_rollback() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction without name (no rollback)
        connection
            .begin_transaction(TransactionIsolationLevel::ReadCommitted, None)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_commit_no_new_transaction() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test02".to_string()),
            )
            .await
            .unwrap();

        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;

        // Commit transaction with name, no new transaction
        connection
            .commit_transaction(Some("test02".to_string()), None)
            .await
            .unwrap();

        // Ensure table is still there.
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(0)],
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_commit_new_transaction_named() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name "test03"
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test03".to_string()),
            )
            .await
            .unwrap();

        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;

        // Commit transaction "test03" and start new transaction "test04"
        connection
            .commit_transaction(
                Some("test03".to_string()),
                Some(CreateTxnParams {
                    level: TransactionIsolationLevel::NoChange,
                    name: Some("test04".to_string()),
                }),
            )
            .await
            .unwrap();

        // Ensure table is still there.
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(0)],
        )
        .await;

        // Commit the new transaction "test04"
        connection
            .commit_transaction(Some("test04".to_string()), None)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_commit_new_unnamed() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name "test05"
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test05".to_string()),
            )
            .await
            .unwrap();

        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;

        // Commit transaction "test05" and start new unnamed transaction
        connection
            .commit_transaction(
                Some("test05".to_string()),
                Some(CreateTxnParams {
                    level: TransactionIsolationLevel::NoChange,
                    name: None,
                }),
            )
            .await
            .unwrap();

        // Ensure table is still there.
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(0)],
        )
        .await;

        // Commit the new unnamed transaction
        connection.commit_transaction(None, None).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_commit_new_unnamed_loop_10x() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        let mut counter = 0;
        loop {
            // Begin transaction with name "test05"
            connection
                .begin_transaction(
                    TransactionIsolationLevel::ReadCommitted,
                    Some("test05".to_string()),
                )
                .await
                .unwrap();

            run_query_and_check_results(
                &mut connection,
                "CREATE TABLE #dummy_int2(col int)".to_string(),
                &expected,
            )
            .await;

            // Commit transaction "test05" and start new unnamed transaction
            connection
                .commit_transaction(
                    Some("test05".to_string()),
                    Some(CreateTxnParams {
                        level: TransactionIsolationLevel::NoChange,
                        name: None,
                    }),
                )
                .await
                .unwrap();

            // Ensure table is still there.
            run_query_and_check_results(
                &mut connection,
                "SELECT * FROM #dummy_int2".to_string(),
                &[ExpectedQueryResultType::Result(0)],
            )
            .await;

            // Commit the new unnamed transaction
            connection.commit_transaction(None, None).await.unwrap();

            counter += 1;
            if counter == 10 {
                break;
            }

            run_query_and_check_results(
                &mut connection,
                "DROP TABLE #dummy_int2".to_string(),
                &expected,
            )
            .await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_rollback_no_new_transaction() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name "test06"
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test06".to_string()),
            )
            .await
            .unwrap();

        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;

        // Commit the creation of the table and start a new transaction "test07"
        connection
            .commit_transaction(
                Some("test06".to_string()),
                Some(CreateTxnParams {
                    level: TransactionIsolationLevel::NoChange,
                    name: Some("test07".to_string()),
                }),
            )
            .await
            .unwrap();

        // Drop the test table so that we can rollback and check that it is still there.
        run_query_and_check_results(
            &mut connection,
            "DROP TABLE #dummy_int2".to_string(),
            &expected,
        )
        .await;

        // Rollback transaction "test07"
        connection
            .rollback_transaction(Some("test07".to_string()), None)
            .await
            .unwrap();

        // Ensure table is still there.
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(0)],
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_rollback_new_transaction_named() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name "test08"
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test08".to_string()),
            )
            .await
            .unwrap();

        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;

        // Commit transaction "test08" and start new transaction "test09"
        connection
            .commit_transaction(
                Some("test08".to_string()),
                Some(CreateTxnParams {
                    level: TransactionIsolationLevel::NoChange,
                    name: Some("test09".to_string()),
                }),
            )
            .await
            .unwrap();

        // Drop the test table so that we can rollback and check that it is still there.
        run_query_and_check_results(
            &mut connection,
            "DROP TABLE #dummy_int2".to_string(),
            &expected,
        )
        .await;

        // Rollback transaction "test09" and start new transaction "test10"
        connection
            .rollback_transaction(
                Some("test09".to_string()),
                Some(CreateTxnParams {
                    level: TransactionIsolationLevel::NoChange,
                    name: Some("test10".to_string()),
                }),
            )
            .await
            .unwrap();

        // Ensure table is still there.
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(0)],
        )
        .await;

        // Commit the new transaction "test10"
        connection
            .commit_transaction(Some("test10".to_string()), None)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_rollback_new_unnamed() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name "test11"
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test11".to_string()),
            )
            .await
            .unwrap();

        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;

        // Commit transaction "test11" and start new transaction "test12"
        connection
            .commit_transaction(
                Some("test11".to_string()),
                Some(CreateTxnParams {
                    level: TransactionIsolationLevel::NoChange,
                    name: Some("test12".to_string()),
                }),
            )
            .await
            .unwrap();

        // Drop the test table so that we can rollback and check that it is still there.
        run_query_and_check_results(
            &mut connection,
            "DROP TABLE #dummy_int2".to_string(),
            &expected,
        )
        .await;

        // Rollback transaction "test12" and start new unnamed transaction
        connection
            .rollback_transaction(
                Some("test12".to_string()),
                Some(CreateTxnParams {
                    level: TransactionIsolationLevel::NoChange,
                    name: None,
                }),
            )
            .await
            .unwrap();

        // Ensure table is still there.
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(0)],
        )
        .await;

        // Commit the new unnamed transaction
        connection.commit_transaction(None, None).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_req_savepoint() {
        let expected = [ExpectedQueryResultType::Update(0)];
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // Begin transaction with name "test13"
        connection
            .begin_transaction(
                TransactionIsolationLevel::ReadCommitted,
                Some("test13".to_string()),
            )
            .await
            .unwrap();

        run_query_and_check_results(
            &mut connection,
            "CREATE TABLE #dummy_int2(col int)".to_string(),
            &expected,
        )
        .await;

        // Create a savepoint where this table exists
        connection
            .save_transaction("test14".to_string())
            .await
            .unwrap();

        // Drop the test table so that we can rollback and check that it is still there.
        run_query_and_check_results(
            &mut connection,
            "DROP TABLE #dummy_int2".to_string(),
            &expected,
        )
        .await;

        // Rollback to the savepoint "test14"
        connection
            .rollback_transaction(Some("test14".to_string()), None)
            .await
            .unwrap();

        // Ensure table is still there.
        run_query_and_check_results(
            &mut connection,
            "SELECT * FROM #dummy_int2".to_string(),
            &[ExpectedQueryResultType::Result(0)],
        )
        .await;
    }
}
