// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock TDS Server implementation

use crate::protocol::{
    PACKET_HEADER_SIZE, PacketHeader, PacketType, ProtocolError, build_done_token,
    build_error_response, build_feature_ext_ack_fedauth, build_login_ack, build_prelogin_response,
    build_prelogin_response_with_fedauth, build_query_result, build_routing_response,
    parse_login7_auth, parse_sql_batch,
};
use crate::query_response::QueryRegistry;
use bytes::BytesMut;
use native_tls::Identity;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_native_tls::{TlsAcceptor, TlsStream};
use tracing::{debug, error, info, warn};

/// Configuration for connection redirection
///
/// When set, the server will redirect clients to a different endpoint
/// during the login phase instead of completing authentication.
#[derive(Debug, Clone)]
pub struct RedirectionConfig {
    /// The hostname to redirect clients to
    pub redirect_host: String,
    /// The port to redirect clients to
    pub redirect_port: u16,
}

impl RedirectionConfig {
    /// Create a new redirection configuration
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            redirect_host: host.into(),
            redirect_port: port,
        }
    }
}

/// Per-connection processor that tracks all connection-specific state.
/// Each connection gets its own processor instance.
/// FedAuth and username/password authentication are always supported.
pub struct ConnectionProcessor {
    /// Client socket address
    addr: SocketAddr,
    /// Whether the client has authenticated
    is_authenticated: bool,
    /// Access token received during FedAuth authentication (if any)
    received_token: Option<Vec<u8>>,
    /// Reference to the shared query registry
    query_registry: Arc<Mutex<QueryRegistry>>,
    /// Packet buffer for this connection
    buffer: BytesMut,
    /// Optional redirection configuration
    redirection: Option<RedirectionConfig>,
}

impl ConnectionProcessor {
    /// Create a new connection processor
    pub fn new(addr: SocketAddr, query_registry: Arc<Mutex<QueryRegistry>>) -> Self {
        Self {
            addr,
            is_authenticated: false,
            received_token: None,
            query_registry,
            buffer: BytesMut::with_capacity(4096),
            redirection: None,
        }
    }

    /// Create a new connection processor with redirection configuration
    pub fn new_with_redirection(
        addr: SocketAddr,
        query_registry: Arc<Mutex<QueryRegistry>>,
        redirection: Option<RedirectionConfig>,
    ) -> Self {
        Self {
            addr,
            is_authenticated: false,
            received_token: None,
            query_registry,
            buffer: BytesMut::with_capacity(4096),
            redirection,
        }
    }

    /// Get the client address
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Check if the client is authenticated
    pub fn is_authenticated(&self) -> bool {
        self.is_authenticated
    }

    /// Get the received access token (raw bytes)
    pub fn received_token(&self) -> Option<&[u8]> {
        self.received_token.as_deref()
    }

    /// Get the received access token as a UTF-16LE decoded string
    pub fn received_token_as_string(&self) -> Option<String> {
        self.received_token.as_ref().and_then(|bytes| {
            // Access tokens in TDS are UTF-16LE encoded
            if bytes.len() % 2 == 0 {
                let u16_chars: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .collect();
                String::from_utf16(&u16_chars).ok()
            } else {
                // Fallback to UTF-8 if not even length
                String::from_utf8(bytes.to_vec()).ok()
            }
        })
    }

