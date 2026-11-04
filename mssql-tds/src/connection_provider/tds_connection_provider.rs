// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::connection::client_context::{ClientContext, TransportContext};
use crate::connection::connect_stage;
use crate::connection::connection_actions::{
    ActionOutcome, ConnectionAction, ConnectionActionChain, ExecutionContext,
};
use crate::connection::instance_cache::InstanceCache;
use crate::connection::session_recovery::SessionRecoveryData;
use crate::connection::tds_client::TdsClient;
use crate::connection::transport::any_transport::AnyTransport;
use crate::connection::transport::network_transport;
#[cfg(fuzzing)]
use crate::connection::transport::tds_transport::TdsTransport;
#[cfg(windows)]
use crate::core::EncryptionSetting;
use crate::core::{CancelHandle, TdsResult};
use crate::error::Error::{OperationCancelledError, TimeoutError};
use crate::error::{Error, SqlInfoMessage, TimeoutErrorType};
use crate::handler::handler_factory::HandlerFactory;
use crate::io::token_stream::GenericTokenParserRegistry;
use crate::ssrp;
use crate::token::tokens::SessionStateToken;

#[cfg(fuzzing)]
use crate::io::token_stream::TdsTokenStreamReader;

use std::sync::LazyLock;

pub(crate) static PARSER_REGISTRY: LazyLock<GenericTokenParserRegistry> =
    LazyLock::new(GenericTokenParserRegistry::default);

/// Redirects one login may follow before it fails.
const MAX_REDIRECTS: u32 = 10;

/// What a completed login yields, ready to become a [`TdsClient`].
type ConnectionParts = (
    AnyTransport,
    crate::handler::handler_factory::NegotiatedSettings,
    crate::connection::execution_context::ExecutionContext,
    Vec<SqlInfoMessage>,
    Vec<SessionStateToken>,
);

/// Factory for establishing TDS connections and producing [`TdsClient`] instances.
pub struct TdsConnectionProvider;

/// Where a diagnostic connection attempt ([`TdsConnectionProvider::diagnose_client`])
/// stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticStop {
    /// After the name or instance is resolved: SQL Server Browser asked, the
    /// TCP host resolved, a LocalDB instance's pipe found; before anything
    /// is connected to. A local named instance without a protocol prefix is
    /// tried over shared memory first (Windows), so there it stops before
    /// shared memory is opened, without a Browser lookup.
    Resolution,
    /// After the transport opened (a TCP connection, or a pipe), before
    /// pre-login. With strict encryption (TDS 8.0) the TLS handshake is part
    /// of opening the transport.
    Transport,
}

/// What a diagnostic connection attempt does beyond a normal one, and whether
/// it stopped, passed down the connect path as `Option<&Diagnosis>`: every
/// normal connection passes `None`.
pub(crate) struct Diagnosis {
    stop_after: Option<DiagnosticStop>,
    /// Whether the attempt stopped where it was asked to.
    stopped: AtomicBool,
    /// The connects to the addresses after the one used. Dropping the set, as
    /// canceling the attempt does, aborts them.
    extra_connects: Mutex<JoinSet<()>>,
}

impl Diagnosis {
    pub(crate) fn new(stop_after: Option<DiagnosticStop>) -> Self {
        Self {
            stop_after,
            stopped: AtomicBool::new(false),
            extra_connects: Mutex::new(JoinSet::new()),
        }
    }

    /// Fails, after noting it, when the attempt stops at `stop`. The error is
    /// not transient, so neither a retry nor another transport follows it.
    pub(crate) fn check_stop(&self, stop: DiagnosticStop) -> TdsResult<()> {
        if self.stops_at(stop) {
            Err(Self::stop_error())
        } else {
            Ok(())
        }
    }

    /// Whether the attempt stops at `stop`; if so, notes it. The caller then
    /// fails with [`Self::stop_error`].
    pub(crate) fn stops_at(&self, stop: DiagnosticStop) -> bool {
        let stops = self.stop_after == Some(stop);
        if stops {
            self.stopped.store(true, Ordering::Relaxed);
        }
        stops
    }

    /// The error a diagnostic stop unwinds the attempt with.
    pub(crate) fn stop_error() -> Error {
        OperationCancelledError("diagnostic stop".to_string())
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Runs the connect to an address after the one used, kept to wait for.
    pub(crate) fn spawn_extra_connect(&self, connect: impl Future<Output = ()> + Send + 'static) {
        self.extra_connects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .spawn(connect);
    }

    /// Waits for the connects to the other addresses, at most `bound_ms`
    /// (0: no bound); those still running then are aborted, as they are if
    /// this is dropped while waiting.
    pub(crate) async fn finish_extra_connects(&self, bound_ms: u64) {
        let mut connects = std::mem::take(
            &mut *self
                .extra_connects
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let all = async { while connects.join_next().await.is_some() {} };
        if bound_ms == 0 {
            all.await;
        } else {
            let _ = timeout(Duration::from_millis(bound_ms), all).await;
        }
    }
}

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
        let (
            transport,
            negotiated_settings,
            execution_context,
            info_messages,
            session_state_tokens,
        ) = Self::connect_with_transport(&context, &context.transport_context, transport).await?;
        let mut client = TdsClient::new(
            transport,
            negotiated_settings,
            execution_context,
            context,
            session_state_tokens,
        );
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
        let action_chain = Self::action_chain(&mut context, datasource)?;

        // ODBC polls this on the application's stack. Keep the connection
        // state out of its block_on frames, leaving room for native TLS and
        // client construction on musl's 128 KiB threads.
        Box::pin(self.execute_action_chain(&context, action_chain, cancel_handle, None)).await
    }

