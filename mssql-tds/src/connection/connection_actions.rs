// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection action chain for SQL Server connections
//!
#![allow(dead_code)]
//! This module implements an action-based approach to connection establishment,
//! converting parsed data sources into executable action sequences.

use crate::core::TdsResult;
use crate::error::Error;
use async_trait::async_trait;
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use super::datasource_parser::ProtocolType;

/// Represents a single action in the connection process
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionAction {
    /// Check connection cache for previously resolved connection info
    CheckCache { cache_key: String },

    /// Query SQL Server Browser (SSRP) to resolve instance port/details
    QuerySsrp {
        server: String,
        instance: String,
        /// Where to store the result (for next action)
        result_slot: ResultSlot,
    },

    /// Update connection cache with resolved information
    UpdateCache {
        cache_key: String,
        /// Port that was resolved
        port: u16,
    },

    /// Attempt TCP connection
    ConnectTcp {
        host: String,
        port: u16,
        timeout_ms: u64,
    },

    /// Attempt TCP connection using port from a result slot
    ConnectTcpFromSlot {
        host: String,
        port_slot: ResultSlot,
        timeout_ms: u64,
    },

    /// Attempt Named Pipe connection
    ConnectNamedPipe { pipe_path: String, timeout_ms: u64 },

    /// Attempt Named Pipe connection using path from a result slot
    ConnectNamedPipeFromSlot {
        path_slot: ResultSlot,
        timeout_ms: u64,
    },

    /// Attempt Shared Memory connection (Windows only)
    #[cfg(windows)]
    ConnectSharedMemory {
        instance_name: String,
        timeout_ms: u64,
    },

    /// Attempt Dedicated Admin Connection (DAC)
    ConnectDac { host: String, timeout_ms: u64 },

    /// Resolve LocalDB instance to Named Pipe path (Windows only)
    #[cfg(windows)]
    ResolveLocalDb {
        instance_name: String,
        result_slot: ResultSlot,
    },

    /// Try multiple connection actions in sequence (failover)
    /// Stops on first success unless fail_fast is false
    TrySequence {
        actions: Vec<ConnectionAction>,
        /// Stop on first success or continue through all
        fail_fast: bool,
    },

    /// Try multiple connection actions in parallel (MultiSubnetFailover)
    TryParallel {
        actions: Vec<ConnectionAction>,
        /// How many must succeed before accepting (typically 1)
        min_successes: usize,
    },
}

impl ConnectionAction {
    /// Get a human-readable description of this action
    pub fn describe(&self) -> String {
        match self {
            ConnectionAction::CheckCache { cache_key } => {
                format!("Check connection cache for '{}'", cache_key)
            }
            ConnectionAction::QuerySsrp {
                server, instance, ..
            } => {
                format!("Query SQL Browser for '{}\\{}'", server, instance)
            }
            ConnectionAction::UpdateCache { cache_key, port } => {
                format!("Update cache '{}' with port {}", cache_key, port)
            }
            ConnectionAction::ConnectTcp { host, port, .. } => {
                format!("Connect via TCP to {}:{}", host, port)
            }
            ConnectionAction::ConnectTcpFromSlot {
                host, port_slot, ..
            } => {
                format!("Connect via TCP to {} (port from {:?})", host, port_slot)
            }
            ConnectionAction::ConnectNamedPipe { pipe_path, .. } => {
                format!("Connect via Named Pipe to {}", pipe_path)
            }
            ConnectionAction::ConnectNamedPipeFromSlot { path_slot, .. } => {
                format!("Connect via Named Pipe (path from {:?})", path_slot)
            }
            #[cfg(windows)]
            ConnectionAction::ConnectSharedMemory { instance_name, .. } => {
                format!("Connect via Shared Memory to instance '{}'", instance_name)
            }
            ConnectionAction::ConnectDac { host, .. } => {
                format!("Connect via DAC to {}", host)
            }
            #[cfg(windows)]
            ConnectionAction::ResolveLocalDb { instance_name, .. } => {
                format!("Resolve LocalDB instance '{}'", instance_name)
            }
            ConnectionAction::TrySequence { actions, fail_fast } => {
                format!(
                    "Try {} actions in sequence (fail_fast={})",
                    actions.len(),
                    fail_fast
                )
            }
            ConnectionAction::TryParallel {
                actions,
                min_successes,
            } => {
                format!(
                    "Try {} actions in parallel (need {} successes)",
                    actions.len(),
                    min_successes
                )
            }
        }
    }
}

/// Slot for storing intermediate results between actions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResultSlot {
    /// Port resolved from SSRP
    ResolvedPort,
    /// Pipe path resolved from LocalDB
    ResolvedPipePath,
    /// Full connection info from cache
    CachedConnectionInfo,
}

/// Result of executing an action
#[derive(Debug)]
pub enum ActionResult {
    /// Action completed successfully
    Success(ActionOutcome),
    /// Action failed but process can continue (e.g., cache miss, protocol unavailable)
    Continue(String),
    /// Action failed and process should stop
    Failed(Error),
}

/// Outcome data from successful actions
#[derive(Debug, Clone)]
pub enum ActionOutcome {
    /// Cache hit with connection details
    CacheHit { protocol: ProtocolType, port: u16 },
    /// Cache miss
    CacheMiss,
    /// SSRP resolved port
    SsrpResolved { port: u16 },
    /// LocalDB resolved to pipe path
    #[cfg(windows)]
    LocalDbResolved { pipe_path: String },
    /// Connection established (marker for successful connection)
    Connected,
    /// Cache updated successfully
    CacheUpdated,
    /// No action needed
    NoOp,
}

