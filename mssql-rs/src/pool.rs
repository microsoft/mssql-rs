// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Connection pooling via [`deadpool`] with native mssql-tds session resets.
//!
//! Reused connections are reset and validated before checkout, including restoring
//! `READ COMMITTED` isolation. Use [`RecyclingMethod::Ping`] only when retaining
//! session state and relying on cached connection health is intentional.
//!
//! Connections marked dead after cancelled bridge I/O are rejected before the
//! recycle reset or ping, so deadpool drops them and creates replacements.
//! Healthy connections follow the selected recycling policy. See [`Client`]'s
//! cancellation safety contract, including the unguarded [`Client::inner_mut`]
//! escape hatch.
//!
//! # Example
//!
//! ```rust,no_run
//! use mssql_rs::{Config, AuthMethod, TdsManager};
//!
//! # fn example() -> std::result::Result<(), Box<dyn std::error::Error>> {
//! let mut cfg = Config::new();
//! cfg.host("localhost").authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD"))).trust_cert();
//! let pool = TdsManager::create_pool(cfg, 10)?;
//! // Use pool.get().await? to checkout connections
//! # Ok(())
//! # }
//! ```

use deadpool::managed::{Manager, Metrics, RecycleError, RecycleResult};

use crate::client::Client;
use crate::config::Config;
use crate::error::Error;

/// A checked-out connection from the pool.
pub type PooledConnection = deadpool::managed::Object<TdsManager>;

/// Connection pool type alias.
pub type Pool = deadpool::managed::Pool<TdsManager>;

/// How an existing connection is prepared for its next borrower.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecyclingMethod {
    /// Reset session state with the native TDS reset flag, restore
    /// `READ COMMITTED` isolation, and validate the server's response.
    ///
    /// Invalidates prepared statements from previous checkouts.
    #[default]
    Reset,
    /// Check cached connection health without I/O or resetting session state.
    ///
    /// Retains temporary tables, session settings, prepared handles, uncommitted
    /// transactions, and outstanding results. Unlike the former `SELECT 1`
    /// probe, this does not verify server responsiveness; an undetected idle
    /// failure may surface on the borrower's next operation.
    Ping,
}

/// [`deadpool::managed::Manager`] implementation for mssql-tds connections.
///
/// Creates and recycles [`Client`] connections using the provided [`Config`].
/// Recycling occurs on checkout, not when a connection is returned. Commit or
/// roll back transactions before returning a connection to avoid holding locks
/// while it is idle in the pool.
#[derive(Debug, Clone)]
pub struct TdsManager {
    config: Config,
    recycling_method: RecyclingMethod,
}

impl TdsManager {
    /// Create a manager using [`RecyclingMethod::Reset`].
    pub fn new(config: Config) -> Self {
        Self {
            config,
            recycling_method: RecyclingMethod::default(),
        }
    }

    /// Choose how reused connections are prepared for checkout.
    pub fn with_recycling_method(mut self, method: RecyclingMethod) -> Self {
        self.recycling_method = method;
        self
    }

    /// Build a [`Pool`] with native reset recycling and the given maximum size.
    ///
    /// # Errors
    ///
    /// Returns a build error if the pool configuration is invalid.
    pub fn create_pool(
        config: Config,
        max_size: usize,
    ) -> Result<Pool, deadpool::managed::BuildError> {
        let mgr = TdsManager::new(config);
        Pool::builder(mgr)
            .max_size(max_size)
            .runtime(deadpool::Runtime::Tokio1)
            .build()
    }
}

impl Manager for TdsManager {
    type Type = Client;
    type Error = Error;

    async fn create(&self) -> Result<Self::Type, Self::Error> {
        Client::connect(&self.config).await
    }

    async fn recycle(&self, conn: &mut Self::Type, _: &Metrics) -> RecycleResult<Self::Error> {
        conn.ensure_usable().map_err(RecycleError::Backend)?;
        match self.recycling_method {
            RecyclingMethod::Reset => conn.reset_session().await,
            RecyclingMethod::Ping => conn.ping().await,
        }
        .map_err(RecycleError::Backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_is_clone_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        fn assert_clone<T: Clone>() {}
        assert_send::<TdsManager>();
        assert_sync::<TdsManager>();
        assert_clone::<TdsManager>();
    }

    #[test]
    fn create_pool_builder() {
        let cfg = Config::new();
        TdsManager::create_pool(cfg, 10).expect("pool configuration should be valid");
    }

    #[test]
    fn native_reset_is_default_and_ping_is_explicit() {
        let manager = TdsManager::new(Config::new());
        assert_eq!(manager.recycling_method, RecyclingMethod::Reset);
        let manager = manager.with_recycling_method(RecyclingMethod::Ping);
        assert_eq!(manager.recycling_method, RecyclingMethod::Ping);
        let pool = Pool::builder(manager)
            .max_size(2)
            .build()
            .expect("pool configuration should be valid");
        assert_eq!(pool.status().max_size, 2);
    }
}
