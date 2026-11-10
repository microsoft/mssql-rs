// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, TryLockError};

use adbc_core::error::Result;
use adbc_core::options::*;
use adbc_core::{Connection, Database, Driver, Optionable};
use arrow_array::RecordBatchReader;
use arrow_schema::Schema;
use mssql_tds::connection::tds_client::{ExecuteOptions, StatementId, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::CancelHandle;
use mssql_tds::message::transaction_management::TransactionIsolationLevel;

use crate::error::{state, tds, unsupported};
use crate::options::{self, Config};
use crate::runtime::Executor;
use crate::statement::MssqlStatement;

pub(crate) type Reader = Box<dyn RecordBatchReader + Send>;

/// Native Rust ADBC driver. Exported as `AdbcDriverMssqlInit` with the `ffi` feature.
#[derive(Default)]
pub struct MssqlDriver;

/// Connection configuration and a runtime shared by its connections.
pub struct MssqlDatabase {
    pub(crate) config: Config,
    executor: Arc<Executor>,
}

impl Driver for MssqlDriver {
    type DatabaseType = MssqlDatabase;
    fn new_database(&mut self) -> Result<MssqlDatabase> {
        Ok(MssqlDatabase {
            config: Config::default(),
            executor: Arc::new(Executor::new()?),
        })
    }
    fn new_database_with_opts(
        &mut self,
        opts: impl IntoIterator<Item = (OptionDatabase, OptionValue)>,
    ) -> Result<MssqlDatabase> {
        let mut database = self.new_database()?;
        for (key, value) in opts {
            database.set_option(key, value)?;
        }
        Ok(database)
    }
}

macro_rules! unsupported_getters {
    ($option:ty) => {
        fn get_option_bytes(&self, key: $option) -> Result<Vec<u8>> {
            Err(unsupported(format!("bytes option {}", key.as_ref())))
        }
        fn get_option_double(&self, key: $option) -> Result<f64> {
            Err(unsupported(format!("double option {}", key.as_ref())))
        }
    };
}
pub(crate) use unsupported_getters;

impl Optionable for MssqlDatabase {
    type Option = OptionDatabase;
    fn set_option(&mut self, key: OptionDatabase, value: OptionValue) -> Result<()> {
        self.config.set(key.as_ref(), value)
    }
    fn get_option_string(&self, key: OptionDatabase) -> Result<String> {
        match key.as_ref() {
            options::DATA_SOURCE => Ok(self.config.context.data_source.clone()),
            options::DATABASE => Ok(self.config.context.database.clone()),
            "username" => Ok(self.config.context.user_name.clone()),
            _ => Err(unsupported(
                "database option retrieval (credentials are write-only)",
            )),
        }
    }
    fn get_option_int(&self, key: OptionDatabase) -> Result<i64> {
        match key.as_ref() {
            options::QUERY_TIMEOUT => Ok(i64::from(self.config.timeout)),
            options::BATCH_ROWS => Ok(self.config.batch_rows as i64),
            options::BATCH_BYTES => Ok(self.config.batch_bytes as i64),
            _ => Err(unsupported(format!("integer option {}", key.as_ref()))),
        }
    }
    unsupported_getters!(OptionDatabase);
}

impl Database for MssqlDatabase {
    type ConnectionType = MssqlConnection;
    fn new_connection(&self) -> Result<MssqlConnection> {
        self.new_connection_with_opts([])
    }
    fn new_connection_with_opts(
        &self,
        opts: impl IntoIterator<Item = (OptionConnection, OptionValue)>,
    ) -> Result<MssqlConnection> {
        if self.config.context.data_source.is_empty() {
            return Err(crate::error::invalid(
                "mssql.data_source or uri is required",
            ));
        }
        let context = self.config.context.clone();
        let source = context.data_source.clone();
        let client = self.executor.run(async {
            TdsConnectionProvider
                .create_client(context, &source, None)
                .await
                .map_err(tds)
        })?;
        let shared = Arc::new(Shared {
            executor: Arc::clone(&self.executor),
            session: Mutex::new(Session {
                client,
                active: None,
                generation: 0,
                next_statement: 1,
                autocommit: true,
                isolation: TransactionIsolationLevel::ReadCommitted,
                pending_unprepare: Vec::new(),
            }),
            cancel: Cancellation::default(),
            config: self.config.clone(),
        });
        let mut connection = MssqlConnection { shared };
        for (key, value) in opts {
            connection.set_option(key, value)?;
        }
        Ok(connection)
    }
}

#[derive(Clone, Default)]
pub(crate) struct Cancellation(Arc<Mutex<Option<Arc<CancelHandle>>>>);

impl Cancellation {
    pub(crate) fn set(&self, handle: Option<Arc<CancelHandle>>) -> Result<()> {
        *self
            .0
            .lock()
            .map_err(|_| state("cancellation mutex poisoned"))? = handle;
        Ok(())
    }
}

impl adbc_core::CancelHandle for Cancellation {
    fn try_cancel(&self) -> Result<()> {
        if let Some(handle) = &*self
            .0
            .lock()
            .map_err(|_| state("cancellation mutex poisoned"))?
        {
            handle.cancel();
        }
        Ok(())
    }
}

pub(crate) struct Session {
    pub(crate) client: TdsClient,
    pub(crate) active: Option<(u64, u64)>,
    pub(crate) generation: u64,
    next_statement: u64,
    pub(crate) autocommit: bool,
    pub(crate) isolation: TransactionIsolationLevel,
    pub(crate) pending_unprepare: Vec<StatementId>,
}

impl Session {
    pub(crate) fn idle(&self) -> Result<()> {
        if self.client.is_connection_dead() {
            return Err(state("connection is dead"));
        }
        if self.active.is_some() {
            return Err(state("another result stream owns this connection"));
        }
        Ok(())
    }
    pub(crate) async fn start(&mut self, statement: u64, timeout: u32) -> Result<u64> {
        if let Some((owner, _)) = self.active {
            if owner != statement {
                return Err(state("another statement has an active result stream"));
            }
            self.abandon().await?;
        }
        self.idle()?;
        for id in self.pending_unprepare.drain(..) {
            self.client
                .unprepare(id, ExecuteOptions::new().timeout_secs(timeout))
                .await
                .map_err(tds)?;
        }
        if !self.autocommit && !self.client.has_active_transaction() {
            self.client
                .begin_transaction_with_options(
                    self.isolation.clone(),
                    None,
                    ExecuteOptions::new().timeout_secs(timeout),
                )
                .await
                .map_err(tds)?;
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| state("execution generation exhausted"))?;
        Ok(self.generation)
    }
    pub(crate) async fn abandon(&mut self) -> Result<()> {
        self.active = None;
        self.client.abandon_query(5).await.map_err(tds)
    }
}

pub(crate) struct Shared {
    pub(crate) executor: Arc<Executor>,
    session: Mutex<Session>,
    pub(crate) cancel: Cancellation,
    pub(crate) config: Config,
}

impl Shared {
    pub(crate) fn with<T>(
        &self,
        work: impl FnOnce(&Executor, &mut Session) -> Result<T>,
    ) -> Result<T> {
        let mut guard = match self.session.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => {
                return Err(state("connection operation already in progress"));
            }
            Err(TryLockError::Poisoned(_)) => return Err(state("connection mutex poisoned")),
        };
        work(&self.executor, &mut guard)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let session = match self.session.get_mut() {
            Ok(session) => session,
            Err(_) => {
                tracing::error!("ADBC connection mutex poisoned during release");
                return;
            }
        };
        let result = self.executor.run(async {
            if session.client.has_open_batch() {
                session.abandon().await?;
            }
            if session.client.has_active_transaction() {
                session
                    .client
                    .rollback_transaction_with_options(
                        None,
                        None,
                        ExecuteOptions::new().timeout_secs(5),
                    )
                    .await
                    .map_err(tds)?;
            }
            session.client.close_connection().await.map_err(tds)
        });
        if let Err(error) = result {
            tracing::error!(%error, "ADBC connection cleanup failed");
        }
    }
}