/// Execution context for storing intermediate results
#[derive(Debug, Default, Clone)]
pub struct ExecutionContext {
    slots: HashMap<ResultSlot, ActionOutcome>,
    attempts: Vec<(String, Result<String, String>)>,
}

impl ExecutionContext {
    /// Create a new execution context
    pub fn new() -> Self {
        Self::default()
    }

    /// Store an action outcome in the appropriate slot
    pub fn store_outcome(&mut self, outcome: ActionOutcome) {
        match &outcome {
            ActionOutcome::SsrpResolved { .. } => {
                self.slots.insert(ResultSlot::ResolvedPort, outcome);
            }
            #[cfg(windows)]
            ActionOutcome::LocalDbResolved { .. } => {
                self.slots.insert(ResultSlot::ResolvedPipePath, outcome);
            }
            ActionOutcome::CacheHit { .. } => {
                self.slots.insert(ResultSlot::CachedConnectionInfo, outcome);
            }
            _ => {}
        }
    }

    /// Get an outcome from a result slot
    pub fn get_outcome(&self, slot: ResultSlot) -> Option<&ActionOutcome> {
        self.slots.get(&slot)
    }

    /// Get port from a slot (if available)
    pub fn get_port(&self, slot: ResultSlot) -> Option<u16> {
        match self.get_outcome(slot)? {
            ActionOutcome::SsrpResolved { port } => Some(*port),
            ActionOutcome::CacheHit { port, .. } => Some(*port),
            _ => None,
        }
    }

    /// Get pipe path from a slot (if available)
    #[cfg(windows)]
    pub fn get_pipe_path(&self, slot: ResultSlot) -> Option<String> {
        match self.get_outcome(slot)? {
            ActionOutcome::LocalDbResolved { pipe_path } => Some(pipe_path.clone()),
            _ => None,
        }
    }

    /// Record an action attempt
    pub fn record_attempt(&mut self, action_desc: String, result: Result<String, String>) {
        self.attempts.push((action_desc, result));
    }

    /// Get all recorded attempts
    pub fn attempts(&self) -> &[(String, Result<String, String>)] {
        &self.attempts
    }
}

/// Ordered sequence of actions to establish a connection
#[derive(Debug, Clone)]
pub struct ConnectionActionChain {
    actions: Vec<ConnectionAction>,
    metadata: ConnectionMetadata,
}

/// Metadata about the connection being established
#[derive(Debug, Clone)]
pub struct ConnectionMetadata {
    /// Original data source string
    pub source_string: String,
    /// Resolved server name
    pub server_name: String,
    /// Instance name (if any)
    pub instance_name: String,
    /// Whether protocol was explicitly specified
    pub explicit_protocol: bool,
    /// Connection timeout in milliseconds
    pub timeout_ms: u64,
}

impl ConnectionActionChain {
    /// Create a new connection action chain
    pub fn new(actions: Vec<ConnectionAction>, metadata: ConnectionMetadata) -> Self {
        Self { actions, metadata }
    }

    /// Get the actions in this chain
    pub fn actions(&self) -> &[ConnectionAction] {
        &self.actions
    }

    /// Get the metadata for this connection
    pub fn metadata(&self) -> &ConnectionMetadata {
        &self.metadata
    }

    /// Get a human-readable description of the connection strategy
    pub fn describe(&self) -> String {
        let mut desc = String::new();
        desc.push_str(&format!(
            "Connection strategy for '{}'\n",
            self.metadata.source_string
        ));
        desc.push_str(&format!("Server: {}\n", self.metadata.server_name));
        if !self.metadata.instance_name.is_empty() {
            desc.push_str(&format!("Instance: {}\n", self.metadata.instance_name));
        }
        desc.push_str(&format!(
            "Explicit protocol: {}\n\n",
            self.metadata.explicit_protocol
        ));
        desc.push_str("Action sequence:\n");
        for (i, action) in self.actions.iter().enumerate() {
            desc.push_str(&format!("{}. {}\n", i + 1, action.describe()));
        }
        desc
    }

    /// Get the number of actions in this chain
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Check if the chain is empty
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Resolve the action chain to a list of TransportContexts to try
    ///
    /// This method walks the action chain and extracts the transport contexts
    /// that should be attempted for connection, in order. This is useful for
    /// the simple case where we don't need SSRP or cache resolution.
    ///
    /// # Returns
    /// A vector of (TransportContext, timeout_ms) tuples to try in order
    pub fn resolve_transport_contexts(
        &self,
    ) -> Vec<(super::client_context::TransportContext, u64)> {
        let context = ExecutionContext::new();
        self.resolve_transport_contexts_with_context(&context)
    }

    /// Resolve transport contexts with a pre-populated execution context
    ///
    /// This is used when SSRP or cache lookups have already been performed
    /// and the resolved values are in the context.
    pub fn resolve_transport_contexts_with_context(
        &self,
        context: &ExecutionContext,
    ) -> Vec<(super::client_context::TransportContext, u64)> {
        let mut transports = Vec::new();
        Self::collect_transport_contexts(&self.actions, context, &mut transports);
        transports
    }