    /// Get mutable access to the buffer for reading data
    pub fn buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.buffer
    }

    /// Process a single packet from the buffer and return the response
    pub async fn process_packet(&mut self) -> Result<Option<BytesMut>, ProtocolError> {
        if self.buffer.len() < PACKET_HEADER_SIZE {
            return Ok(None);
        }

        // Parse packet header
        let header = {
            let mut buf_clone = self.buffer.clone();
            match PacketHeader::parse(&mut buf_clone) {
                Ok(h) => h,
                Err(e) => {
                    warn!("Failed to parse packet header: {}", e);
                    return Ok(None);
                }
            }
        };

        // Check if we have the full packet
        if self.buffer.len() < header.length as usize {
            debug!(
                "Incomplete packet: have {} bytes, need {}",
                self.buffer.len(),
                header.length
            );
            return Ok(None);
        }

        // Extract the complete packet
        let packet_data = self.buffer.split_to(header.length as usize);

        // Process the packet and build response
        let response = match header.packet_type {
            PacketType::Login7 => {
                debug!("Handling Login7 from {}", self.addr);

                // Parse Login7 packet body (skip header) for authentication info
                let packet_body = &packet_data[PACKET_HEADER_SIZE..];
                let auth_info = parse_login7_auth(packet_body);

                // Log the server name sent by client (important for verifying redirection behavior)
                if let Some(ref server_name) = auth_info.server_name {
                    info!(
                        "Login7 from {}: client sent ServerName='{}'",
                        self.addr, server_name
                    );
                } else {
                    info!("Login7 from {}: no ServerName in packet", self.addr);
                }

                // FedAuth is always supported - check if client used it
                if auth_info.has_fedauth {
                    let token_len = auth_info
                        .access_token_bytes
                        .as_ref()
                        .map(|v| v.len())
                        .unwrap_or(0);
                    debug!(
                        "FedAuth detected with {} byte token from {}",
                        token_len, self.addr
                    );

                    // Store the received token for verification
                    if let Some(token_bytes) = auth_info.access_token_bytes {
                        debug!(
                            "Stored access token ({} bytes) from {} for verification",
                            token_bytes.len(),
                            self.addr
                        );
                        self.received_token = Some(token_bytes);
                    }
                }

                // Always authenticate (both FedAuth and username/password are supported)
                self.is_authenticated = true;

                // Check if redirection is configured
                if let Some(ref redir) = self.redirection {
                    // Redirect the client to a different server
                    info!(
                        "Redirecting client {} to {}:{}",
                        self.addr, redir.redirect_host, redir.redirect_port
                    );
                    Some(build_routing_response(
                        &redir.redirect_host,
                        redir.redirect_port,
                    ))
                } else {
                    // Build response with LoginAck + optional FeatureExtAck + Done
                    let mut response = build_login_ack();

                    // If client sent FedAuth, respond with FeatureExtAck
                    if auth_info.has_fedauth {
                        debug!("Including FeatureExtAck for FedAuth");
                        response.extend_from_slice(&build_feature_ext_ack_fedauth());
                    }

                    response.extend_from_slice(&build_done_token(0));

                    // Wrap in packet
                    let total_length = (PACKET_HEADER_SIZE + response.len()) as u16;
                    let mut packet = BytesMut::with_capacity(total_length as usize);
                    let resp_header = PacketHeader::new(PacketType::TabularResult, total_length, 1);
                    resp_header.write(&mut packet);
                    packet.extend_from_slice(&response);

                    Some(packet)
                }
            }

            PacketType::SqlBatch => {
                if !self.is_authenticated {
                    warn!(
                        "Received SQL batch from {} before authentication",
                        self.addr
                    );
                    Some(build_error_response("Not authenticated"))
                } else {
                    debug!("Handling SQL batch from {}", self.addr);

                    // Extract packet body (skip header)
                    let packet_body = &packet_data[PACKET_HEADER_SIZE..];

                    // Parse SQL
                    match parse_sql_batch(packet_body) {
                        Ok(sql) => {
                            info!("Executing SQL from {}: {}", self.addr, sql);

                            // Look up query in registry
                            let registry = self.query_registry.lock().await;
                            if let Some(response_data) = registry.get(&sql) {
                                info!("Found registered response for query");
                                // build_query_result already wraps in a packet, so return directly
                                let packet = build_query_result(response_data);
                                Some(packet)
                            } else {
                                info!("No registered response, returning empty result");
                                // Return DONE token
                                let response = build_done_token(0);

                                let total_length = (PACKET_HEADER_SIZE + response.len()) as u16;
                                let mut packet = BytesMut::with_capacity(total_length as usize);
                                let resp_header =
                                    PacketHeader::new(PacketType::TabularResult, total_length, 1);
                                resp_header.write(&mut packet);
                                packet.extend_from_slice(&response);

                                Some(packet)
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse SQL batch from {}: {}", self.addr, e);
                            Some(build_error_response(&format!("Parse error: {}", e)))
                        }
                    }
                }
            }

            PacketType::Attention => {
                debug!("Handling Attention from {}", self.addr);
                // Send DONE with attention flag
                let response = build_done_token(0x0020); // DONE_ATTN

                let total_length = (PACKET_HEADER_SIZE + response.len()) as u16;
                let mut packet = BytesMut::with_capacity(total_length as usize);
                let resp_header = PacketHeader::new(PacketType::TabularResult, total_length, 1);
                resp_header.write(&mut packet);
                packet.extend_from_slice(&response);

                Some(packet)
            }

            _ => {
                debug!(
                    "Ignoring packet type {:?} from {}",
                    header.packet_type, self.addr
                );
                None
            }
        };

        Ok(response)
    }
}