    /// [`create_client`](Self::create_client) for connection diagnostics: the
    /// same connection attempt, observed through its connect-stage spans
    /// ([`connect_stage`](crate::connection::connect_stage)), stopped after
    /// `stop_after` when given. A diagnostic attempt does not use or update
    /// the instance cache, so a named instance is looked up whenever the
    /// attempt reaches SQL Server Browser, and that lookup counts resolving
    /// the server against the lookup's time limit. As the client does, it
    /// tries a local named instance without a protocol prefix over shared
    /// memory first (Windows) and asks SQL Server Browser only if that does
    /// not connect; a [`DiagnosticStop::Resolution`] stop on that path
    /// happens before shared memory is opened, without a Browser lookup.
    /// Over TCP without MultiSubnetFailover, once an
    /// address accepts the connection, it also connects to the addresses
    /// after it (at most 64, as MultiSubnetFailover tries), at the same time
    /// and each within the connect timeout, closing each connection at once,
    /// so each address is reported; the connection used is the first one, so
    /// the login does not wait. With MultiSubnetFailover it cannot stop after
    /// resolution, since resolving is part of the parallel connect.
    ///
    /// Gives `Some(client)` once the attempt has logged in, `None` once it
    /// stopped where asked, and returns only when the connects to the other
    /// addresses have ended too (each is bounded by the connect timeout). To
    /// cancel it, drop the future.
    pub async fn diagnose_client(
        &self,
        mut context: ClientContext,
        datasource: &str,
        stop_after: Option<DiagnosticStop>,
    ) -> TdsResult<Option<TdsClient>> {
        let action_chain = Self::action_chain(&mut context, datasource)?;
        if stop_after == Some(DiagnosticStop::Resolution)
            && context
                .multi_subnet_failover_policy()
                .is_some_and(|policy| policy.enabled)
        {
            return Err(Error::UsageError(
                "A diagnostic attempt with MultiSubnetFailover cannot stop after resolution: \
                 resolving is part of the parallel connect."
                    .to_string(),
            ));
        }
        let diagnosis = Diagnosis::new(stop_after);
        let bound_ms = u64::from(context.connect_timeout) * 1000;
        let result =
            Box::pin(self.execute_action_chain(&context, action_chain, None, Some(&diagnosis)))
                .await;
        diagnosis.finish_extra_connects(bound_ms).await;
        if diagnosis.stopped() {
            return Ok(None);
        }
        result.map(Some)
    }