    /// Recursively collect transport contexts from actions
    fn collect_transport_contexts(
        actions: &[ConnectionAction],
        context: &ExecutionContext,
        result: &mut Vec<(super::client_context::TransportContext, u64)>,
    ) {
        for action in actions {
            match action {
                ConnectionAction::TrySequence { actions: inner, .. } => {
                    // Recursively collect from sequence
                    Self::collect_transport_contexts(inner, context, result);
                }
                ConnectionAction::TryParallel { actions: inner, .. } => {
                    // For parallel, we still need all transports (they'll be tried in parallel)
                    Self::collect_transport_contexts(inner, context, result);
                }
                ConnectionAction::ConnectTcp { timeout_ms, .. }
                | ConnectionAction::ConnectTcpFromSlot { timeout_ms, .. }
                | ConnectionAction::ConnectNamedPipe { timeout_ms, .. }
                | ConnectionAction::ConnectNamedPipeFromSlot { timeout_ms, .. }
                | ConnectionAction::ConnectDac { timeout_ms, .. } => {
                    if let Some(transport) = action.to_transport_context(context) {
                        result.push((transport, *timeout_ms));
                    }
                }
                #[cfg(windows)]
                ConnectionAction::ConnectSharedMemory { timeout_ms, .. } => {
                    if let Some(transport) = action.to_transport_context(context) {
                        result.push((transport, *timeout_ms));
                    }
                }
                // Skip non-connection actions
                ConnectionAction::CheckCache { .. }
                | ConnectionAction::QuerySsrp { .. }
                | ConnectionAction::UpdateCache { .. } => {}
                #[cfg(windows)]
                ConnectionAction::ResolveLocalDb { .. } => {}
            }
        }
    }

    /// Check if the action chain requires SSRP resolution
    ///
    /// Returns true if the chain contains a QuerySsrp action
    pub fn requires_ssrp(&self) -> bool {
        self.actions
            .iter()
            .any(|a| matches!(a, ConnectionAction::QuerySsrp { .. }))
    }

    /// Check if the action chain requires LocalDB resolution (Windows only)
    ///
    /// Returns Some(instance_name) if the chain contains a ResolveLocalDb action
    #[cfg(windows)]
    pub fn requires_localdb_resolution(&self) -> Option<String> {
        for action in &self.actions {
            if let ConnectionAction::ResolveLocalDb { instance_name, .. } = action {
                return Some(instance_name.clone());
            }
        }
        None
    }

    /// Check if the action chain uses caching
    ///
    /// Returns true if the chain contains a CheckCache action
    pub fn uses_cache(&self) -> bool {
        self.actions
            .iter()
            .any(|a| matches!(a, ConnectionAction::CheckCache { .. }))
    }
}

impl fmt::Display for ConnectionActionChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.describe())
    }
}

/// Result of executing an action chain - contains the transport context to use
#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    /// The transport context to use for connection
    pub transport_context: super::client_context::TransportContext,
    /// Port resolved (if any)
    pub resolved_port: Option<u16>,
}

impl ConnectionAction {
    /// Convert a connection action to a TransportContext if it's a connection action
    ///
    /// Returns Some(TransportContext) for connection actions like ConnectTcp, ConnectNamedPipe, etc.
    /// Returns None for non-connection actions like CheckCache, QuerySsrp, etc.
    pub fn to_transport_context(
        &self,
        context: &ExecutionContext,
    ) -> Option<super::client_context::TransportContext> {
        use super::client_context::TransportContext;

        match self {
            ConnectionAction::ConnectTcp { host, port, .. } => Some(TransportContext::Tcp {
                host: host.clone(),
                port: *port,
                instance_name: None,
            }),
            ConnectionAction::ConnectTcpFromSlot {
                host, port_slot, ..
            } => {
                let port = context.get_port(*port_slot)?;
                Some(TransportContext::Tcp {
                    host: host.clone(),
                    port,
                    instance_name: None,
                })
            }
            ConnectionAction::ConnectNamedPipe { pipe_path, .. } => {
                Some(TransportContext::NamedPipe {
                    pipe_name: pipe_path.clone(),
                })
            }
            #[cfg(windows)]
            ConnectionAction::ConnectNamedPipeFromSlot { path_slot, .. } => {
                let pipe_path = context.get_pipe_path(*path_slot)?;
                Some(TransportContext::NamedPipe {
                    pipe_name: pipe_path,
                })
            }
            #[cfg(not(windows))]
            ConnectionAction::ConnectNamedPipeFromSlot { .. } => None,
            #[cfg(windows)]
            ConnectionAction::ConnectSharedMemory { instance_name, .. } => {
                Some(TransportContext::SharedMemory {
                    instance_name: instance_name.clone(),
                })
            }
            ConnectionAction::ConnectDac { host, .. } => {
                // DAC uses TCP on port 1434 (SQL Browser port) by default
                // The actual DAC port is typically instance_port + 1 or a specific admin port
                Some(TransportContext::Tcp {
                    host: host.clone(),
                    port: 1434, // DAC default port
                    instance_name: None,
                })
            }
            // Non-connection actions
            ConnectionAction::CheckCache { .. }
            | ConnectionAction::QuerySsrp { .. }
            | ConnectionAction::UpdateCache { .. }
            | ConnectionAction::TrySequence { .. }
            | ConnectionAction::TryParallel { .. } => None,
            #[cfg(windows)]
            ConnectionAction::ResolveLocalDb { .. } => None,
        }
    }
}

/// Builder for constructing connection action chains
#[derive(Debug)]
pub struct ConnectionActionChainBuilder {
    actions: Vec<ConnectionAction>,
    metadata: ConnectionMetadata,
}

impl ConnectionActionChainBuilder {
    /// Create a new builder with metadata
    pub fn new(metadata: ConnectionMetadata) -> Self {
        Self {
            actions: Vec::new(),
            metadata,
        }
    }

    /// Add a cache check action
    pub fn add_check_cache(&mut self, cache_key: &str) -> &mut Self {
        self.actions.push(ConnectionAction::CheckCache {
            cache_key: cache_key.to_string(),
        });
        self
    }

    /// Add an SSRP query action
    pub fn add_ssrp_query(&mut self, server: &str, instance: &str) -> &mut Self {
        self.actions.push(ConnectionAction::QuerySsrp {
            server: server.to_string(),
            instance: instance.to_string(),
            result_slot: ResultSlot::ResolvedPort,
        });
        self
    }