/// Store for captured connection processors.
/// This allows tests to access per-connection state after connections complete.
#[derive(Debug, Default)]
pub struct ConnectionStore {
    /// Completed connection processors keyed by client socket address
    connections: HashMap<SocketAddr, ConnectionInfo>,
}

/// Captured information from a completed connection
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    /// Client socket address
    pub addr: SocketAddr,
    /// Access token received (if any)
    pub received_token: Option<Vec<u8>>,
    /// Whether the client authenticated successfully
    pub authenticated: bool,
}

impl ConnectionInfo {
    /// Get the received access token as a UTF-16LE decoded string
    pub fn received_token_as_string(&self) -> Option<String> {
        self.received_token.as_ref().and_then(|bytes| {
            if bytes.len() % 2 == 0 {
                let u16_chars: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .collect();
                String::from_utf16(&u16_chars).ok()
            } else {
                String::from_utf8(bytes.to_vec()).ok()
            }
        })
    }
}

impl ConnectionStore {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// Store connection info when a connection completes
    pub fn store(&mut self, processor: &ConnectionProcessor) {
        let info = ConnectionInfo {
            addr: processor.addr(),
            received_token: processor.received_token().map(|t| t.to_vec()),
            authenticated: processor.is_authenticated(),
        };
        self.connections.insert(processor.addr(), info);
    }

    /// Get connection info by address
    pub fn get(&self, addr: &SocketAddr) -> Option<&ConnectionInfo> {
        self.connections.get(addr)
    }

    /// Get all connection infos
    pub fn all(&self) -> &HashMap<SocketAddr, ConnectionInfo> {
        &self.connections
    }

    /// Get the count of stored connections
    pub fn count(&self) -> usize {
        self.connections.len()
    }

    /// Clear all stored connections
    pub fn clear(&mut self) {
        self.connections.clear();
    }
}

/// Mock TDS Server
///
/// The server always supports both FedAuth (access token) and username/password authentication.
/// TLS encryption is optional and controlled by providing a TLS identity.
pub struct MockTdsServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    query_registry: Arc<Mutex<QueryRegistry>>,
    tls_acceptor: Option<TlsAcceptor>,
    /// If true, use TDS 8.0 strict mode where TLS starts immediately
    strict_mode: bool,
    /// Store for captured connection info (for test verification)
    connection_store: Arc<Mutex<ConnectionStore>>,
    /// Optional redirection configuration for testing client redirection behavior
    redirection: Option<RedirectionConfig>,
}

impl MockTdsServer {
    /// Create a new mock TDS server without TLS encryption.
    /// FedAuth and username/password authentication are always supported.
    pub async fn new(addr: &str) -> Result<Self, std::io::Error> {
        Self::new_internal(addr, None, false, None).await
    }

    /// Create a new mock TDS server with connection redirection.
    /// When configured, the server will redirect clients to a different endpoint
    /// during login instead of completing authentication.
    ///
    /// # Arguments
    /// * `addr` - Address to bind to (e.g., "127.0.0.1:1433")
    /// * `redirect_host` - The hostname to redirect clients to
    /// * `redirect_port` - The port to redirect clients to
    pub async fn new_with_redirection(
        addr: &str,
        redirect_host: impl Into<String>,
        redirect_port: u16,
    ) -> Result<Self, std::io::Error> {
        Self::new_internal(
            addr,
            None,
            false,
            Some(RedirectionConfig::new(redirect_host, redirect_port)),
        )
        .await
    }