/// A SQL Server session. Statements share the session; concurrent wire use is rejected.
pub struct MssqlConnection {
    pub(crate) shared: Arc<Shared>,
}

impl MssqlConnection {
    fn finish_transaction(&self, commit: bool) -> Result<()> {
        self.shared.with(|executor, session| {
            session.idle()?;
            if session.autocommit {
                return Err(state("commit/rollback requires autocommit disabled"));
            }
            executor.run(async {
                if session.client.has_active_transaction() {
                    let opts = ExecuteOptions::new().timeout_secs(self.shared.config.timeout);
                    if commit {
                        session
                            .client
                            .commit_transaction_with_options(None, None, opts)
                            .await
                            .map_err(tds)?;
                    } else {
                        session
                            .client
                            .rollback_transaction_with_options(None, None, opts)
                            .await
                            .map_err(tds)?;
                    }
                }
                Ok(())
            })
        })
    }
}

impl Optionable for MssqlConnection {
    type Option = OptionConnection;
    fn set_option(&mut self, key: OptionConnection, value: OptionValue) -> Result<()> {
        match key {
            OptionConnection::AutoCommit => {
                let enabled = bool::try_from(value)?;
                self.shared.with(|executor, session| {
                    session.idle()?;
                    executor.run(async {
                        if enabled && !session.autocommit && session.client.has_active_transaction()
                        {
                            session
                                .client
                                .commit_transaction_with_options(
                                    None,
                                    None,
                                    ExecuteOptions::new().timeout_secs(self.shared.config.timeout),
                                )
                                .await
                                .map_err(tds)?;
                        }
                        session.autocommit = enabled;
                        Ok(())
                    })
                })
            }
            _ => Err(unsupported(format!("connection option {}", key.as_ref()))),
        }
    }
    fn get_option_string(&self, key: OptionConnection) -> Result<String> {
        self.shared.with(|_, session| match key {
            OptionConnection::AutoCommit => Ok(session.autocommit.to_string()),
            OptionConnection::CurrentCatalog => Ok(session.client.database().to_owned()),
            _ => Err(unsupported(format!("connection option {}", key.as_ref()))),
        })
    }
    fn get_option_int(&self, key: OptionConnection) -> Result<i64> {
        Err(unsupported(key.as_ref()))
    }
    unsupported_getters!(OptionConnection);
}

