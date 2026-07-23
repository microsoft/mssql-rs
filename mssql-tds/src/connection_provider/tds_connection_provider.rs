// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{debug, info};

use crate::connection::client_context::{ClientContext, TransportContext};
use crate::connection::connection_actions::{
    ActionOutcome, ConnectionAction, ConnectionActionChain, ExecutionContext,
};
use crate::connection::instance_cache::InstanceCache;
use crate::connection::session_recovery::SessionRecoveryData;
use crate::connection::tds_client::TdsClient;
use crate::connection::transport::network_transport;
use crate::connection::transport::tds_transport::TdsTransport;
#[cfg(windows)]
use crate::core::EncryptionSetting;
use crate::core::{CancelHandle, TdsResult};
use crate::error::Error::{OperationCancelledError, TimeoutError};
use crate::error::{Error, SqlInfoMessage, TimeoutErrorType};
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
        let (transport, negotiated_settings, execution_context, info_messages) =
            Self::connect_with_transport(&context, &context.transport_context, transport).await?;
        let mut client = TdsClient::new(transport, negotiated_settings, execution_context, context);
        client.extend_info_messages(info_messages);
        Ok(client)
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

        validate_multi_subnet_failover(
            context.multi_subnet_failover,
            &context.failover_partner,
            parsed.needs_ssrp(),
        )?;

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
            // Resolve SSRP (SQL Browser) if the action chain requires it.
            // Check the instance cache first to avoid a redundant UDP round-trip.
            let mut exec_context = ExecutionContext::new();
            let mut cache_key: Option<(String, String)> = None;

            // Try Shared Memory before SSRP for local named instances (Windows only).
            // SM doesn't need instance resolution — it uses the name directly.
            // If SM succeeds, we skip SSRP entirely. This matches ODBC/SNI behavior.
            #[cfg(windows)]
            if action_chain.requires_ssrp()
                && let Some(sm_transport) = action_chain.first_shared_memory_transport()
            {
                debug!("Trying Shared Memory before SSRP");
                let timeout_duration = match context.connect_timeout {
                    1.. => Some(Duration::from_secs(context.connect_timeout.into())),
                    _ => None,
                };
                let connect_future =
                    Self::connect_with_transport_context(context, &sm_transport, None);
                let sm_result = match timeout_duration.as_ref() {
                    Some(duration) => match timeout(*duration, connect_future).await {
                        Ok(result) => result,
                        Err(_) => Err(TimeoutError(TimeoutErrorType::String(
                            "Timeout while connecting via Shared Memory".to_string(),
                        ))),
                    },
                    None => connect_future.await,
                };
                match sm_result {
                    Ok((transport, negotiated_settings, execution_context, info_messages)) => {
                        debug!("Shared Memory connection succeeded, skipping SSRP");
                        let mut client = TdsClient::new(
                            transport,
                            negotiated_settings,
                            execution_context,
                            context.clone(),
                        );
                        client.extend_info_messages(info_messages);
                        return Ok(client);
                    }
                    Err(err) => {
                        debug!("Shared Memory failed ({}), falling through to SSRP", err);
                    }
                }
            }

            if action_chain.requires_ssrp() {
                // Derive cache key from the QuerySsrp action
                let (server, instance) = action_chain
                    .actions()
                    .iter()
                    .find_map(|a| match a {
                        ConnectionAction::QuerySsrp {
                            server, instance, ..
                        } => Some((server.as_str(), instance.as_str())),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        Error::ImplementationError(
                            "requires_ssrp() returned true but no QuerySsrp action found"
                                .to_string(),
                        )
                    })?;

                if action_chain.uses_cache() {
                    if let Some(cached) = InstanceCache::global().get(server, instance)? {
                        debug!(?cached, server, instance, "SSRP cache hit");
                        exec_context.store_outcome(ActionOutcome::CacheHit {
                            protocol: crate::connection::datasource_parser::ProtocolType::Tcp,
                            port: cached.port,
                            pipe_path: cached.pipe_path,
                        });
                    } else {
                        debug!(server, instance, "SSRP cache miss");
                        Self::resolve_ssrp(&action_chain, &mut exec_context, context.ssrp_timeout_ms).await?;
                        cache_key = Some((server.to_string(), instance.to_string()));
                    }
                } else {
                    Self::resolve_ssrp(&action_chain, &mut exec_context, context.ssrp_timeout_ms).await?;
                }
            }

            // Update instance cache after a successful SSRP resolution
            if let Some((server, instance)) = &cache_key {
                use crate::connection::connection_actions::ResultSlot;

                let port = exec_context.get_port(ResultSlot::ResolvedPort);
                #[cfg(windows)]
                let pipe_path = exec_context.get_pipe_path(ResultSlot::ResolvedPipePath);
                #[cfg(not(windows))]
                let pipe_path: Option<String> = None;

                if port.is_some() || pipe_path.is_some() {
                    debug!(?port, ?pipe_path, server = %server, instance = %instance, "Caching SSRP result");
                    InstanceCache::global().insert(server, instance, port, pipe_path)?;
                }
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

            let connect_retry_count = context.connect_retry_count;
            let connect_retry_interval = Duration::from_secs(context.connect_retry_interval.into());
            // The outer login deadline bounds the whole connect (network + TDS
            // handshake + auth token acquisition). It uses `login_timeout` when
            // set, else falls back to `connect_timeout` so callers that only set
            // the historical single knob keep the same behavior. `0` (either
            // field) means "no deadline". `connect_timeout` still bounds each
            // individual TCP-connect attempt, so an unreachable host fails fast
            // even when the login deadline is large (e.g. interactive sign-in).
            let login_timeout = context.login_timeout.unwrap_or(context.connect_timeout);
            let deadline = match login_timeout {
                1.. => Some(Instant::now() + Duration::from_secs(login_timeout.into())),
                _ => None,
            };

            let cancellation_token = cancel_handle.map(|handle| handle.cancel_token.child_token());

            // Outer retry loop for transient connection failures
            let mut last_error = None;

            for attempt in 0..=connect_retry_count {
                if attempt > 0 {
                    // Check if enough time remains for the retry interval wait
                    if let Some(dl) = deadline
                        && Instant::now() + connect_retry_interval > dl
                    {
                        debug!(
                            "Not enough time remaining for retry interval; aborting after {} attempt(s)",
                            attempt
                        );
                        break;
                    }
                    debug!(
                        "Waiting {}s before connection retry attempt {}",
                        connect_retry_interval.as_secs(),
                        attempt
                    );
                    tokio::time::sleep(connect_retry_interval).await;
                }

                // Compute remaining time budget for this attempt
                let attempt_timeout = deadline.map(|dl| dl.saturating_duration_since(Instant::now()));
                if let Some(remaining) = attempt_timeout
                    && remaining.is_zero()
                {
                    debug!("Connect timeout expired before attempt {}", attempt);
                    break;
                }

                let mut redirect_count = 0;
                let max_redirects = 10;

                // Try each transport context in order
                for (transport_ctx, _action_timeout_ms) in &transport_contexts {
                    debug!("Attempt {}: connecting with {:?}", attempt, transport_ctx);

                    // Check for cancellation
                    if cancellation_token
                        .as_ref()
                        .map_or_else(|| false, |token| token.is_cancelled())
                    {
                        return Err(OperationCancelledError(
                            "Login has been cancelled.".to_string(),
                        ));
                    }

                    let connect_future = Self::connect_with_transport_context(&context, transport_ctx, None);

                    // Recompute remaining time budget before each timeout call
                    let remaining = deadline.map(|dl| dl.saturating_duration_since(Instant::now()));
                    let mut connection_result = match remaining {
                        Some(remaining) => {
                            match timeout(remaining, connect_future).await {
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
                            Ok((transport, negotiated_settings, execution_context, info_messages)) => {
                                debug!("Connection successful via action chain");
                                let mut client = TdsClient::new(
                                    transport,
                                    negotiated_settings,
                                    execution_context,
                                    context.clone(),
                                );
                                client.extend_info_messages(info_messages);
                                return Ok(client);
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
                                let redirect_future = Self::connect_with_transport_context(
                                    &context,
                                    &tcp_transport_context,
                                    None,
                                );
                                // Recompute remaining time budget for redirected connect
                                let remaining = deadline
                                    .map(|dl| dl.saturating_duration_since(Instant::now()));
                                connection_result = match remaining {
                                    Some(remaining) => {
                                        match timeout(remaining, redirect_future).await {
                                            Ok(result) => result,
                                            Err(_) => Err(TimeoutError(
                                                TimeoutErrorType::String(
                                                    "Timeout while connecting".to_string(),
                                                ),
                                            )),
                                        }
                                    }
                                    None => redirect_future.await,
                                };
                            }
                            Err(err) => {
                                debug!("Connection attempt failed: {}", err);

                                // Permanent errors should not be retried
                                if !err.is_transient_connect_error() {
                                    return Err(err);
                                }

                                last_error = Some(err);
                                break;
                            }
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

    /// Query SQL Browser (SSRP) to resolve the TCP port or named pipe path for a named instance.
    async fn resolve_ssrp(
        action_chain: &ConnectionActionChain,
        exec_context: &mut ExecutionContext,
        ssrp_timeout_ms: u64,
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

        let timeout_ms = if ssrp_timeout_ms == 0 {
            ssrp::DEFAULT_SSRP_TIMEOUT_MS
        } else {
            ssrp_timeout_ms
        };
        debug!(server = %server, instance = %instance, timeout_ms, "Executing SSRP query");

        let instance_info =
            ssrp::get_instance_info_ext(&server, &instance, ssrp::SSRP_PORT, timeout_ms)
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

        // Extract TCP port if available
        let tcp_port = transports.iter().find_map(|t| match t {
            TransportContext::Tcp { port, .. } => Some(*port),
            _ => None,
        });

        // Extract named pipe path if available (Windows only)
        #[cfg(windows)]
        let pipe_path = transports.iter().find_map(|t| match t {
            TransportContext::NamedPipe { pipe_name } => Some(pipe_name.clone()),
            _ => None,
        });

        if tcp_port.is_none() {
            #[cfg(windows)]
            if pipe_path.is_none() {
                return Err(Error::ConnectionError(format!(
                    "SQL Browser returned instance information for '{}' \
                     but no TCP or Named Pipe endpoint was available.",
                    instance
                )));
            }

            #[cfg(not(windows))]
            return Err(Error::ConnectionError(format!(
                "SQL Browser returned instance information for '{}' \
                 but no TCP endpoint was available.",
                instance
            )));
        }

        if let Some(port) = tcp_port {
            debug!(port, "SSRP resolved TCP port");
            exec_context.store_outcome(ActionOutcome::SsrpResolved { port });
        }

        #[cfg(windows)]
        if let Some(pipe) = pipe_path {
            // Normalize for local connections: \\COMPUTERNAME\pipe\... → \\.\pipe\...
            // This avoids SMB round-trips and "Access is denied" errors.
            let pipe = crate::connection::transport::named_pipes::localize_pipe_path(&pipe);
            debug!(pipe = %pipe, "SSRP resolved named pipe path");
            exec_context.store_outcome(ActionOutcome::SsrpResolvedPipe { pipe_path: pipe });
        }

        Ok(())
    }

    /// Creates a new connection from the given transport context.
    /// This method will create a new transport and execute the session handler.
    /// If the session handler returns a redirection token, this method will return an error.
    /// If the session handler returns a successful connection, this method will return the connection.
    /// If the session handler returns an error, this method will return the error.
    pub(crate) async fn connect_with_transport_context(
        context: &ClientContext,
        transport_context: &TransportContext,
        recovery_data: Option<Box<SessionRecoveryData>>,
    ) -> TdsResult<(
        Box<dyn TdsTransport>,
        crate::handler::handler_factory::NegotiatedSettings,
        crate::connection::execution_context::ExecutionContext,
        Vec<SqlInfoMessage>,
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
            recovery_data,
        };
        let session_result = factory
            .session_handler(transport_context)
            .execute(&mut *transport)
            .await;

        match session_result {
            Ok((negotiated_settings, info_messages)) => {
                // Create execution context for the new connection
                let execution_context =
                    crate::connection::execution_context::ExecutionContext::new();

                Ok((
                    transport as Box<dyn TdsTransport>,
                    negotiated_settings,
                    execution_context,
                    info_messages,
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
        Vec<SqlInfoMessage>,
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
            recovery_data: None,
        };
        let session_result = factory
            .session_handler(transport_context)
            .execute(&mut transport)
            .await;

        match session_result {
            Ok((negotiated_settings, info_messages)) => {
                // Create execution context for the new connection
                let execution_context =
                    crate::connection::execution_context::ExecutionContext::new();

                Ok((
                    Box::new(transport) as Box<dyn TdsTransport>,
                    negotiated_settings,
                    execution_context,
                    info_messages,
                ))
            }
            Err(err) => {
                let _ = transport.close_transport().await;
                Err(err)
            }
        }
    }
}

/// Validate MultiSubnetFailover constraints.
///
/// MSF is incompatible with database mirroring (failover partner) and with
/// named instances that require SSRP resolution (no explicit port).
fn validate_multi_subnet_failover(
    multi_subnet_failover: bool,
    failover_partner: &str,
    needs_ssrp: bool,
) -> TdsResult<()> {
    if !multi_subnet_failover {
        return Ok(());
    }

    if !failover_partner.is_empty() {
        return Err(Error::UsageError(
            "MultiSubnetFailover cannot be used with FailoverPartner (database mirroring). \
             These features are mutually exclusive. Remove one of the options."
                .to_string(),
        ));
    }

    if needs_ssrp {
        return Err(Error::UsageError(
            "MultiSubnetFailover cannot be used with a named instance \
             without an explicit port. Specify a port (e.g. server\\instance,1433) \
             or remove MultiSubnetFailover."
                .to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // ── MultiSubnetFailover validation tests ──

    #[test]
    fn msf_disabled_always_ok() {
        assert!(validate_multi_subnet_failover(false, "partner", true).is_ok());
    }

    #[test]
    fn msf_with_failover_partner_rejected() {
        let err = validate_multi_subnet_failover(true, "partner", false).unwrap_err();
        assert!(err.to_string().contains("FailoverPartner"));
    }

    #[test]
    fn msf_with_ssrp_rejected() {
        let err = validate_multi_subnet_failover(true, "", true).unwrap_err();
        assert!(err.to_string().contains("named instance"));
    }

    #[test]
    fn msf_with_explicit_port_ok() {
        assert!(validate_multi_subnet_failover(true, "", false).is_ok());
    }

    // ── Connection retry tests ──

    /// Helper to build a ClientContext targeting a specific host:port
    /// with the given retry settings.
    fn context_for_retry_test(
        host: &str,
        port: u16,
        connect_timeout: u32,
        retry_count: u32,
        retry_interval: u32,
    ) -> ClientContext {
        ClientContext {
            transport_context: TransportContext::Tcp {
                host: host.to_string(),
                port,
                instance_name: None,
            },
            connect_timeout,
            connect_retry_count: retry_count,
            connect_retry_interval: retry_interval,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn permanent_error_not_retried() {
        // SSRP resolution errors propagate immediately without triggering
        // the retry loop (SSRP runs before the transport retry loop).
        // Using 127.0.0.1 ensures DNS resolves instantly; the SSRP query
        // times out in ~1s (no SQL Browser listening).
        let provider = TdsConnectionProvider;
        let ctx = ClientContext {
            connect_retry_count: 3,
            connect_retry_interval: 1,
            connect_timeout: 30,
            ..Default::default()
        };

        let start = Instant::now();
        // Named instance forces SSRP path; 127.0.0.1 avoids DNS delays.
        let result = provider
            .create_client(ctx, "tcp:127.0.0.1\\instance", None)
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        // SSRP timeout is ~1s. With retry_count=3 and retry_interval=1s,
        // retries would add ≥3s. Verify we finish well under that.
        assert!(
            elapsed.as_secs() < 3,
            "SSRP error should propagate immediately without retry sleep; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn retry_with_unreachable_host() {
        // Bind to a port and immediately drop the listener so the port is
        // guaranteed unused. Connection attempts will get ConnectionRefused (Io error = transient).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // port is now closed

        let ctx = context_for_retry_test(
            "127.0.0.1",
            port,
            30, // generous timeout
            2,  // retry_count = 2 means 3 total attempts (0, 1, 2)
            1,  // 1 second interval
        );

        let provider = TdsConnectionProvider;
        let start = Instant::now();
        // Include the port in the datasource string so that create_client
        // (which parses the datasource and overrides transport_context)
        // uses the correct closed port rather than defaulting to 1433.
        let datasource = format!("tcp:127.0.0.1,{port}");
        let result = provider.create_client(ctx, &datasource, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        // With retry_count=2, we sleep 1s between each retry → at least 2s total sleep
        assert!(
            elapsed.as_secs() >= 2,
            "Expected at least 2s of retry sleep, but took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn retry_zero_means_single_attempt() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let ctx = context_for_retry_test(
            "127.0.0.1",
            port,
            30,
            0, // retry_count = 0 → exactly 1 attempt, no retries
            5, // interval won't be used
        );

        let provider = TdsConnectionProvider;
        let start = Instant::now();
        let datasource = format!("tcp:127.0.0.1,{port}");
        let result = provider.create_client(ctx, &datasource, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        // With 0 retries, should complete almost instantly — no 5s sleep
        assert!(
            elapsed.as_secs() < 3,
            "With retry_count=0, should not sleep; took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn retry_respects_timeout_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let ctx = context_for_retry_test(
            "127.0.0.1",
            port,
            3,  // 3 second total timeout
            10, // up to 10 retries (but timeout should cut it short)
            2,  // 2 second interval
        );

        let provider = TdsConnectionProvider;
        let start = Instant::now();
        let datasource = format!("tcp:127.0.0.1,{port}");
        let result = provider.create_client(ctx, &datasource, None).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        // Timeout is 3s, retry interval is 2s.
        // Attempt 0 completes quickly (~connect refused).
        // Before attempt 1: need 2s sleep, 2s < 3s remaining → sleeps, then attempt 1.
        // Before attempt 2: need 2s sleep, but <1s remaining → aborts.
        // Total: should finish within ~4s (timeout + some overhead)
        assert!(
            elapsed.as_secs() < 6,
            "Should respect timeout deadline; took {elapsed:?}"
        );
    }
}
