// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::connection::client_context::{ClientContext, TransportContext};
use crate::connection::connection_actions::{
    ActionOutcome, ConnectionAction, ConnectionActionChain, ExecutionContext,
};
use crate::connection::tds_client::TdsClient;
use crate::connection::transport::network_transport;
use crate::connection::transport::tds_transport::TdsTransport;
#[cfg(windows)]
use crate::core::EncryptionSetting;
use crate::core::{CancelHandle, TdsResult};
use crate::error::Error::{OperationCancelledError, TimeoutError};
use crate::error::{Error, TimeoutErrorType};
use crate::handler::handler_factory::HandlerFactory;
use crate::io::token_stream::GenericTokenParserRegistry;
use crate::ssrp;

#[cfg(fuzzing)]
use crate::io::token_stream::TdsTokenStreamReader;

use std::sync::LazyLock;

pub(crate) static PARSER_REGISTRY: LazyLock<GenericTokenParserRegistry> =
    LazyLock::new(GenericTokenParserRegistry::default);

/// Factory for establishing TDS connections and producing [`TdsClient`] instances.
pub struct TdsConnectionProvider;

impl Default for TdsConnectionProvider {
    fn default() -> Self {
        Self
    }
}

impl TdsConnectionProvider {
    /// Create a new TdsConnectionProvider
    pub fn new() -> Self {
        Self
    }