    /// Create a new mock TDS server with optional TLS support (TDS 7.4 style).
    /// FedAuth and username/password authentication are always supported.
    ///
    /// # Arguments
    /// * `addr` - Address to bind to (e.g., "127.0.0.1:1433")
    /// * `identity` - Optional TLS identity for encryption. If None, TLS is disabled.
    pub async fn new_with_tls(
        addr: &str,
        identity: Option<Identity>,
    ) -> Result<Self, std::io::Error> {
        Self::new_internal(addr, identity, false, None).await
    }

    /// Create a new mock TDS server with strict/TDS 8.0 mode.
    /// In strict mode, TLS handshake happens immediately before any TDS packets.
    /// FedAuth and username/password authentication are always supported.
    ///
    /// # Arguments
    /// * `addr` - Address to bind to (e.g., "127.0.0.1:1433")
    /// * `identity` - TLS identity for encryption (required for strict mode)
    pub async fn new_with_strict_tls(
        addr: &str,
        identity: Identity,
    ) -> Result<Self, std::io::Error> {
        Self::new_internal(addr, Some(identity), true, None).await
    }

    /// Internal constructor
    async fn new_internal(
        addr: &str,
        identity: Option<Identity>,
        strict_mode: bool,
        redirection: Option<RedirectionConfig>,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;

        let tls_acceptor = identity.map(|id| {
            let mut builder = native_tls::TlsAcceptor::builder(id);
            if strict_mode {
                builder.accept_alpn(&[mssql_tds::core::TDS_8_ALPN_PROTOCOL]);
            }
            let acceptor = builder.build().expect("Failed to build TLS acceptor");
            TlsAcceptor::from(acceptor)
        });

        let has_tls = tls_acceptor.is_some();
        if strict_mode {
            info!(
                "Mock TDS Server listening on {} with TDS 8.0 strict TLS mode (FedAuth + user/pass supported)",
                local_addr
            );
        } else if has_tls {
            info!(
                "Mock TDS Server listening on {} with TLS enabled (FedAuth + user/pass supported)",
                local_addr
            );
        } else {
            info!(
                "Mock TDS Server listening on {} (no TLS, FedAuth + user/pass supported)",
                local_addr
            );
        }

        if let Some(ref redir) = redirection {
            info!(
                "Redirection enabled: clients will be redirected to {}:{}",
                redir.redirect_host, redir.redirect_port
            );
        }

        Ok(Self {
            listener,
            local_addr,
            query_registry: Arc::new(Mutex::new(QueryRegistry::new())),
            tls_acceptor,
            strict_mode,
            connection_store: Arc::new(Mutex::new(ConnectionStore::new())),
            redirection,
        })
    }

    /// Get a reference to the query registry for registering custom responses
    pub fn query_registry(&self) -> Arc<Mutex<QueryRegistry>> {
        Arc::clone(&self.query_registry)
    }

    /// Get a reference to the connection store for test verification.
    /// This allows tests to check connection state including received access tokens.
    pub fn connection_store(&self) -> Arc<Mutex<ConnectionStore>> {
        Arc::clone(&self.connection_store)
    }

    /// Get the local address the server is bound to
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Run the server (accepts connections in a loop)
    pub async fn run(self) -> Result<(), std::io::Error> {
        let listener = Arc::new(self.listener);
        let registry = self.query_registry;
        let tls_acceptor = self.tls_acceptor.map(Arc::new);
        let strict_mode = self.strict_mode;
        let connection_store = self.connection_store;
        let redirection = self.redirection.map(Arc::new);

        loop {
            let (socket, addr) = listener.accept().await?;
            info!("New connection from {}", addr);

            let registry_clone = Arc::clone(&registry);
            let tls_acceptor_clone = tls_acceptor.clone();
            let store_clone = Arc::clone(&connection_store);
            let redirection_clone = redirection.clone();

            // Spawn a task to handle this connection
            tokio::spawn(async move {
                if let Err(e) = handle_connection_with_tls(
                    socket,
                    addr,
                    registry_clone,
                    tls_acceptor_clone,
                    strict_mode,
                    store_clone,
                    redirection_clone,
                )
                .await
                {
                    error!("Error handling connection from {}: {}", addr, e);
                }
            });
        }
    }