    /// Add a cache update action
    pub fn add_update_cache(&mut self, cache_key: &str, port: u16) -> &mut Self {
        self.actions.push(ConnectionAction::UpdateCache {
            cache_key: cache_key.to_string(),
            port,
        });
        self
    }

    /// Add a TCP connection action
    pub fn add_connect_tcp(&mut self, host: &str, port: u16) -> &mut Self {
        self.actions.push(ConnectionAction::ConnectTcp {
            host: host.to_string(),
            port,
            timeout_ms: self.metadata.timeout_ms,
        });
        self
    }

    /// Add a TCP connection action using port from a slot
    pub fn add_connect_tcp_from_slot(&mut self, host: &str, port_slot: ResultSlot) -> &mut Self {
        self.actions.push(ConnectionAction::ConnectTcpFromSlot {
            host: host.to_string(),
            port_slot,
            timeout_ms: self.metadata.timeout_ms,
        });
        self
    }

    /// Add a Named Pipe connection action
    pub fn add_connect_named_pipe(&mut self, pipe_path: &str) -> &mut Self {
        self.actions.push(ConnectionAction::ConnectNamedPipe {
            pipe_path: pipe_path.to_string(),
            timeout_ms: self.metadata.timeout_ms,
        });
        self
    }

    /// Add a Named Pipe connection action using path from a slot
    pub fn add_connect_named_pipe_from_slot(&mut self, path_slot: ResultSlot) -> &mut Self {
        self.actions
            .push(ConnectionAction::ConnectNamedPipeFromSlot {
                path_slot,
                timeout_ms: self.metadata.timeout_ms,
            });
        self
    }

    /// Add a Shared Memory connection action (Windows only)
    #[cfg(windows)]
    pub fn add_connect_shared_memory(&mut self, instance_name: &str) -> &mut Self {
        self.actions.push(ConnectionAction::ConnectSharedMemory {
            instance_name: instance_name.to_string(),
            timeout_ms: self.metadata.timeout_ms,
        });
        self
    }

    /// Add a DAC connection action
    pub fn add_connect_dac(&mut self, host: &str) -> &mut Self {
        self.actions.push(ConnectionAction::ConnectDac {
            host: host.to_string(),
            timeout_ms: self.metadata.timeout_ms,
        });
        self
    }

    /// Add a LocalDB resolution action (Windows only)
    #[cfg(windows)]
    pub fn add_resolve_localdb(&mut self, instance_name: &str) -> &mut Self {
        self.actions.push(ConnectionAction::ResolveLocalDb {
            instance_name: instance_name.to_string(),
            result_slot: ResultSlot::ResolvedPipePath,
        });
        self
    }

    /// Add a protocol waterfall (try multiple protocols in sequence)
    #[allow(clippy::vec_init_then_push)]
    pub fn add_protocol_waterfall(&mut self, server: &str, _is_local: bool) -> &mut Self {
        let mut waterfall_actions = Vec::new();

        // 1. Shared Memory (Windows only, local connections)
        #[cfg(windows)]
        if _is_local {
            waterfall_actions.push(ConnectionAction::ConnectSharedMemory {
                instance_name: String::new(), // default instance
                timeout_ms: self.metadata.timeout_ms,
            });
        }

        // 2. TCP (always available)
        waterfall_actions.push(ConnectionAction::ConnectTcp {
            host: server.to_string(),
            port: 1433,
            timeout_ms: self.metadata.timeout_ms,
        });

        // 3. Named Pipes (Windows only)
        #[cfg(windows)]
        {
            let pipe_path = if _is_local {
                r"\\.\pipe\sql\query".to_string()
            } else {
                format!(r"\\{}\pipe\sql\query", server)
            };
            waterfall_actions.push(ConnectionAction::ConnectNamedPipe {
                pipe_path,
                timeout_ms: self.metadata.timeout_ms,
            });
        }

        self.actions.push(ConnectionAction::TrySequence {
            actions: waterfall_actions,
            fail_fast: false,
        });
        self
    }

    /// Add a parallel connection attempt (for MultiSubnetFailover)
    pub fn add_parallel_tcp_connect(&mut self, host: &str, port: u16) -> &mut Self {
        // For now, just add a single TCP connection
        // In a full implementation, this would resolve DNS and create
        // multiple parallel connection attempts
        self.actions.push(ConnectionAction::ConnectTcp {
            host: host.to_string(),
            port,
            timeout_ms: self.metadata.timeout_ms,
        });
        self
    }

    /// Add a custom action
    pub fn add_action(&mut self, action: ConnectionAction) -> &mut Self {
        self.actions.push(action);
        self
    }

    /// Build the final action chain
    pub fn build(self) -> ConnectionActionChain {
        ConnectionActionChain::new(self.actions, self.metadata)
    }
}

/// Information about a cached connection
#[derive(Debug, Clone)]
pub struct CachedConnectionInfo {
    pub protocol: ProtocolType,
    pub port: u16,
}

/// Response from SSRP (SQL Server Browser) query
#[derive(Debug, Clone)]
pub struct SsrpResponse {
    pub port: u16,
    pub server_name: String,
    pub instance_name: String,
}

/// Trait for executing connection actions
///
/// This trait abstracts the actual execution of connection actions,
/// allowing different implementations for production, testing, and fuzzing.
#[async_trait]
pub trait ConnectionExecutor {
    /// Execute a single connection action
    ///
    /// This method dispatches to the appropriate handler based on the action type.
    async fn execute_action(
        &mut self,
        action: &ConnectionAction,
        context: &mut ExecutionContext,
    ) -> TdsResult<ActionResult>;