    /// Create a client with a custom transport (used for fuzzing)
    #[cfg(fuzzing)]
    #[allow(private_bounds)]
    pub async fn create_client_with_transport<T>(
        context: ClientContext,
        transport: T,
    ) -> TdsResult<TdsClient>
    where
        T: TdsTransport
            + crate::io::reader_writer::NetworkReaderWriter
            + TdsTokenStreamReader
            + crate::io::packet_reader::TdsPacketReader
            + 'static,
    {
        let (transport, negotiated_settings, execution_context) =
            Self::connect_with_transport(&context, &context.transport_context, transport).await?;
        Ok(TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
        ))
    }

    /// Create a client from a datasource string.
    /// This is the primary API for creating connections.
    ///
    /// This method uses the action chain pattern to determine the connection strategy,
    /// providing explicit and testable connection sequences.
    ///
    /// # Arguments
    /// * `context` - Client context with credentials and options (without transport_context set)
    /// * `datasource` - The data source string (e.g., "tcp:server,1433", "server\\instance", "lpc:.")
    /// * `cancel_handle` - Optional cancellation handle
    ///
    /// # Example
    /// ```ignore
    /// let provider = TdsConnectionProvider::new();
    /// let context = ClientContext::default();
    /// let client = provider.create_client(context, "tcp:myserver,1433", None).await?;
    /// ```
    pub async fn create_client(
        &self,
        mut context: ClientContext,
        datasource: &str,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<TdsClient> {
        // Parse the datasource to get the action chain
        let parsed = context.parse_datasource(datasource)?;

        // Validate the ClientContext before attempting connection
        context.validate()?;

        // Validate MultiSubnetFailover constraints
        if context.multi_subnet_failover {
            // MultiSubnetFailover cannot be used with database mirroring (failover_partner)
            if !context.failover_partner.is_empty() {
                return Err(Error::UsageError(
                    "MultiSubnetFailover cannot be used with FailoverPartner (database mirroring). \
                     These features are mutually exclusive. Remove one of the options."
                        .to_string(),
                ));
            }
        }

        // Get connection timeout
        let timeout_ms = if context.connect_timeout > 0 {
            (context.connect_timeout as u64) * 1000
        } else {
            15000 // Default 15 seconds
        };

        // Generate the action chain
        let action_chain = parsed.to_connection_actions(timeout_ms);

        debug!("Connection strategy:\n{}", action_chain.describe());

        // Execute the action chain to get transport contexts
        self.execute_action_chain(&context, action_chain, cancel_handle)
            .await
    }

    /// Execute an action chain to create a client
    ///
    /// This method resolves the action chain into transport contexts and attempts
    /// connection using each one in order.
    async fn execute_action_chain(
        &self,
        context: &ClientContext,
        action_chain: ConnectionActionChain,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<TdsClient> {
        CancelHandle::run_until_cancelled(cancel_handle, async move {
            // Resolve SSRP (SQL Browser) if the action chain requires it
            let mut exec_context = ExecutionContext::new();
            if action_chain.requires_ssrp() {
                Self::resolve_ssrp(&action_chain, &mut exec_context).await?;
            }

            // Check if LocalDB resolution is required (Windows only)
            // Apply encryption override for LocalDB connections at resolution time
            #[cfg(windows)]
            let (transport_contexts, context) = {
                if let Some(instance_name) = action_chain.requires_localdb_resolution() {
                    debug!("Action chain requires LocalDB resolution for instance: {}", instance_name);

                    // Apply LocalDB encryption override
                    let mut modified_context = context.clone();
                    if modified_context.encryption_options.mode != EncryptionSetting::PreferOff {
                        debug!(
                            "LocalDB connection detected: overriding encryption from {:?} to PreferOff",
                            modified_context.encryption_options.mode
                        );
                        modified_context.encryption_options.mode = EncryptionSetting::PreferOff;
                    }

                    // Resolve LocalDB instance to get the named pipe path
                    use crate::connection::transport::localdb::resolve_localdb_instance;
                    let pipe_path = resolve_localdb_instance(&instance_name).await?;
                    debug!("LocalDB resolved to pipe: {}", pipe_path);

                    (vec![(TransportContext::NamedPipe { pipe_name: pipe_path }, modified_context.connect_timeout as u64 * 1000)], modified_context)
                } else {
                    (action_chain.resolve_transport_contexts_with_context(&exec_context), context.clone())
                }
            };

            #[cfg(not(windows))]
            let (transport_contexts, context) = (action_chain.resolve_transport_contexts_with_context(&exec_context), context.clone());

            if transport_contexts.is_empty() {
                return Err(Error::ProtocolError(
                    "No transport protocols available in action chain".to_string()
                ));
            }

            debug!("Resolved {} transport context(s) from action chain", transport_contexts.len());

            let timeout_duration = match context.connect_timeout {
                1.. => Some(Duration::from_secs(context.connect_timeout.into())),
                _ => None,
            };

            let cancellation_token = cancel_handle.map(|handle| handle.cancel_token.child_token());

            // Try each transport context in order
            let mut last_error = None;
            let mut redirect_count = 0;
            let max_redirects = 10;

            for (transport_ctx, _action_timeout_ms) in &transport_contexts {
                debug!("Attempting connection with {:?}", transport_ctx);

                // Check for cancellation
                if cancellation_token
                    .as_ref()
                    .map_or_else(|| false, |token| token.is_cancelled())
                {
                    return Err(OperationCancelledError(
                        "Login has been cancelled.".to_string(),
                    ));
                }

                let connect_future = Self::connect_with_transport_context(&context, transport_ctx);

                let mut connection_result = match timeout_duration.as_ref() {
                    Some(duration) => {
                        match timeout(*duration, connect_future).await {
                            Ok(result) => result,
                            Err(_) => Err(TimeoutError(TimeoutErrorType::String(
                                "Timeout while connecting".to_string(),
                            ))),
                        }
                    }
                    None => connect_future.await,
                };

                // Handle redirections
                loop {
                    match connection_result {
                        Ok((transport, negotiated_settings, execution_context)) => {
                            debug!("Connection successful via action chain");
                            return Ok(TdsClient::new(
                                transport,
                                negotiated_settings,
                                execution_context,
                            ));
                        }
                        Err(Error::Redirection { host, port }) => {
                            info!("Redirection to: {:?}, {:?}", host, port);
                            redirect_count += 1;
                            if redirect_count > max_redirects {
                                return Err(Error::ProtocolError(
                                    "Received more redirection tokens than expected.".to_string(),
                                ));
                            }

                            let tcp_transport_context = TransportContext::from_routing_token(host, port);
                            connection_result = Self::connect_with_transport_context(
                                &context,
                                &tcp_transport_context,
                            ).await;
                        }
                        Err(err) => {
                            debug!("Connection attempt failed: {}", err);
                            last_error = Some(err);
                            break;
                        }
                    }
                }
            }

            // All transports failed
            Err(last_error.unwrap_or_else(|| {
                Error::ProtocolError(
                    "All connection attempts from action chain failed.".to_string(),
                )
            }))
        })
        .await
    }

    /// Query SQL Browser (SSRP) to resolve the TCP port for a named instance.
    async fn resolve_ssrp(
        action_chain: &ConnectionActionChain,
        exec_context: &mut ExecutionContext,
    ) -> TdsResult<()> {
        let (server, instance) = action_chain
            .actions()
            .iter()
            .find_map(|a| match a {
                ConnectionAction::QuerySsrp {
                    server, instance, ..
                } => Some((server.clone(), instance.clone())),
                _ => None,
            })
            .ok_or_else(|| {
                Error::ProtocolError(
                    "Action chain requires SSRP but contains no QuerySsrp action".to_string(),
                )
            })?;

        debug!(server = %server, instance = %instance, "Executing SSRP query");

        let instance_info = ssrp::get_instance_info(&server, &instance)
            .await
            .map_err(|e| {
                Error::ConnectionError(format!(
                    "Error Locating Server/Instance Specified [{}\\{}]. \
                     Ensure the instance name is correct and SQL Server Browser \
                     service is running on the host. ({})",
                    server, instance, e
                ))
            })?;

        let transports = ssrp::build_transport_list(instance_info, &server, &instance);

        let tcp_port = transports
            .iter()
            .find_map(|t| match t {
                TransportContext::Tcp { port, .. } => Some(*port),
                _ => None,
            })
            .ok_or_else(|| {
                Error::ConnectionError(format!(
                    "SQL Browser returned instance information for '{}' \
                     but no TCP endpoint was available.",
                    instance
                ))
            })?;

        debug!(tcp_port, "SSRP resolved instance port");
        exec_context.store_outcome(ActionOutcome::SsrpResolved { port: tcp_port });
        Ok(())
    }

    /// Creates a new connection from the given transport context.
    /// This method will create a new transport and execute the session handler.
    /// If the session handler returns a redirection token, this method will return an error.
    /// If the session handler returns a successful connection, this method will return the connection.
    /// If the session handler returns an error, this method will return the error.
    async fn connect_with_transport_context(
        context: &ClientContext,
        transport_context: &TransportContext,
    ) -> TdsResult<(
        Box<dyn TdsTransport>,
        crate::handler::handler_factory::NegotiatedSettings,
        crate::connection::execution_context::ExecutionContext,
    )> {
        // Create network transport directly
        // Convert connect_timeout from seconds to milliseconds
        let connect_timeout_ms = (context.connect_timeout as u64) * 1000;
        let mut transport = network_transport::create_transport(
            context.ipaddress_preference,
            context.tds_version(),
            transport_context,
            context.encryption_options.clone(),
            context.keep_alive_in_ms,
            context.keep_alive_interval_in_ms,
            context.multi_subnet_failover,
            connect_timeout_ms,
        )
        .await?;

        let factory = HandlerFactory {
            context: context.clone(),
        };
        let session_result = factory
            .session_handler(transport_context)
            .execute(&mut *transport)
            .await;

        match session_result {
            Ok(negotiated_settings) => {
                // Create execution context for the new connection
                let execution_context =
                    crate::connection::execution_context::ExecutionContext::new();

                Ok((
                    transport as Box<dyn TdsTransport>,
                    negotiated_settings,
                    execution_context,
                ))
            }
            Err(err) => {
                let _ = transport.close_transport().await;
                Err(err)
            }
        }
    }

    /// Internal generic method that works with a concrete transport type.
    /// This is separated out to allow working with specific transport implementations
    /// (NetworkTransport, MockTransport, etc.) without boxing overhead.
    /// Only exposed for fuzzing.
    #[cfg(fuzzing)]
    async fn connect_with_transport<T>(
        context: &ClientContext,
        transport_context: &TransportContext,
        mut transport: T,
    ) -> TdsResult<(
        Box<dyn TdsTransport>,
        crate::handler::handler_factory::NegotiatedSettings,
        crate::connection::execution_context::ExecutionContext,
    )>
    where
        T: TdsTransport
            + crate::io::reader_writer::NetworkReaderWriter
            + TdsTokenStreamReader
            + crate::io::packet_reader::TdsPacketReader
            + 'static,
    {
        let factory = HandlerFactory {
            context: context.clone(),
        };
        let session_result = factory
            .session_handler(transport_context)
            .execute(&mut transport)
            .await;

        match session_result {
            Ok(negotiated_settings) => {
                // Create execution context for the new connection
                let execution_context =
                    crate::connection::execution_context::ExecutionContext::new();

                Ok((
                    Box::new(transport) as Box<dyn TdsTransport>,
                    negotiated_settings,
                    execution_context,
                ))
            }
            Err(err) => {
                let _ = transport.close_transport().await;
                Err(err)
            }
        }
    }
}