    /// Run the server with a shutdown signal
    pub async fn run_with_shutdown(
        self,
        shutdown: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), std::io::Error> {
        let listener = Arc::new(Mutex::new(self.listener));
        let registry = self.query_registry;
        let tls_acceptor = self.tls_acceptor.map(Arc::new);
        let strict_mode = self.strict_mode;
        let connection_store = self.connection_store;
        let redirection = self.redirection.map(Arc::new);

        tokio::select! {
            result = async {
                loop {
                    let listener = listener.lock().await;
                    match listener.accept().await {
                        Ok((socket, addr)) => {
                            info!("New connection from {}", addr);
                            drop(listener); // Release lock before spawning

                            let registry_clone = Arc::clone(&registry);
                            let tls_acceptor_clone = tls_acceptor.clone();
                            let store_clone = Arc::clone(&connection_store);
                            let redirection_clone = redirection.clone();

                            tokio::spawn(async move {
                                if let Err(e) = handle_connection_with_tls(socket, addr, registry_clone, tls_acceptor_clone, strict_mode, store_clone, redirection_clone).await {
                                    error!("Error handling connection from {}: {}", addr, e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Error accepting connection: {}", e);
                            return Err(e);
                        }
                    }
                }
            } => result,
            _ = shutdown => {
                info!("Shutdown signal received, stopping server");
                Ok(())
            }
        }
    }
}

/// Handle a connection with optional TLS support.
/// FedAuth and username/password authentication are always supported.
async fn handle_connection_with_tls(
    socket: TcpStream,
    addr: SocketAddr,
    query_registry: Arc<Mutex<QueryRegistry>>,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    strict_mode: bool,
    connection_store: Arc<Mutex<ConnectionStore>>,
    redirection: Option<Arc<RedirectionConfig>>,
) -> Result<(), ProtocolError> {
    if strict_mode {
        // TDS 8.0 Strict mode: TLS handshake happens immediately on the socket
        // No TDS wrapping - raw TLS handshake followed by TDS packets over TLS
        let tls_acceptor = tls_acceptor.ok_or_else(|| {
            ProtocolError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Strict mode requires TLS acceptor",
            ))
        })?;

        debug!("Starting TDS 8.0 strict mode TLS handshake for {}", addr);

        let tls_stream = tls_acceptor.accept(socket).await.map_err(|e| {
            error!("TLS handshake failed in strict mode: {}", e);
            ProtocolError::Io(std::io::Error::other(format!(
                "TLS handshake failed: {}",
                e
            )))
        })?;

        info!("TLS handshake successful for {} (strict mode)", addr);
        handle_strict_encrypted_connection(
            tls_stream,
            addr,
            query_registry,
            connection_store,
            redirection,
        )
        .await
    } else {
        // TDS 7.4 mode: First handle PreLogin, then optionally do TDS-wrapped TLS
        let supports_tls = tls_acceptor.is_some();
        let (prelogin_socket, should_encrypt) =
            handle_prelogin_negotiation(socket, addr, supports_tls).await?;

        if should_encrypt && tls_acceptor.is_some() {
            // For TDS 7.4, the client wraps TLS handshake data in TDS PreLogin packets.
            // We need to use TdsTlsWrapper to unwrap TDS packets and extract TLS data.
            use crate::tds_tls_wrapper::TdsTlsWrapper;

            let tds_wrapper = TdsTlsWrapper::new(prelogin_socket);

            // Perform TLS handshake over the TDS-wrapped stream
            let tls_stream = tls_acceptor
                .unwrap()
                .accept(tds_wrapper)
                .await
                .map_err(|e| {
                    error!("TLS handshake failed: {}", e);
                    ProtocolError::Io(std::io::Error::other(format!(
                        "TLS handshake failed: {}",
                        e
                    )))
                })?;

            info!("TLS handshake successful for {}", addr);
            handle_encrypted_tds_wrapped_connection(
                tls_stream,
                addr,
                query_registry,
                connection_store,
                redirection,
            )
            .await
        } else {
            // Continue without encryption
            handle_unencrypted_connection(
                prelogin_socket,
                addr,
                query_registry,
                connection_store,
                redirection,
            )
            .await
        }
    }
}

/// Handle PreLogin packet to negotiate encryption.
/// Always advertises FedAuth support.
async fn handle_prelogin_negotiation(
    mut socket: TcpStream,
    addr: SocketAddr,
    supports_tls: bool,
) -> Result<(TcpStream, bool), ProtocolError> {
    let mut buffer = BytesMut::with_capacity(4096);

    // Read PreLogin packet
    let n = socket.read_buf(&mut buffer).await?;
    if n == 0 {
        return Err(ProtocolError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Connection closed during PreLogin",
        )));
    }

