// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the low-level primitives a connection pool needs from
//! `mssql-tds`: a cheap liveness check and accessors for the connection's
//! current session state (database, language, collation, packet size).
//!
//! These require a live SQL Server. Configure via the same environment
//! variables / `.env` as the other integration tests and run single-threaded:
//!
//! ```sh
//! SQL_PASSWORD='...' TRUST_SERVER_CERTIFICATE=true \
//!   cargo test -p mssql-tds --test test_connection_pool_primitives -- --test-threads=1
//! ```

#[cfg(test)]
mod common;

mod connection_pool_primitives {
    use crate::common::{build_tcp_datasource, create_client, get_scalar_value, init_tracing};
    use mssql_tds::connection::tds_client::TdsClient;

    #[ctor::ctor]
    fn init() {
        init_tracing();
    }

    /// Executes a statement and drains every result set so that any ENVCHANGE
    /// tokens (database / language / collation) are processed and reflected in
    /// the client's session-state accessors.
    async fn exec_and_drain(client: &mut TdsClient, sql: &str) {
        client.execute(sql.to_string(), ()).await.unwrap();
        let _ = get_scalar_value(client).await.unwrap();
    }

    #[tokio::test]
    async fn test_session_state_getters_at_login() -> Result<(), Box<dyn std::error::Error>> {
        let mut client = create_client(&build_tcp_datasource()).await?;

        // create_context connects to `master`.
        assert_eq!(client.database(), "master");
        // Language reflects the login default. The server only populates it when
        // it sends a Language ENVCHANGE, so it may be empty at login; calling the
        // accessor must not panic regardless.
        let _ = client.language();
        // Packet size is negotiated at login and must be a valid TDS size.
        assert!(client.packet_size() >= 512);
        // A freshly opened connection must not report as dead.
        assert!(!client.is_connection_dead());

        client.close_connection().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_database_getter_reflects_use() -> Result<(), Box<dyn std::error::Error>> {
        let mut client = create_client(&build_tcp_datasource()).await?;
        assert_eq!(client.database(), "master");

        // Changing database emits a Database ENVCHANGE token.
        exec_and_drain(&mut client, "USE tempdb").await;
        assert_eq!(client.database(), "tempdb");

        // And switching back is reflected too.
        exec_and_drain(&mut client, "USE master").await;
        assert_eq!(client.database(), "master");

        client.close_connection().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_language_getter_reflects_set_language() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut client = create_client(&build_tcp_datasource()).await?;
        let initial = client.language().to_string();

        // SET LANGUAGE emits a Language ENVCHANGE token with the new language.
        exec_and_drain(&mut client, "SET LANGUAGE Français").await;
        let updated = client.language();
        assert_ne!(
            updated, initial,
            "language getter should reflect the SET LANGUAGE change (was {initial:?}, now {updated:?})"
        );

        client.close_connection().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_is_connection_dead_after_close() -> Result<(), Box<dyn std::error::Error>> {
        let mut client = create_client(&build_tcp_datasource()).await?;
        assert!(!client.is_connection_dead());

        // After the transport is closed, the connection must report as dead so a
        // pool can discard it instead of handing it out for reuse.
        client.close_connection().await?;
        assert!(client.is_connection_dead());
        Ok(())
    }
}