    /// Check connection cache for previously resolved connection info
    async fn check_cache(&self, key: &str) -> Option<CachedConnectionInfo>;

    /// Query SQL Server Browser (SSRP) to resolve instance details
    async fn query_ssrp(&self, server: &str, instance: &str) -> TdsResult<SsrpResponse>;

    /// Update connection cache with resolved information
    async fn update_cache(&mut self, key: &str, info: CachedConnectionInfo) -> TdsResult<()>;

    /// Attempt TCP connection and return success/failure
    async fn connect_tcp(&self, host: &str, port: u16, timeout: Duration) -> TdsResult<()>;

    /// Attempt Named Pipe connection and return success/failure
    async fn connect_named_pipe(&self, pipe: &str, timeout: Duration) -> TdsResult<()>;

    /// Attempt Shared Memory connection (Windows only)
    #[cfg(windows)]
    async fn connect_shared_memory(&self, instance: &str, timeout: Duration) -> TdsResult<()>;

    /// Attempt DAC connection
    async fn connect_dac(&self, host: &str, timeout: Duration) -> TdsResult<()>;

    /// Resolve LocalDB instance to Named Pipe path (Windows only)
    #[cfg(windows)]
    async fn resolve_localdb(&self, instance: &str) -> TdsResult<String>;