    debug!("Received {} bytes from {} (PreLogin)", n, addr);

    if buffer.len() < PACKET_HEADER_SIZE {
        return Err(ProtocolError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Incomplete PreLogin packet",
        )));
    }

    let mut buf_clone = buffer.clone();
    let header = PacketHeader::parse(&mut buf_clone)?;

    if header.packet_type != PacketType::PreLogin {
        return Err(ProtocolError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Expected PreLogin, got {:?}", header.packet_type),
        )));
    }

    debug!("Handling PreLogin negotiation");

    // Build PreLogin response - always advertise FedAuth support
    let response = build_prelogin_response_with_fedauth(supports_tls, true);

    debug!("Sending {} bytes PreLogin response", response.len());
    socket.write_all(&response).await?;

    // Return socket and whether client should encrypt
    Ok((socket, supports_tls))
}

/// Handle strict mode encrypted connection (TDS 8.0)
/// In strict mode, TLS is established first, then PreLogin and all other TDS packets
/// are exchanged over the encrypted channel.
/// Always supports FedAuth and username/password authentication.
async fn handle_strict_encrypted_connection(
    mut socket: TlsStream<TcpStream>,
    addr: SocketAddr,
    query_registry: Arc<Mutex<QueryRegistry>>,
    connection_store: Arc<Mutex<ConnectionStore>>,
    redirection: Option<Arc<RedirectionConfig>>,
) -> Result<(), ProtocolError> {
    let redir_config = redirection
        .as_ref()
        .map(|r| RedirectionConfig::new(r.redirect_host.clone(), r.redirect_port));
    let mut processor =
        ConnectionProcessor::new_with_redirection(addr, query_registry, redir_config);
    let mut prelogin_handled = false;

    loop {
        // Read data from TLS socket
        let n = socket.read_buf(processor.buffer_mut()).await?;

        if n == 0 {
            debug!("TLS connection closed by client {} (strict mode)", addr);
            break;
        }

        debug!("Received {} encrypted bytes from {} (strict mode)", n, addr);

        // In strict mode, we need to handle PreLogin first, then other packets
        if !prelogin_handled {
            // Check if we have enough data for a packet header
            if processor.buffer_mut().len() >= PACKET_HEADER_SIZE {
                let mut buf_clone = processor.buffer_mut().clone();
                if let Ok(header) = PacketHeader::parse(&mut buf_clone)
                    && header.packet_type == PacketType::PreLogin
                {
                    // Wait for full PreLogin packet
                    if processor.buffer_mut().len() >= header.length as usize {
                        debug!("Handling PreLogin in strict mode");
                        let _ = processor.buffer_mut().split_to(header.length as usize);

                        // Send PreLogin response - strict mode already has encryption, always include FedAuth
                        let response = build_prelogin_response_with_fedauth(true, true);
                        debug!(
                            "Sending {} bytes PreLogin response (strict mode)",
                            response.len()
                        );
                        socket.write_all(&response).await?;
                        prelogin_handled = true;
                        continue;
                    }
                }
            }
            // If we got here and haven't handled PreLogin yet, continue reading
            if !prelogin_handled {
                continue;
            }
        }

        // Process other packets (Login7, SqlBatch, etc.)
        while let Some(response) = processor.process_packet().await? {
            debug!(
                "Sending {} encrypted bytes response (strict mode)",
                response.len()
            );
            socket.write_all(&response).await?;
        }
    }

    // Store connection info for test verification
    let mut store = connection_store.lock().await;
    store.store(&processor);

    Ok(())
}