impl Connection for MssqlConnection {
    type StatementType = MssqlStatement;
    fn new_statement(&mut self) -> Result<MssqlStatement> {
        let id = self.shared.with(|_, session| {
            let id = session.next_statement;
            session.next_statement = id
                .checked_add(1)
                .ok_or_else(|| state("statement identifiers exhausted"))?;
            Ok(id)
        })?;
        Ok(MssqlStatement::new(Arc::clone(&self.shared), id))
    }
    fn get_cancel_handle(&self) -> Box<dyn adbc_core::CancelHandle> {
        Box::new(self.shared.cancel.clone())
    }
    fn commit(&mut self) -> Result<()> {
        self.finish_transaction(true)
    }
    fn rollback(&mut self) -> Result<()> {
        self.finish_transaction(false)
    }
    fn get_info(&self, _codes: Option<HashSet<InfoCode>>) -> Result<Reader> {
        Err(unsupported("get_info"))
    }
    fn get_objects(
        &self,
        _depth: ObjectDepth,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
        _types: Option<Vec<&str>>,
        _column: Option<&str>,
    ) -> Result<Reader> {
        Err(unsupported("get_objects"))
    }
    fn get_table_schema(
        &self,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: &str,
    ) -> Result<Schema> {
        Err(unsupported("get_table_schema"))
    }
    fn get_table_types(&self) -> Result<Reader> {
        Err(unsupported("get_table_types"))
    }
    fn get_statistic_names(&self) -> Result<Reader> {
        Err(unsupported("statistics"))
    }
    fn get_statistics(
        &self,
        _catalog: Option<&str>,
        _schema: Option<&str>,
        _table: Option<&str>,
        _approximate: bool,
    ) -> Result<Reader> {
        Err(unsupported("statistics"))
    }
    fn read_partition(&self, _partition: impl AsRef<[u8]>) -> Result<Reader> {
        Err(unsupported("partition reads"))
    }
}