    /// Default implementation for executing an action
    ///
    /// This provides the core dispatch logic that can be overridden if needed.
    async fn execute_action_default(
        &mut self,
        action: &ConnectionAction,
        context: &mut ExecutionContext,
    ) -> TdsResult<ActionResult> {
        match action {
            ConnectionAction::CheckCache { cache_key } => {
                if let Some(cached) = self.check_cache(cache_key).await {
                    Ok(ActionResult::Success(ActionOutcome::CacheHit {
                        protocol: cached.protocol,
                        port: cached.port,
                    }))
                } else {
                    Ok(ActionResult::Success(ActionOutcome::CacheMiss))
                }
            }

            ConnectionAction::QuerySsrp {
                server,
                instance,
                result_slot: _,
            } => match self.query_ssrp(server, instance).await {
                Ok(response) => {
                    let outcome = ActionOutcome::SsrpResolved {
                        port: response.port,
                    };
                    context.store_outcome(outcome.clone());
                    Ok(ActionResult::Success(outcome))
                }
                Err(e) => Ok(ActionResult::Continue(format!("SSRP query failed: {}", e))),
            },

            ConnectionAction::UpdateCache { cache_key, port } => {
                // Get port from context if it's 0 (placeholder)
                let actual_port = if *port == 0 {
                    context.get_port(ResultSlot::ResolvedPort).unwrap_or(*port)
                } else {
                    *port
                };

                let info = CachedConnectionInfo {
                    protocol: ProtocolType::Tcp,
                    port: actual_port,
                };
                match self.update_cache(cache_key, info).await {
                    Ok(_) => Ok(ActionResult::Success(ActionOutcome::CacheUpdated)),
                    Err(e) => Ok(ActionResult::Continue(format!(
                        "Cache update failed: {}",
                        e
                    ))),
                }
            }

            ConnectionAction::ConnectTcp {
                host,
                port,
                timeout_ms,
            } => {
                let timeout = Duration::from_millis(*timeout_ms);
                match self.connect_tcp(host, *port, timeout).await {
                    Ok(_) => Ok(ActionResult::Success(ActionOutcome::Connected)),
                    Err(e) => Ok(ActionResult::Continue(format!(
                        "TCP connection to {}:{} failed: {}",
                        host, port, e
                    ))),
                }
            }

            ConnectionAction::ConnectTcpFromSlot {
                host,
                port_slot,
                timeout_ms,
            } => {
                let port = context.get_port(*port_slot).ok_or_else(|| {
                    Error::ProtocolError(format!("No port found in slot {:?}", port_slot))
                })?;
                let timeout = Duration::from_millis(*timeout_ms);
                match self.connect_tcp(host, port, timeout).await {
                    Ok(_) => Ok(ActionResult::Success(ActionOutcome::Connected)),
                    Err(e) => Ok(ActionResult::Continue(format!(
                        "TCP connection to {}:{} failed: {}",
                        host, port, e
                    ))),
                }
            }

            ConnectionAction::ConnectNamedPipe {
                pipe_path,
                timeout_ms,
            } => {
                let timeout = Duration::from_millis(*timeout_ms);
                match self.connect_named_pipe(pipe_path, timeout).await {
                    Ok(_) => Ok(ActionResult::Success(ActionOutcome::Connected)),
                    Err(e) => Ok(ActionResult::Continue(format!(
                        "Named Pipe connection to {} failed: {}",
                        pipe_path, e
                    ))),
                }
            }

            ConnectionAction::ConnectNamedPipeFromSlot {
                path_slot: _path_slot,
                timeout_ms: _timeout_ms,
            } => {
                #[cfg(windows)]
                {
                    let pipe_path = context.get_pipe_path(*_path_slot).ok_or_else(|| {
                        Error::ProtocolError(format!("No pipe path found in slot {:?}", _path_slot))
                    })?;
                    let timeout = Duration::from_millis(*_timeout_ms);
                    match self.connect_named_pipe(&pipe_path, timeout).await {
                        Ok(_) => Ok(ActionResult::Success(ActionOutcome::Connected)),
                        Err(e) => Ok(ActionResult::Continue(format!(
                            "Named Pipe connection to {} failed: {}",
                            pipe_path, e
                        ))),
                    }
                }
                #[cfg(not(windows))]
                {
                    Ok(ActionResult::Continue(
                        "Named Pipes not supported on this platform".to_string(),
                    ))
                }
            }

            #[cfg(windows)]
            ConnectionAction::ConnectSharedMemory {
                instance_name,
                timeout_ms,
            } => {
                let timeout = Duration::from_millis(*timeout_ms);
                match self.connect_shared_memory(instance_name, timeout).await {
                    Ok(_) => Ok(ActionResult::Success(ActionOutcome::Connected)),
                    Err(e) => Ok(ActionResult::Continue(format!(
                        "Shared Memory connection to instance '{}' failed: {}",
                        instance_name, e
                    ))),
                }
            }

            ConnectionAction::ConnectDac { host, timeout_ms } => {
                let timeout = Duration::from_millis(*timeout_ms);
                match self.connect_dac(host, timeout).await {
                    Ok(_) => Ok(ActionResult::Success(ActionOutcome::Connected)),
                    Err(e) => Ok(ActionResult::Continue(format!(
                        "DAC connection to {} failed: {}",
                        host, e
                    ))),
                }
            }

            #[cfg(windows)]
            ConnectionAction::ResolveLocalDb {
                instance_name,
                result_slot: _,
            } => match self.resolve_localdb(instance_name).await {
                Ok(pipe_path) => {
                    let outcome = ActionOutcome::LocalDbResolved { pipe_path };
                    context.store_outcome(outcome.clone());
                    Ok(ActionResult::Success(outcome))
                }
                Err(e) => Ok(ActionResult::Failed(e)),
            },

            ConnectionAction::TrySequence { actions, fail_fast } => {
                for action in actions {
                    match self.execute_action(action, context).await? {
                        ActionResult::Success(ActionOutcome::Connected) => {
                            return Ok(ActionResult::Success(ActionOutcome::Connected));
                        }
                        ActionResult::Continue(msg) if !fail_fast => {
                            context.record_attempt(action.describe(), Err(msg));
                            continue;
                        }
                        ActionResult::Continue(msg) => {
                            return Ok(ActionResult::Continue(msg));
                        }
                        ActionResult::Failed(e) => {
                            return Ok(ActionResult::Failed(e));
                        }
                        ActionResult::Success(outcome) => {
                            context.store_outcome(outcome);
                        }
                    }
                }
                Ok(ActionResult::Continue(
                    "All sequence actions failed".to_string(),
                ))
            }

            ConnectionAction::TryParallel {
                actions,
                min_successes,
            } => {
                // For now, just try sequentially
                // TODO: Implement true parallel execution
                let mut successes = 0;
                for action in actions {
                    match self.execute_action(action, context).await? {
                        ActionResult::Success(ActionOutcome::Connected) => {
                            successes += 1;
                            if successes >= *min_successes {
                                return Ok(ActionResult::Success(ActionOutcome::Connected));
                            }
                        }
                        _ => continue,
                    }
                }
                Ok(ActionResult::Continue(format!(
                    "Parallel actions failed: only {} of {} succeeded (needed {})",
                    successes,
                    actions.len(),
                    min_successes
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execution_context_store_retrieve() {
        let mut ctx = ExecutionContext::new();

        // Store SSRP result
        ctx.store_outcome(ActionOutcome::SsrpResolved { port: 54321 });

        // Retrieve port
        assert_eq!(ctx.get_port(ResultSlot::ResolvedPort), Some(54321));

        // Cache hit
        ctx.store_outcome(ActionOutcome::CacheHit {
            protocol: ProtocolType::Tcp,
            port: 1433,
        });
        assert_eq!(ctx.get_port(ResultSlot::CachedConnectionInfo), Some(1433));
    }

    #[test]
    fn test_action_chain_builder() {
        let metadata = ConnectionMetadata {
            source_string: "myserver\\SQLEXPRESS".to_string(),
            server_name: "myserver".to_string(),
            instance_name: "SQLEXPRESS".to_string(),
            explicit_protocol: false,
            timeout_ms: 15000,
        };

        let mut builder = ConnectionActionChainBuilder::new(metadata);
        builder
            .add_check_cache("myserver\\SQLEXPRESS")
            .add_ssrp_query("myserver", "SQLEXPRESS")
            .add_update_cache("myserver\\SQLEXPRESS", 54321)
            .add_connect_tcp_from_slot("myserver", ResultSlot::ResolvedPort);
        let chain = builder.build();

        assert_eq!(chain.len(), 4);
        assert!(matches!(
            chain.actions()[0],
            ConnectionAction::CheckCache { .. }
        ));
        assert!(matches!(
            chain.actions()[1],
            ConnectionAction::QuerySsrp { .. }
        ));
        assert!(matches!(
            chain.actions()[2],
            ConnectionAction::UpdateCache { .. }
        ));
        assert!(matches!(
            chain.actions()[3],
            ConnectionAction::ConnectTcpFromSlot { .. }
        ));
    }

    #[test]
    fn test_action_describe() {
        let action = ConnectionAction::ConnectTcp {
            host: "myserver".to_string(),
            port: 1433,
            timeout_ms: 15000,
        };
        assert_eq!(action.describe(), "Connect via TCP to myserver:1433");

        let action = ConnectionAction::QuerySsrp {
            server: "myserver".to_string(),
            instance: "SQLEXPRESS".to_string(),
            result_slot: ResultSlot::ResolvedPort,
        };
        assert_eq!(
            action.describe(),
            "Query SQL Browser for 'myserver\\SQLEXPRESS'"
        );
    }

    #[test]
    fn test_resolve_transport_contexts_simple_tcp() {
        use crate::connection::client_context::TransportContext;

        let metadata = ConnectionMetadata {
            source_string: "tcp:myserver,1433".to_string(),
            server_name: "myserver".to_string(),
            instance_name: String::new(),
            explicit_protocol: true,
            timeout_ms: 15000,
        };

        let mut builder = ConnectionActionChainBuilder::new(metadata);
        builder.add_connect_tcp("myserver", 1433);
        let chain = builder.build();

        let transports = chain.resolve_transport_contexts();

        assert_eq!(transports.len(), 1);
        assert!(matches!(
            &transports[0].0,
            TransportContext::Tcp { host, port, .. } if host == "myserver" && *port == 1433
        ));
        assert_eq!(transports[0].1, 15000);
    }

    #[test]
    fn test_resolve_transport_contexts_waterfall() {
        use crate::connection::client_context::TransportContext;

        let metadata = ConnectionMetadata {
            source_string: "myserver".to_string(),
            server_name: "myserver".to_string(),
            instance_name: String::new(),
            explicit_protocol: false,
            timeout_ms: 15000,
        };

        let mut builder = ConnectionActionChainBuilder::new(metadata);
        builder.add_protocol_waterfall("myserver", false);
        let chain = builder.build();

        let transports = chain.resolve_transport_contexts();

        // Should have at least TCP in the waterfall
        assert!(!transports.is_empty());

        // First transport should be TCP (on non-local, no shared memory)
        let has_tcp = transports.iter().any(|(t, _)| {
            matches!(t, TransportContext::Tcp { host, port, .. } if host == "myserver" && *port == 1433)
        });
        assert!(has_tcp, "Waterfall should include TCP transport");
    }

    #[test]
    fn test_requires_ssrp() {
        let metadata = ConnectionMetadata {
            source_string: "myserver\\SQLEXPRESS".to_string(),
            server_name: "myserver".to_string(),
            instance_name: "SQLEXPRESS".to_string(),
            explicit_protocol: false,
            timeout_ms: 15000,
        };

        // Chain with SSRP
        let mut builder = ConnectionActionChainBuilder::new(metadata.clone());
        builder
            .add_check_cache("myserver\\SQLEXPRESS")
            .add_ssrp_query("myserver", "SQLEXPRESS")
            .add_connect_tcp_from_slot("myserver", ResultSlot::ResolvedPort);
        let chain = builder.build();

        assert!(chain.requires_ssrp());
        assert!(chain.uses_cache());

        // Chain without SSRP (explicit port)
        let mut builder = ConnectionActionChainBuilder::new(metadata);
        builder.add_connect_tcp("myserver", 1433);
        let chain = builder.build();

        assert!(!chain.requires_ssrp());
        assert!(!chain.uses_cache());
    }

    #[test]
    fn test_to_transport_context() {
        use crate::connection::client_context::TransportContext;

        let ctx = ExecutionContext::new();

        // TCP action
        let action = ConnectionAction::ConnectTcp {
            host: "myserver".to_string(),
            port: 1433,
            timeout_ms: 15000,
        };
        let transport = action.to_transport_context(&ctx);
        assert!(matches!(
            transport,
            Some(TransportContext::Tcp { host, port, .. }) if host == "myserver" && port == 1433
        ));

        // Named pipe action
        let action = ConnectionAction::ConnectNamedPipe {
            pipe_path: r"\\myserver\pipe\sql\query".to_string(),
            timeout_ms: 15000,
        };
        let transport = action.to_transport_context(&ctx);
        assert!(matches!(
            transport,
            Some(TransportContext::NamedPipe { pipe_name }) if pipe_name == r"\\myserver\pipe\sql\query"
        ));

        // Non-connection action should return None
        let action = ConnectionAction::CheckCache {
            cache_key: "test".to_string(),
        };
        let transport = action.to_transport_context(&ctx);
        assert!(transport.is_none());
    }

    #[test]
    fn describe_all_action_types() {
        let check = ConnectionAction::CheckCache {
            cache_key: "k".to_string(),
        };
        assert!(check.describe().contains("cache"));

        let ssrp = ConnectionAction::QuerySsrp {
            server: "s".to_string(),
            instance: "i".to_string(),
            result_slot: ResultSlot::ResolvedPort,
        };
        assert!(ssrp.describe().contains("SQL Browser"));

        let update = ConnectionAction::UpdateCache {
            cache_key: "k".to_string(),
            port: 1433,
        };
        assert!(update.describe().contains("Update cache"));

        let slot = ConnectionAction::ConnectTcpFromSlot {
            host: "h".to_string(),
            port_slot: ResultSlot::ResolvedPort,
            timeout_ms: 100,
        };
        assert!(slot.describe().contains("TCP"));

        let pipe = ConnectionAction::ConnectNamedPipe {
            pipe_path: "p".to_string(),
            timeout_ms: 100,
        };
        assert!(pipe.describe().contains("Named Pipe"));

        let pipe_slot = ConnectionAction::ConnectNamedPipeFromSlot {
            path_slot: ResultSlot::ResolvedPipePath,
            timeout_ms: 100,
        };
        assert!(pipe_slot.describe().contains("Named Pipe"));

        let dac = ConnectionAction::ConnectDac {
            host: "h".to_string(),
            timeout_ms: 100,
        };
        assert!(dac.describe().contains("DAC"));

        let seq = ConnectionAction::TrySequence {
            actions: vec![],
            fail_fast: true,
        };
        assert!(seq.describe().contains("sequence"));

        let par = ConnectionAction::TryParallel {
            actions: vec![],
            min_successes: 1,
        };
        assert!(par.describe().contains("parallel"));
    }

    #[test]
    fn execution_context_record_and_get_attempts() {
        let mut ctx = ExecutionContext::new();
        ctx.record_attempt("action1".to_string(), Ok("success".to_string()));
        ctx.record_attempt("action2".to_string(), Err("failed".to_string()));
        assert_eq!(ctx.attempts().len(), 2);
    }

    #[test]
    fn execution_context_get_port_returns_none_for_missing_slot() {
        let ctx = ExecutionContext::new();
        assert!(ctx.get_port(ResultSlot::ResolvedPort).is_none());
    }

    #[test]
    fn execution_context_get_port_returns_none_for_wrong_outcome() {
        let mut ctx = ExecutionContext::new();
        ctx.store_outcome(ActionOutcome::CacheMiss);
        assert!(ctx.get_port(ResultSlot::ResolvedPort).is_none());
    }

    #[test]
    fn chain_describe_includes_metadata() {
        let metadata = ConnectionMetadata {
            source_string: "tcp:myserver,1433".to_string(),
            server_name: "myserver".to_string(),
            instance_name: "inst".to_string(),
            explicit_protocol: true,
            timeout_ms: 5000,
        };
        let mut builder = ConnectionActionChainBuilder::new(metadata);
        builder.add_connect_tcp("myserver", 1433);
        let chain = builder.build();
        let desc = chain.describe();
        assert!(desc.contains("myserver"));
        assert!(desc.contains("inst"));
        assert!(desc.contains("Explicit protocol: true"));
    }

    #[test]
    fn chain_display_trait() {
        let metadata = ConnectionMetadata {
            source_string: "srv".to_string(),
            server_name: "srv".to_string(),
            instance_name: String::new(),
            explicit_protocol: false,
            timeout_ms: 5000,
        };
        let mut builder = ConnectionActionChainBuilder::new(metadata);
        builder.add_connect_tcp("srv", 1433);
        let chain = builder.build();
        let display = format!("{chain}");
        assert!(display.contains("srv"));
    }

    #[test]
    fn chain_is_empty() {
        let metadata = ConnectionMetadata {
            source_string: "srv".to_string(),
            server_name: "srv".to_string(),
            instance_name: String::new(),
            explicit_protocol: false,
            timeout_ms: 5000,
        };
        let builder = ConnectionActionChainBuilder::new(metadata);
        let chain = builder.build();
        assert!(chain.is_empty());
    }

    #[test]
    fn to_transport_context_tcp_from_slot_with_context() {
        use crate::connection::client_context::TransportContext;

        let mut ctx = ExecutionContext::new();
        ctx.store_outcome(ActionOutcome::SsrpResolved { port: 54321 });

        let action = ConnectionAction::ConnectTcpFromSlot {
            host: "myserver".to_string(),
            port_slot: ResultSlot::ResolvedPort,
            timeout_ms: 15000,
        };
        let transport = action.to_transport_context(&ctx);
        assert!(matches!(
            transport,
            Some(TransportContext::Tcp { port: 54321, .. })
        ));
    }

    #[test]
    fn to_transport_context_tcp_from_slot_no_context() {
        let ctx = ExecutionContext::new();
        let action = ConnectionAction::ConnectTcpFromSlot {
            host: "myserver".to_string(),
            port_slot: ResultSlot::ResolvedPort,
            timeout_ms: 15000,
        };
        assert!(action.to_transport_context(&ctx).is_none());
    }

    #[test]
    fn to_transport_context_dac() {
        use crate::connection::client_context::TransportContext;

        let ctx = ExecutionContext::new();
        let action = ConnectionAction::ConnectDac {
            host: "myserver".to_string(),
            timeout_ms: 15000,
        };
        let transport = action.to_transport_context(&ctx);
        assert!(matches!(
            transport,
            Some(TransportContext::Tcp { port: 1434, .. })
        ));
    }

    #[test]
    fn to_transport_context_non_connection_actions() {
        let ctx = ExecutionContext::new();
        let actions = vec![
            ConnectionAction::QuerySsrp {
                server: "s".to_string(),
                instance: "i".to_string(),
                result_slot: ResultSlot::ResolvedPort,
            },
            ConnectionAction::UpdateCache {
                cache_key: "k".to_string(),
                port: 1433,
            },
            ConnectionAction::TrySequence {
                actions: vec![],
                fail_fast: true,
            },
            ConnectionAction::TryParallel {
                actions: vec![],
                min_successes: 1,
            },
        ];
        for action in &actions {
            assert!(action.to_transport_context(&ctx).is_none());
        }
    }

    #[test]
    fn resolve_transport_contexts_nested_sequence() {
        let metadata = ConnectionMetadata {
            source_string: "srv".to_string(),
            server_name: "srv".to_string(),
            instance_name: String::new(),
            explicit_protocol: false,
            timeout_ms: 5000,
        };
        let chain = ConnectionActionChain::new(
            vec![ConnectionAction::TrySequence {
                actions: vec![ConnectionAction::ConnectTcp {
                    host: "srv".to_string(),
                    port: 1433,
                    timeout_ms: 5000,
                }],
                fail_fast: false,
            }],
            metadata,
        );
        let transports = chain.resolve_transport_contexts();
        assert_eq!(transports.len(), 1);
    }

    #[test]
    fn resolve_transport_contexts_parallel() {
        let metadata = ConnectionMetadata {
            source_string: "srv".to_string(),
            server_name: "srv".to_string(),
            instance_name: String::new(),
            explicit_protocol: false,
            timeout_ms: 5000,
        };
        let chain = ConnectionActionChain::new(
            vec![ConnectionAction::TryParallel {
                actions: vec![
                    ConnectionAction::ConnectTcp {
                        host: "srv1".to_string(),
                        port: 1433,
                        timeout_ms: 5000,
                    },
                    ConnectionAction::ConnectTcp {
                        host: "srv2".to_string(),
                        port: 1433,
                        timeout_ms: 5000,
                    },
                ],
                min_successes: 1,
            }],
            metadata,
        );
        let transports = chain.resolve_transport_contexts();
        assert_eq!(transports.len(), 2);
    }
}