/// Handle encrypted connection (after TLS handshake)
/// Always supports FedAuth and username/password authentication.
#[allow(dead_code)]
async fn handle_encrypted_connection(
    mut socket: TlsStream<TcpStream>,
    addr: SocketAddr,
    query_registry: Arc<Mutex<QueryRegistry>>,
    connection_store: Arc<Mutex<ConnectionStore>>,
) -> Result<(), ProtocolError> {
    let mut processor = ConnectionProcessor::new(addr, query_registry);

    loop {
        // Read data from TLS socket
        let n = socket.read_buf(processor.buffer_mut()).await?;

        if n == 0 {
            debug!("TLS connection closed by client {}", addr);
            break;
        }

        debug!("Received {} encrypted bytes from {}", n, addr);

        // Process packets
        while let Some(response) = processor.process_packet().await? {
            debug!("Sending {} encrypted bytes response", response.len());
            socket.write_all(&response).await?;
        }
    }

    // Store connection info for test verification
    let mut store = connection_store.lock().await;
    store.store(&processor);

    Ok(())
}

/// Handle encrypted connection over TDS-wrapped TLS (TDS 7.4 style)
/// After the TLS handshake completes, subsequent TDS packets (Login7, SqlBatch, etc.)
/// are sent encrypted through TLS, but no longer wrapped in PreLogin packets.
/// Always supports FedAuth and username/password authentication.
async fn handle_encrypted_tds_wrapped_connection(
    mut socket: TlsStream<crate::tds_tls_wrapper::TdsTlsWrapper>,
    addr: SocketAddr,
    query_registry: Arc<Mutex<QueryRegistry>>,
    connection_store: Arc<Mutex<ConnectionStore>>,
    redirection: Option<Arc<RedirectionConfig>>,
) -> Result<(), ProtocolError> {
    let redir_config = redirection
        .as_ref()
        .map(|r| RedirectionConfig::new(r.redirect_host.clone(), r.redirect_port));
    let mut processor =
        ConnectionProcessor::new_with_redirection(addr, query_registry, redir_config);

    loop {
        // Read data from TLS socket (which wraps TdsTlsWrapper)
        let n = socket.read_buf(processor.buffer_mut()).await?;

        if n == 0 {
            debug!("TLS connection closed by client {}", addr);
            break;
        }

        debug!(
            "Received {} encrypted bytes from {} (TDS-wrapped TLS)",
            n, addr
        );

        // Process packets
        while let Some(response) = processor.process_packet().await? {
            debug!("Sending {} encrypted bytes response", response.len());
            socket.write_all(&response).await?;
        }
    }

    // Store connection info for test verification
    let mut store = connection_store.lock().await;
    store.store(&processor);

    Ok(())
}

/// Handle unencrypted connection (after PreLogin or no TLS)
/// Always supports FedAuth and username/password authentication.
async fn handle_unencrypted_connection(
    mut socket: TcpStream,
    addr: SocketAddr,
    query_registry: Arc<Mutex<QueryRegistry>>,
    connection_store: Arc<Mutex<ConnectionStore>>,
    redirection: Option<Arc<RedirectionConfig>>,
) -> Result<(), ProtocolError> {
    let redir_config = redirection
        .as_ref()
        .map(|r| RedirectionConfig::new(r.redirect_host.clone(), r.redirect_port));
    let mut processor =
        ConnectionProcessor::new_with_redirection(addr, query_registry, redir_config);

    loop {
        // Read data from plain socket
        let n = socket.read_buf(processor.buffer_mut()).await?;

        if n == 0 {
            debug!("Connection closed by client {}", addr);
            break;
        }

        debug!("Received {} bytes from {}", n, addr);

        // Process packets
        while let Some(response) = processor.process_packet().await? {
            debug!("Sending {} bytes response", response.len());
            socket.write_all(&response).await?;
        }
    }

    // Store connection info for test verification
    let mut store = connection_store.lock().await;
    store.store(&processor);

    Ok(())
}

