// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod common;

mod info_message_tests {
    use crate::common::{begin_connection, build_tcp_datasource, init_tracing};
    use mssql_tds::connection::tds_client::{ResultSet, ResultSetClient};
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::error::SqlInfoMessage;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    fn messages_contain(messages: &[SqlInfoMessage], expected: &str) -> bool {
        messages
            .iter()
            .any(|message| message.message.contains(expected))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_captures_multiple_info_messages_before_result_set() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;
        // No manual clear needed: execute() starts each command with a fresh
        // info-message buffer, so any login-time messages are dropped here.

        connection
            .execute(
                "PRINT N'tds info before result one';
                 RAISERROR(N'tds info before result two', 10, 1) WITH NOWAIT;
                 SELECT CAST(42 AS INT) AS answer;"
                    .to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        let messages = connection.info_messages();
        assert!(
            messages_contain(messages, "tds info before result one"),
            "INFO messages should include PRINT output: {messages:?}"
        );
        assert!(
            messages_contain(messages, "tds info before result two"),
            "INFO messages should include low-severity RAISERROR output: {messages:?}"
        );

        let resultset = connection
            .get_current_resultset()
            .expect("query should be positioned on a result set");
        let row = resultset
            .next_row()
            .await
            .unwrap()
            .expect("query should return one row");
        assert_eq!(row[0], ColumnValues::Int(42));

        connection.close_query().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_captures_info_messages_from_no_result_batch() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        connection
            .execute(
                "PRINT N'tds info no result one';
                 RAISERROR(N'tds info no result two', 10, 1) WITH NOWAIT;"
                    .to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        assert!(
            connection.get_current_resultset().is_none(),
            "INFO-only batch should not open a result set"
        );

        let messages = connection.take_info_messages();
        assert!(
            messages_contain(&messages, "tds info no result one"),
            "INFO messages should include PRINT output: {messages:?}"
        );
        assert!(
            messages_contain(&messages, "tds info no result two"),
            "INFO messages should include low-severity RAISERROR output: {messages:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_query_captures_info_messages_after_result_rows() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        connection
            .execute(
                "SELECT CAST(1 AS INT) AS value;
                 PRINT N'tds info after rows one';
                 RAISERROR(N'tds info after rows two', 10, 1) WITH NOWAIT;"
                    .to_string(),
                None,
                None,
            )
            .await
            .unwrap();

        let resultset = connection
            .get_current_resultset()
            .expect("query should be positioned on a result set");
        let row = resultset
            .next_row()
            .await
            .unwrap()
            .expect("query should return one row");
        assert_eq!(row[0], ColumnValues::Int(1));

        connection.close_query().await.unwrap();

        let messages = connection.take_info_messages();
        assert!(
            messages_contain(&messages, "tds info after rows one"),
            "INFO messages should include PRINT output drained after rows: {messages:?}"
        );
        assert!(
            messages_contain(&messages, "tds info after rows two"),
            "INFO messages should include low-severity RAISERROR output drained after rows: {messages:?}"
        );
    }

    /// A new command starts with a clean info-message buffer: messages from a
    /// prior command (or login) must not bleed into the next command's
    /// `info_messages()`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_clears_info_messages_from_previous_command() {
        let mut connection = begin_connection(&build_tcp_datasource()).await;

        // First command emits an informational message.
        connection
            .execute("PRINT N'tds first command info';".to_string(), None, None)
            .await
            .unwrap();
        assert!(
            messages_contain(connection.info_messages(), "tds first command info"),
            "first command should surface its PRINT output: {:?}",
            connection.info_messages()
        );
        connection.close_query().await.unwrap();

        // Second command emits no informational messages; the buffer must be
        // reset so the first command's message is gone.
        connection
            .execute("SELECT CAST(7 AS INT) AS value;".to_string(), None, None)
            .await
            .unwrap();
        assert!(
            !messages_contain(connection.info_messages(), "tds first command info"),
            "stale info from the previous command must be cleared: {:?}",
            connection.info_messages()
        );

        let resultset = connection
            .get_current_resultset()
            .expect("query should be positioned on a result set");
        let row = resultset
            .next_row()
            .await
            .unwrap()
            .expect("query should return one row");
        assert_eq!(row[0], ColumnValues::Int(7));
        connection.close_query().await.unwrap();
    }
}