    /// Parses `datasource`, validates `context` for it, and gives the
    /// connection strategy.
    fn action_chain(
        context: &mut ClientContext,
        datasource: &str,
    ) -> TdsResult<ConnectionActionChain> {
        // Parse the datasource to get the action chain
        let parsed = context.prepare_datasource(datasource)?;

        // Validate the ClientContext before attempting connection
        context.validate()?;

        // Get connection timeout
        let timeout_ms = if context.connect_timeout > 0 {
            (context.connect_timeout as u64) * 1000
        } else {
            15000 // Default 15 seconds
        };

        // Generate the action chain
        let action_chain = parsed.to_connection_actions(timeout_ms);

        debug!("Connection strategy:\n{}", action_chain.describe());

        Ok(action_chain)
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
        diagnosis: Option<&Diagnosis>,
    ) -> TdsResult<TdsClient> {
        CancelHandle::run_until_cancelled(cancel_handle, async move {
            // Resolve SSRP (SQL Browser) if the action chain requires it.
            // Check the instance cache first to avoid a redundant UDP round-trip.
            let mut exec_context = ExecutionContext::new();
            let mut cache_key: Option<(String, String)> = None;

            // Compute the overall login deadline up front. It bounds the
            // shared-memory shortcut below and the connect/retry loop — the phases
            // that run the TDS/auth handshake. Name resolution is not covered by
            // this deadline: SSRP uses its own `ssrp_timeout_ms` (default 1s), and
            // LocalDB resolution, which starts an instance that is not running, has
            // no limit of its own and ends early only when the caller cancels the
            // connect or times it out. `login_timeout` falls back to
            // `connect_timeout` for callers that only set the historical single
            // knob; `0` means "no deadline".
            // `connect_timeout` still bounds each individual TCP-connect attempt,
            // so an unreachable host fails fast even when the login budget is large
            // (e.g. interactive sign-in).
            let login_timeout = context.login_timeout.unwrap_or(context.connect_timeout);
            let deadline = match login_timeout {
                1.. => Some(Instant::now() + Duration::from_secs(login_timeout.into())),
                _ => None,
            };
            let mut redirect_count = 0;

            // Try Shared Memory before SSRP for local named instances (Windows only).
            // SM doesn't need instance resolution — it uses the name directly.
            // If SM succeeds, we skip SSRP entirely. This matches ODBC/SNI behavior,
            // and a diagnostic connection follows it as the real connection does.
            #[cfg(windows)]
            if action_chain.requires_ssrp()
                && let Some(sm_transport) = action_chain.first_shared_memory_transport()
            {
                debug!("Trying Shared Memory before SSRP");
                // Bound the shared-memory attempt (which wraps the full TDS/auth
                // handshake) by the remaining login budget rather than
                // `connect_timeout`, so interactive sign-in against a local named
                // instance isn't cancelled after the default 15s.
                let timeout_duration =
                    deadline.map(|dl| dl.saturating_duration_since(Instant::now()));
                let connect_future =
                    Self::connect_with_transport_context(context, &sm_transport, None, diagnosis);
                let sm_result = match timeout_duration.as_ref() {
                    Some(duration) => match timeout(*duration, connect_future).await {
                        Ok(result) => result,
                        Err(_) => Err(TimeoutError(TimeoutErrorType::String(
                            "Timeout while connecting via Shared Memory".to_string(),
                        ))),
                    },
                    None => connect_future.await,
                };
                // A route received over shared memory is followed over TCP like
                // any other, instead of surfacing as a permanent failure below.
                match Self::follow_redirects(context, sm_result, deadline, &mut redirect_count, diagnosis).await {
                    Ok((transport, negotiated_settings, execution_context, info_messages, session_state_tokens)) => {
                        debug!("Shared Memory connection succeeded, skipping SSRP");
                        let mut client = TdsClient::new(
                            transport,
                            negotiated_settings,
                            execution_context,
                            context.clone(),
                            session_state_tokens,
                        );
                        client.extend_info_messages(info_messages);
                        return Ok(client);
                    }
                    Err(err) => {
                        // Only transient failures before routing fall through to
                        // SSRP/TCP. A definitive failure — e.g. the
                        // user cancelled interactive sign-in (`AuthenticationDenied`)
                        // or the login was rejected — would recur on every transport,
                        // and falling through would relaunch the interactive browser.
                        // Surface it immediately.
                        if redirect_count > 0 || !err.is_transient_connect_error() {
                            debug!(
                                "Shared Memory login or routed connection failed ({}), not falling through",
                                err
                            );
                            return Err(err);
                        }
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

                // A diagnostic connection that gets here asks SQL Server
                // Browser even on a cache hit, and does not leave its answer
                // in the cache.
                if action_chain.uses_cache() && diagnosis.is_none() {
                    if let Some(cached) = InstanceCache::global().get(server, instance)? {
                        debug!(?cached, server, instance, "SSRP cache hit");
                        exec_context.store_outcome(ActionOutcome::CacheHit {
                            protocol: crate::connection::datasource_parser::ProtocolType::Tcp,
                            port: cached.port,
                            pipe_path: cached.pipe_path,
                        });
                    } else {
                        debug!(server, instance, "SSRP cache miss");
                        Self::resolve_ssrp(&action_chain, &mut exec_context, context.ssrp_timeout_ms, diagnosis.is_some()).await?;
                        cache_key = Some((server.to_string(), instance.to_string()));
                    }
                } else {
                    Self::resolve_ssrp(&action_chain, &mut exec_context, context.ssrp_timeout_ms, diagnosis.is_some()).await?;
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

                    let connect_future = Self::connect_with_transport_context(&context, transport_ctx, None, diagnosis);

                    // Recompute remaining time budget before each timeout call
                    let remaining = deadline.map(|dl| dl.saturating_duration_since(Instant::now()));
                    let connection_result = match remaining {
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

                    match Self::follow_redirects(&context, connection_result, deadline, &mut redirect_count, diagnosis).await {
                        Ok((transport, negotiated_settings, execution_context, info_messages, session_state_tokens)) => {
                            debug!("Connection successful via action chain");
                            let mut client = TdsClient::new(
                                transport,
                                negotiated_settings,
                                execution_context,
                                context.clone(),
                                session_state_tokens,
                            );
                            client.extend_info_messages(info_messages);
                            return Ok(client);
                        }
                        Err(err) => {
                            debug!("Connection attempt failed: {}", err);

                            // A routed failure is terminal, as in msodbcsql's
                            // OpenOrRedirectConnection; do not restart the gateway.
                            if redirect_count > 0 || !err.is_transient_connect_error() {
                                return Err(err);
                            }

                            last_error = Some(err);
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
    /// A diagnostic attempt (`bound_resolution`) has resolving the server count
    /// against `ssrp_timeout_ms` too, so a slow resolver cannot outlast the
    /// limit reported for the lookup.
    async fn resolve_ssrp(
        action_chain: &ConnectionActionChain,
        exec_context: &mut ExecutionContext,
        ssrp_timeout_ms: u64,
        bound_resolution: bool,
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

        let stage = tracing::info_span!(
            target: connect_stage::TARGET,
            connect_stage::INSTANCE_LOOKUP,
            server = %server,
            instance = %instance,
            addresses = tracing::field::Empty,
            port = tracing::field::Empty,
            pipe = tracing::field::Empty,
            result = tracing::field::Empty,
            os_error = tracing::field::Empty,
            error = tracing::field::Empty,
            ok = tracing::field::Empty
        );
        let instance_info = ssrp::query_instance(
            &server,
            &instance,
            ssrp::SSRP_PORT,
            timeout_ms,
            bound_resolution,
            |addresses| {
                if !stage.is_disabled() {
                    stage.record(
                        connect_stage::ADDRESSES,
                        connect_stage::address_list(addresses),
                    );
                }
            },
        )
        .await
        .map_err(|lookup| {
            if !stage.is_disabled() {
                stage.record(connect_stage::RESULT, lookup.code());
                if let Some(code) = lookup.os_error() {
                    stage.record(connect_stage::OS_ERROR, code);
                }
                if let Some(detail) = lookup.detail() {
                    stage.record(connect_stage::ERROR, detail);
                }
                stage.record(connect_stage::OK, false);
            }
            let e = Error::from(lookup);
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
            let no_endpoint = pipe_path.is_none();
            #[cfg(not(windows))]
            let no_endpoint = true;
            if no_endpoint {
                stage.record(connect_stage::RESULT, connect_stage::NO_ENDPOINT);
                stage.record(connect_stage::OK, false);
            }

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
            stage.record(connect_stage::PORT, port);
            exec_context.store_outcome(ActionOutcome::SsrpResolved { port });
        }

        #[cfg(windows)]
        if let Some(pipe) = pipe_path {
            // Normalize for local connections: \\COMPUTERNAME\pipe\... → \\.\pipe\...
            // This avoids SMB round-trips and "Access is denied" errors.
            let pipe = crate::connection::transport::named_pipes::localize_pipe_path(&pipe);
            debug!(pipe = %pipe, "SSRP resolved named pipe path");
            stage.record(connect_stage::PIPE_PATH, pipe.as_str());
            exec_context.store_outcome(ActionOutcome::SsrpResolvedPipe { pipe_path: pipe });
        }

        stage.record(connect_stage::OK, true);
        Ok(())
    }

    /// Follows routing ENVCHANGEs until a login completes or fails for another
    /// reason. The shared-memory shortcut and the transport loop both hand
    /// their login here, so a route is honored whichever transport carried it
    /// (msodbcsql `OpenOrRedirectConnection`). `redirect_count` spans the
    /// entire login, including the shared-memory shortcut.
    async fn follow_redirects(
        context: &ClientContext,
        mut result: TdsResult<ConnectionParts>,
        deadline: Option<Instant>,
        redirect_count: &mut u32,
        diagnosis: Option<&Diagnosis>,
    ) -> TdsResult<ConnectionParts> {
        while let Err(Error::Redirection {
            host,
            port,
            database,
        }) = result
        {
            info!(
                "Redirection to: {:?}, {:?}, database: {:?}",
                host, port, database
            );
            *redirect_count += 1;
            if *redirect_count > MAX_REDIRECTS {
                return Err(Error::ProtocolError(
                    "Received more redirection tokens than expected.".to_string(),
                ));
            }

            let tcp_transport_context = TransportContext::from_routing_token(host, port);
            // Enhanced Routing names the database for this hop; plain routing
            // logs in to the original one (msodbcsql
            // `CRoutingInfo::UpdateLoginStruct`).
            let routed_context = database.map(|database| {
                let mut routed = Box::new(context.clone());
                routed.database = database;
                routed
            });
            let redirect_future = Self::connect_with_transport_context(
                routed_context.as_deref().unwrap_or(context),
                &tcp_transport_context,
                None,
                diagnosis,
            );
            // Recompute remaining time budget for redirected connect
            let remaining = deadline.map(|dl| dl.saturating_duration_since(Instant::now()));
            result = match remaining {
                Some(remaining) => match timeout(remaining, redirect_future).await {
                    Ok(result) => result,
                    Err(_) => Err(TimeoutError(TimeoutErrorType::String(
                        "Timeout while connecting".to_string(),
                    ))),
                },
                None => redirect_future.await,
            };
        }
        result
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
        diagnosis: Option<&Diagnosis>,
    ) -> TdsResult<ConnectionParts> {
        if let Some(trace) = &context.connection_trace {
            trace.begin_transport_attempt();
        }
        // Create network transport directly
        // Convert connect_timeout from seconds to milliseconds
        let connect_timeout_ms = (context.connect_timeout as u64) * 1000;
        let (mut transport, dns_resolution) = network_transport::create_transport(
            context.ipaddress_preference,
            context.tds_version(),
            transport_context,
            context.encryption_options.clone(),
            context.keep_alive_in_ms,
            context.keep_alive_interval_in_ms,
            context
                .multi_subnet_failover_policy()
                .ok_or_else(|| {
                    Error::ImplementationError(
                        "MSF policy was not resolved before connecting".to_string(),
                    )
                })?
                .enabled,
            connect_timeout_ms,
            diagnosis,
        )
        .await?;

        if diagnosis.is_some_and(|diagnosis| diagnosis.stops_at(DiagnosticStop::Transport)) {
            // Dropping the transport closes it at once; nothing was sent on it.
            drop(transport);
            return Err(Diagnosis::stop_error());
        }

        let factory = HandlerFactory {
            context: context.clone(),
            recovery_data,
            dns_resolution,
        };
        let session_result = factory
            .session_handler(transport_context)
            .execute(&mut transport)
            .await;

        match session_result {
            Ok((negotiated_settings, info_messages, session_state_tokens)) => {
                // Create execution context for the new connection
                let execution_context =
                    crate::connection::execution_context::ExecutionContext::new();

                Ok((
                    AnyTransport::network(transport),
                    negotiated_settings,
                    execution_context,
                    info_messages,
                    session_state_tokens,
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
        AnyTransport,
        crate::handler::handler_factory::NegotiatedSettings,
        crate::connection::execution_context::ExecutionContext,
        Vec<SqlInfoMessage>,
        Vec<SessionStateToken>,
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
            dns_resolution: None,
        };
        let session_result = factory
            .session_handler(transport_context)
            .execute(&mut transport)
            .await;

        match session_result {
            Ok((negotiated_settings, info_messages, session_state_tokens)) => {
                // Create execution context for the new connection
                let execution_context =
                    crate::connection::execution_context::ExecutionContext::new();

                Ok((
                    AnyTransport::dynamic(transport),
                    negotiated_settings,
                    execution_context,
                    info_messages,
                    session_state_tokens,
                ))
            }
            Err(err) => {
                let _ = transport.close_transport().await;
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn connection_future_stays_small() {
        const MAX: usize = 4096;
        let provider = TdsConnectionProvider::new();
        let context = ClientContext::default();
        let size = std::mem::size_of_val(&provider.create_client(context, "localhost", None));
        assert!(
            size <= MAX,
            "create_client future is {size} B, expected <= {MAX} B"
        );
    }

    #[test]
    fn transport_attempts_clear_stale_trace_ids_before_connecting() {
        use crate::trace_context::{self, ConnectionTrace, Context, Snapshot};
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::prelude::*;

        struct TransportEvents(Arc<Mutex<Vec<Snapshot>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for TransportEvents {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if event
                    .metadata()
                    .target()
                    .starts_with("mssql_tds::connection::transport")
                {
                    self.0.lock().unwrap().push(trace_context::snapshot());
                }
            }
        }

        trace_context::enable();
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(TransportEvents(Arc::clone(&events)));
        tracing::subscriber::with_default(subscriber, || {
            let trace = Arc::new(ConnectionTrace::default());
            let established = uuid::Uuid::new_v4();
            trace.establish(established);
            let _scope = Context::connection(Arc::clone(&trace)).enter();
            let expected_dbc = trace_context::snapshot().dbc;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut context = context_for_retry_test("127.0.0.1", port, 1, 0, 0);
            context
                .prepare_datasource(&format!("tcp:127.0.0.1,{port}"))
                .unwrap();
            context.set_connection_trace(Arc::clone(&trace));

            for _ in 0..2 {
                trace.begin_attempt(uuid::Uuid::new_v4());
                let context = context.clone();
                events.lock().unwrap().clear();
                let result =
                    runtime.block_on(TdsConnectionProvider::connect_with_transport_context(
                        &context,
                        &context.transport_context,
                        None,
                        None,
                    ));
                assert!(result.is_err());
                assert_eq!(trace.client_connection_id(), Some(established));
                assert_eq!(trace_context::snapshot().cid, None);
                let events = events.lock().unwrap();
                assert!(
                    !events.is_empty(),
                    "must observe the transport's connect events"
                );
                assert!(
                    events
                        .iter()
                        .all(|event| event.dbc == expected_dbc && event.cid.is_none())
                );
            }
        });
    }

    // ── Shared-memory fall-through policy ──

    /// Both arms of the `Err` branch in the shared-memory shortcut.
    ///
    /// That branch is Windows-only and needs a live named instance to reach
    /// end to end, so the policy it applies is asserted directly here: this is
    /// the decision that determines whether *every* local named-instance
    /// connection gets a second chance over TCP.
    #[cfg(windows)]
    #[test]
    fn shared_memory_falls_through_only_on_transient_failures() {
        use crate::security::SecurityError;

        // Falls through to SSRP/TCP. The shared-memory endpoint was absent or
        // unresponsive; TCP is a different path and may well succeed.
        for err in [
            Error::ConnectionError("shared memory endpoint not found".to_string()),
            Error::TimeoutError(TimeoutErrorType::String(
                "Timeout while connecting via Shared Memory".to_string(),
            )),
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such pipe",
            )),
        ] {
            assert!(
                err.is_transient_connect_error(),
                "should retry over TCP: {err}"
            );
        }

        // Surfaced immediately. The handshake got far enough to be refused, so
        // it would be refused identically over TCP — and retrying would raise a
        // second interactive sign-in prompt for a user who just cancelled one.
        for err in [
            Error::Security(SecurityError::AuthenticationDenied(
                "user cancelled the sign-in".to_string(),
            )),
            Error::ProtocolError("unexpected token in login response".to_string()),
        ] {
            assert!(
                !err.is_transient_connect_error(),
                "should not fall through: {err}"
            );
        }
    }

    // ── Diagnosis tests ──

    #[tokio::test]
    async fn dropping_the_diagnosis_aborts_the_extra_connects() {
        let diagnosis = Diagnosis::new(None);
        let (started, running) = tokio::sync::oneshot::channel::<()>();
        let (finished, done) = tokio::sync::oneshot::channel::<()>();
        diagnosis.spawn_extra_connect(async move {
            let _finished = finished;
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        running.await.unwrap();

        drop(diagnosis);

        // The task's end drops its sender, which completes `done` with an error.
        assert!(
            tokio::time::timeout(Duration::from_secs(2), done)
                .await
                .expect("the task was not aborted")
                .is_err()
        );
    }

    #[tokio::test]
    async fn canceling_the_wait_for_extra_connects_aborts_them() {
        let diagnosis = Diagnosis::new(None);
        let (finished, done) = tokio::sync::oneshot::channel::<()>();
        diagnosis.spawn_extra_connect(async move {
            let _finished = finished;
            std::future::pending::<()>().await;
        });

        let waiting = tokio::time::timeout(
            Duration::from_millis(50),
            diagnosis.finish_extra_connects(0),
        )
        .await;
        assert!(waiting.is_err(), "the wait has no bound of its own");

        assert!(
            tokio::time::timeout(Duration::from_secs(2), done)
                .await
                .expect("the task was left running")
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_wait_for_extra_connects_ends_at_its_bound_and_aborts_them() {
        let diagnosis = Diagnosis::new(None);
        let (finished, done) = tokio::sync::oneshot::channel::<()>();
        diagnosis.spawn_extra_connect(async move {
            let _finished = finished;
            std::future::pending::<()>().await;
        });

        tokio::time::timeout(Duration::from_secs(2), diagnosis.finish_extra_connects(50))
            .await
            .expect("the wait ends at its bound");

        assert!(
            tokio::time::timeout(Duration::from_secs(2), done)
                .await
                .expect("the task was left running")
                .is_err()
        );
    }

    #[tokio::test]
    async fn multi_subnet_failover_cannot_stop_after_resolution() {
        let context = ClientContext {
            multi_subnet_failover: Some(true),
            ..Default::default()
        };
        let error = TdsConnectionProvider::new()
            .diagnose_client(context, "tcp:127.0.0.1,1", Some(DiagnosticStop::Resolution))
            .await
            .expect_err("the combination is refused");
        assert!(matches!(error, Error::UsageError(_)), "{error}");
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

    // ── Diagnostic connections ──

    mod diagnostic {
        use super::*;
        use crate::connection::connect_stage::{self as stage, tests::StageLog};
        use std::future::Future;

        /// Runs `attempt` on a current-thread runtime, as connection
        /// diagnostics do, recording its connect stages.
        fn observe<F: Future>(attempt: impl FnOnce() -> F) -> (F::Output, StageLog) {
            let log = StageLog::default();
            let output = tracing::subscriber::with_default(log.clone(), || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(attempt())
            });
            (output, log)
        }

        fn context() -> ClientContext {
            let mut context = ClientContext {
                connect_timeout: 2,
                login_timeout: Some(2),
                connect_retry_count: 0,
                ssrp_timeout_ms: 300,
                // These tests follow the sequential connect, one stage per
                // address; MultiSubnetFailover is on by default otherwise.
                multi_subnet_failover: Some(false),
                ..Default::default()
            };
            context.encryption_options.mode = crate::core::EncryptionSetting::PreferOff;
            context
        }

        fn diagnose(
            datasource: &str,
            stop_after: Option<DiagnosticStop>,
        ) -> (TdsResult<Option<TdsClient>>, StageLog) {
            diagnose_with(context(), datasource, stop_after)
        }

        fn diagnose_with(
            context: ClientContext,
            datasource: &str,
            stop_after: Option<DiagnosticStop>,
        ) -> (TdsResult<Option<TdsClient>>, StageLog) {
            observe(|| async move {
                TdsConnectionProvider::new()
                    .diagnose_client(context, datasource, stop_after)
                    .await
            })
        }

        fn stopped(result: &TdsResult<Option<TdsClient>>) -> bool {
            matches!(result, Ok(None))
        }

        /// Accepts connections on 127.0.0.1 and closes each at once.
        fn closing_listener() -> (std::net::TcpListener, u16) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            (listener, port)
        }

        fn accepted(listener: &std::net::TcpListener) -> usize {
            std::iter::from_fn(|| listener.accept().ok()).count()
        }

        #[test]
        fn a_resolution_stop_connects_to_nothing() {
            let (listener, port) = closing_listener();
            let (result, log) = diagnose(
                &format!("tcp:127.0.0.1,{port}"),
                Some(DiagnosticStop::Resolution),
            );
            assert!(stopped(&result));
            let dns = log.named(stage::DNS);
            assert_eq!(dns.len(), 1);
            assert_eq!(dns[0].field(stage::HOST), Some("127.0.0.1"));
            assert_eq!(dns[0].field(stage::ADDRESSES), Some("127.0.0.1"));
            assert_eq!(dns[0].field(stage::OK), Some("true"));
            assert!(log.named(stage::TCP).is_empty(), "{:#?}", log.stages());
            assert_eq!(accepted(&listener), 0);
        }

        #[test]
        fn a_transport_stop_closes_the_connection_before_prelogin() {
            let (listener, port) = closing_listener();
            let (result, log) = diagnose(
                &format!("tcp:127.0.0.1,{port}"),
                Some(DiagnosticStop::Transport),
            );
            assert!(stopped(&result));
            let tcp = log.named(stage::TCP);
            assert_eq!(tcp.len(), 1, "{:#?}", log.stages());
            assert_eq!(tcp[0].field(stage::ADDRESS), Some("127.0.0.1"));
            assert_eq!(tcp[0].field(stage::PORT), Some(port.to_string().as_str()));
            assert_eq!(tcp[0].field(stage::OK), Some("true"));
            assert!(log.named(stage::PRELOGIN).is_empty(), "{:#?}", log.stages());
            assert_eq!(accepted(&listener), 1);
        }

        /// Without a stop, a server that closes the connection fails the
        /// pre-login, which records that it failed and with what.
        #[test]
        fn a_failed_stage_records_ok_false_and_its_error() {
            let (listener, port) = closing_listener();
            listener.set_nonblocking(false).unwrap();
            let server = std::thread::spawn(move || drop(listener.accept()));
            let (result, log) = diagnose(&format!("tcp:127.0.0.1,{port}"), None);
            server.join().unwrap();
            assert!(result.is_err());
            let prelogin = log.named(stage::PRELOGIN);
            assert_eq!(prelogin.len(), 1, "{:#?}", log.stages());
            assert_eq!(prelogin[0].field(stage::OK), Some("false"));
            assert!(prelogin[0].field(stage::ERROR).is_some());
            assert!(log.named(stage::LOGIN).is_empty());
            assert!(log.stages().iter().all(|s| s.closed));
        }

        /// A server that never answers: the time limit drops the pre-login,
        /// which closes without `ok`, abandoned rather than failed.
        #[test]
        fn a_stage_the_time_limit_drops_is_abandoned_not_failed() {
            let (_listener, port) = closing_listener();
            let (result, log) = diagnose(&format!("tcp:127.0.0.1,{port}"), None);
            assert!(matches!(result, Err(Error::TimeoutError(_))));
            let prelogin = log.named(stage::PRELOGIN);
            assert_eq!(prelogin.len(), 1, "{:#?}", log.stages());
            assert!(prelogin[0].closed);
            assert_eq!(prelogin[0].field(stage::OK), None, "{prelogin:#?}");
        }

        /// A port nothing listens on: the TCP stage records the refusal.
        #[test]
        fn a_refused_connect_records_what_the_operating_system_said() {
            let (listener, port) = closing_listener();
            drop(listener);
            // Windows retries a refused connect for about two seconds.
            let mut patient = context();
            patient.connect_timeout = 10;
            patient.login_timeout = Some(10);
            let (result, log) = diagnose_with(patient, &format!("tcp:127.0.0.1,{port}"), None);
            assert!(result.is_err());
            let tcp = &log.named(stage::TCP)[0];
            assert_eq!(tcp.field(stage::OK), Some("false"));
            assert_eq!(tcp.field(stage::IO_KIND), Some("ConnectionRefused"));
            assert!(tcp.field(stage::OS_ERROR).is_some());
            assert!(tcp.field(stage::TIMED_OUT).is_none());
        }

        /// Every address after the one connected to is tried too, each in its
        /// own `tcp` stage, while the connection goes on; a normal connection
        /// tries none of them.
        #[test]
        fn the_addresses_after_the_one_used_are_tried_in_their_own_stages() {
            let resolved: Vec<std::net::SocketAddr> =
                std::net::ToSocketAddrs::to_socket_addrs(&("localhost", 0))
                    .unwrap()
                    .collect();
            let listener = std::net::TcpListener::bind((resolved[0].ip(), 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let mut listeners = vec![listener];
            for address in &resolved[1..] {
                if let Ok(listener) = std::net::TcpListener::bind((address.ip(), port)) {
                    listeners.push(listener);
                }
            }
            if resolved.len() < 2 || listeners.len() < 2 {
                // This computer gives localhost one address: nothing to try.
                return;
            }
            let (result, log) = diagnose(
                &format!("tcp:localhost,{port}"),
                Some(DiagnosticStop::Transport),
            );
            assert!(stopped(&result));
            let tcp = log.named(stage::TCP);
            assert_eq!(tcp.len(), resolved.len(), "{:#?}", log.stages());
            for attempt in &tcp {
                assert_eq!(attempt.field(stage::OK), Some("true"), "{attempt:#?}");
                assert!(attempt.closed, "waited for: {attempt:#?}");
            }

            let (result, log) = observe(|| async move {
                TdsConnectionProvider::new()
                    .create_client(context(), &format!("tcp:localhost,{port}"), None)
                    .await
            });
            assert!(result.is_err());
            assert_eq!(log.named(stage::TCP).len(), 1, "{:#?}", log.stages());
        }

        /// The diagnostic lookup asks SQL Server Browser past the instance
        /// cache, and records why it failed; a normal connection
        /// fails with the same message as before.
        #[test]
        fn the_instance_lookup_is_recorded_and_its_message_kept() {
            let instance = "NoSuchDiagnoseInstance";
            InstanceCache::global()
                .insert("127.0.0.1", instance, Some(1), None)
                .unwrap();
            let datasource = format!("tcp:127.0.0.1\\{instance}");
            let (result, log) = diagnose(&datasource, Some(DiagnosticStop::Resolution));
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("the lookup cannot succeed: {:#?}", log.stages()),
            };
            let lookup = log.named(stage::INSTANCE_LOOKUP);
            assert_eq!(lookup.len(), 1, "{:#?}", log.stages());
            assert_eq!(lookup[0].field(stage::SERVER), Some("127.0.0.1"));
            assert_eq!(lookup[0].field(stage::INSTANCE), Some(instance));
            assert_eq!(lookup[0].field(stage::ADDRESSES), Some("127.0.0.1"));
            assert_eq!(lookup[0].field(stage::OK), Some("false"));
            assert!(lookup[0].field(stage::RESULT).is_some(), "{lookup:#?}");
            assert!(
                error.to_string().contains(&format!(
                    "Error Locating Server/Instance Specified [127.0.0.1\\{instance}]. \
                     Ensure the instance name is correct and SQL Server Browser service \
                     is running on the host. ("
                )),
                "{error}"
            );
        }

        /// A local named instance without a protocol prefix is tried over
        /// shared memory first, as the client does: a resolution stop comes
        /// before shared memory is opened and asks no Browser, and SQL Server
        /// Browser is asked only once shared memory has failed.
        #[cfg(windows)]
        #[test]
        fn a_local_named_instance_tries_shared_memory_before_the_browser() {
            let datasource = r".\NoSuchDiagnoseInstance";
            let (result, log) = diagnose(datasource, Some(DiagnosticStop::Resolution));
            assert!(stopped(&result), "{:#?}", log.stages());
            assert!(log.stages().is_empty(), "{:#?}", log.stages());

            for stop_after in [Some(DiagnosticStop::Transport), None] {
                let (result, log) = diagnose(datasource, stop_after);
                assert!(result.is_err(), "{stop_after:?}: {:#?}", log.stages());
                let stages = log.stages();
                let shared_memory = stages
                    .iter()
                    .position(|s| {
                        s.name == stage::PIPE
                            && s.field(stage::KIND) == Some(stage::KIND_SHARED_MEMORY)
                    })
                    .unwrap_or_else(|| panic!("{stop_after:?}: {stages:#?}"));
                assert_eq!(
                    stages[shared_memory].field(stage::OK),
                    Some("false"),
                    "{stop_after:?}: {stages:#?}"
                );
                let lookup = stages
                    .iter()
                    .position(|s| s.name == stage::INSTANCE_LOOKUP)
                    .unwrap_or_else(|| panic!("{stop_after:?}: {stages:#?}"));
                assert!(shared_memory < lookup, "{stop_after:?}: {stages:#?}");
                assert_eq!(
                    stages[lookup].field(stage::INSTANCE),
                    Some("NoSuchDiagnoseInstance")
                );
            }
        }

        /// A pipe stops before it is opened, and records how its opening
        /// failed.
        #[cfg(windows)]
        #[test]
        fn a_pipe_is_a_stage_of_its_own() {
            let pipe = r"\\.\pipe\NoSuchDiagnosePipe\sql\query";
            let (result, log) = diagnose(&format!("np:{pipe}"), Some(DiagnosticStop::Resolution));
            assert!(stopped(&result));
            assert!(log.stages().is_empty(), "{:#?}", log.stages());

            let (result, log) = diagnose(&format!("np:{pipe}"), None);
            assert!(result.is_err());
            let opened = log.named(stage::PIPE);
            assert_eq!(opened.len(), 1, "{:#?}", log.stages());
            assert_eq!(opened[0].field(stage::KIND), Some(stage::KIND_NAMED_PIPE));
            assert_eq!(opened[0].field(stage::PIPE_PATH), Some(pipe));
            assert_eq!(opened[0].field(stage::OK), Some("false"));
            assert_eq!(opened[0].field(stage::OS_ERROR), Some("2"));

            let (result, log) = diagnose("lpc:.", Some(DiagnosticStop::Resolution));
            assert!(stopped(&result));
            assert!(log.stages().is_empty(), "{:#?}", log.stages());
        }

        /// A normal connection is not stopped and tries no other address; a
        /// diagnosis stops only where it is asked to.
        #[test]
        fn a_normal_connection_is_not_diagnosed() {
            let (_listener, port) = closing_listener();
            let (result, log) = observe(|| async move {
                TdsConnectionProvider::new()
                    .create_client(context(), &format!("tcp:127.0.0.1,{port}"), None)
                    .await
            });
            assert!(result.is_err());
            assert_eq!(log.named(stage::TCP).len(), 1);
            assert_eq!(log.named(stage::PRELOGIN).len(), 1);

            let diagnosis = Diagnosis::new(Some(DiagnosticStop::Resolution));
            assert!(diagnosis.check_stop(DiagnosticStop::Transport).is_ok());
            assert!(!diagnosis.stopped());
            assert!(diagnosis.check_stop(DiagnosticStop::Resolution).is_err());
            assert!(diagnosis.stopped());
            assert!(
                Diagnosis::new(None)
                    .check_stop(DiagnosticStop::Transport)
                    .is_ok()
            );
        }
    }

    /// The provider must see the endpoint being dialled, not `context.transport_context`.
    #[tokio::test]
    async fn security_context_provider_sees_the_attempted_endpoint() {
        use crate::connection::client_context::TdsAuthenticationMethod;
        use crate::core::{EncryptionOptions, EncryptionSetting};
        use crate::security::mock::MockSecurityContext;
        use crate::security::{
            IntegratedAuthConfig, SecurityContext, SecurityContextProvider, SecurityError,
        };
        use mssql_mock_tds::MockTdsServer;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct RecordingProvider(Mutex<Vec<(String, u16, bool)>>);

        impl SecurityContextProvider for RecordingProvider {
            fn create_context(
                &self,
                config: &IntegratedAuthConfig,
                server: &str,
                port: u16,
            ) -> Result<Box<dyn SecurityContext>, SecurityError> {
                self.0
                    .lock()
                    .unwrap()
                    .push((server.to_string(), port, config.is_loopback));
                Ok(Box::new(MockSecurityContext::single_round(b"t".to_vec())))
            }
        }

        let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().port();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _ = server.run_with_shutdown(shutdown_rx).await;
        });

        let recorder = Arc::new(RecordingProvider::default());
        let mut context = ClientContext {
            tds_authentication_method: TdsAuthenticationMethod::SSPI,
            security_context_provider: Some(recorder.clone()),
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::PreferOff,
                trust_server_certificate: true,
                host_name_in_cert: None,
                server_certificate: None,
            },
            ..Default::default()
        };
        context
            .prepare_datasource("tcp:sql.contoso.com,1433")
            .unwrap();
        assert!(!context.transport_context.is_loopback());

        let attempted = TransportContext::Tcp {
            host: "127.0.0.1".to_string(),
            port,
            instance_name: None,
        };
        let result =
            TdsConnectionProvider::connect_with_transport_context(&context, &attempted, None, None)
                .await;
        assert!(result.is_ok(), "connect failed: {:?}", result.err());

        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![("127.0.0.1".to_string(), port, true)]
        );

        let _ = shutdown_tx.send(());
        let _ = handle.await;
    }
}