/// Handle a single client connection (legacy, non-TLS)
#[allow(dead_code)]
async fn handle_connection(
    mut socket: TcpStream,
    addr: SocketAddr,
    query_registry: Arc<Mutex<QueryRegistry>>,
) -> Result<(), ProtocolError> {
    let mut buffer = BytesMut::with_capacity(4096);
    let mut is_authenticated = false;

    loop {
        // Read data from socket
        let n = socket.read_buf(&mut buffer).await?;

        if n == 0 {
            debug!("Connection closed by client {}", addr);
            break;
        }

        debug!("Received {} bytes from {}", n, addr);

        // Process all complete packets in the buffer
        while buffer.len() >= PACKET_HEADER_SIZE {
            // Parse packet header
            let header = {
                let mut buf_clone = buffer.clone();
                match PacketHeader::parse(&mut buf_clone) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("Failed to parse packet header: {}", e);
                        break;
                    }
                }
            };

            // Check if we have the full packet
            if buffer.len() < header.length as usize {
                debug!(
                    "Incomplete packet: have {} bytes, need {}",
                    buffer.len(),
                    header.length
                );
                break;
            }

            // Extract the complete packet
            let packet_data = buffer.split_to(header.length as usize);

            // Process the packet
            let response = match header.packet_type {
                PacketType::PreLogin => {
                    debug!("Handling PreLogin");
                    Some(build_prelogin_response())
                }

                PacketType::Login7 => {
                    debug!("Handling Login7");
                    is_authenticated = true;

                    // Build response with LoginAck + EnvChange + Done
                    let mut response = build_login_ack();
                    response.extend_from_slice(&build_done_token(0));

                    // Wrap in packet
                    let total_length = (PACKET_HEADER_SIZE + response.len()) as u16;
                    let mut packet = BytesMut::with_capacity(total_length as usize);
                    let resp_header = PacketHeader::new(PacketType::TabularResult, total_length, 1);
                    resp_header.write(&mut packet);
                    packet.extend_from_slice(&response);

                    Some(packet)
                }

                PacketType::SqlBatch => {
                    if !is_authenticated {
                        warn!("Received SQL batch before authentication");
                        Some(build_error_response("Not authenticated"))
                    } else {
                        debug!("Handling SQL batch");

                        // Extract packet body (skip header)
                        let packet_body = &packet_data[PACKET_HEADER_SIZE..];

                        // Parse SQL
                        match parse_sql_batch(packet_body) {
                            Ok(sql) => {
                                info!("Executing SQL: {}", sql);

                                // Look up query in registry
                                let registry = query_registry.lock().await;
                                if let Some(response) = registry.get(&sql) {
                                    Some(build_query_result(response))
                                } else if sql.to_uppercase().starts_with("SELECT") {
                                    // Return empty result set with DONE for unknown SELECT queries
                                    let mut response = BytesMut::new();
                                    response.extend_from_slice(&build_done_token(0));

                                    let total_length = (PACKET_HEADER_SIZE + response.len()) as u16;
                                    let mut packet = BytesMut::with_capacity(total_length as usize);
                                    let resp_header = PacketHeader::new(
                                        PacketType::TabularResult,
                                        total_length,
                                        1,
                                    );
                                    resp_header.write(&mut packet);
                                    packet.extend_from_slice(&response);

                                    Some(packet)
                                } else {
                                    // For other commands, just return DONE
                                    let response = build_done_token(0);

                                    let total_length = (PACKET_HEADER_SIZE + response.len()) as u16;
                                    let mut packet = BytesMut::with_capacity(total_length as usize);
                                    let resp_header = PacketHeader::new(
                                        PacketType::TabularResult,
                                        total_length,
                                        1,
                                    );
                                    resp_header.write(&mut packet);
                                    packet.extend_from_slice(&response);

                                    Some(packet)
                                }
                            }
                            Err(e) => {
                                error!("Failed to parse SQL batch: {}", e);
                                Some(build_error_response("Failed to parse SQL"))
                            }
                        }
                    }
                }

                PacketType::RpcRequest => {
                    debug!("Handling RPC request (not fully implemented)");
                    // Just return DONE for now
                    let response = build_done_token(0);

                    let total_length = (PACKET_HEADER_SIZE + response.len()) as u16;
                    let mut packet = BytesMut::with_capacity(total_length as usize);
                    let resp_header = PacketHeader::new(PacketType::TabularResult, total_length, 1);
                    resp_header.write(&mut packet);
                    packet.extend_from_slice(&response);

                    Some(packet)
                }

                _ => {
                    warn!("Unhandled packet type: {:?}", header.packet_type);
                    None
                }
            };

            // Send response if we have one
            if let Some(response_data) = response {
                debug!("Sending {} bytes response", response_data.len());
                socket.write_all(&response_data).await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_creation() {
        let server = MockTdsServer::new("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr();
        assert!(addr.port() > 0);
    }
}
