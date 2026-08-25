// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::bulk_copy_state::ATTENTION_TIMEOUT_SECONDS;
use crate::connection::client_context::{IPAddressPreference, TransportContext};
use crate::connection::transport::buffers::TdsReadBuffer;
use crate::connection::transport::extractable_stream;
use crate::connection::transport::parallel_connect::{ParallelConnectConfig, parallel_connect};
use crate::connection::transport::request_timeout::await_within_request_timeout;
use crate::connection::transport::ssl_handler::SslHandler;
use crate::connection_provider::tds_connection_provider::PARSER_REGISTRY;
use crate::core::{
    CancelHandle, EncryptionOptions, EncryptionSetting, NegotiatedEncryptionSetting, TdsResult,
};
use crate::datatypes::row_writer::RowWriter;
use crate::error::Error::{OperationCancelledError, TimeoutError};
use crate::error::TimeoutErrorType;
use crate::handler::handler_factory::SessionSettings;
use crate::io::packet_reader::{LENGTH_NULL, TdsPacketReader};
use crate::io::packet_writer::PacketWriter;
use crate::io::reader_writer::{NetworkReader, NetworkReaderWriter, NetworkWriter};
use crate::io::token_stream::{
    ColumnPolicy, ParserContext, PlpPauseState, RowHeader, RowPauseState, RowReadResult,
    TdsTokenStreamReader, read_active_plp_bytes_internal, receive_row_header_internal,
    receive_row_into_internal, receive_token_internal, resume_row_into_internal,
};
use crate::message::attention::AttentionRequest;
use crate::message::login_options::TdsVersion;
use crate::message::messages::{PacketStatusFlags, Request, ResetConnectionMode};
use crate::token::tokens::{DoneStatus, Tokens};
use async_trait::async_trait;
use byteorder::{BigEndian, ByteOrder, LittleEndian};
use std::cmp::min;
use std::io::Error;
use std::io::ErrorKind;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{self, TcpStream};
use tokio::time::{Instant, timeout, timeout_at};
use tracing::{debug, error, event, info, trace, warn};

#[cfg(windows)]
use crate::connection::transport::localdb::resolve_localdb_instance;
#[cfg(windows)]
use crate::connection::transport::named_pipes::open_named_pipe_with_retry;

pub(crate) const PRE_NEGOTIATED_PACKET_SIZE: u32 = 4096;

/// Creates a base stream for the specified transport context.
/// This function handles the transport-specific connection logic (TCP, Named Pipe, Shared Memory)
/// and returns a boxed Stream that can be used with any TDS version.
///
/// # Arguments
///
/// * `ipaddress_preference` - Preference for IPv4 or IPv6 addresses (used only for sequential mode)
/// * `transport_context` - The transport context specifying the connection type and parameters
/// * `keep_alive_in_ms` - TCP keep-alive idle time in milliseconds
/// * `keep_alive_interval_in_ms` - TCP keep-alive interval in milliseconds
/// * `multi_subnet_failover` - If true, enables parallel connection mode for TCP
/// * `connect_timeout_ms` - Connection timeout in milliseconds
async fn create_base_stream(
    ipaddress_preference: IPAddressPreference,
    transport_context: &TransportContext,
    keep_alive_in_ms: u32,
    keep_alive_interval_in_ms: u32,
    multi_subnet_failover: bool,
    connect_timeout_ms: u64,
) -> TdsResult<Box<dyn Stream>> {
    match transport_context {
        TransportContext::Tcp { host, port, .. } => {
            if multi_subnet_failover {
                // Use parallel connection mode for MultiSubnetFailover
                create_base_stream_parallel(
                    host,
                    *port,
                    keep_alive_in_ms,
                    keep_alive_interval_in_ms,
                    connect_timeout_ms,
                )
                .await
            } else {
                // Use sequential connection mode (original behavior)
                create_base_stream_sequential(
                    ipaddress_preference,
                    host,
                    *port,
                    keep_alive_in_ms,
                    keep_alive_interval_in_ms,
                    connect_timeout_ms,
                )
                .await
            }
        }
        #[cfg(windows)]
        TransportContext::NamedPipe { pipe_name } => {
            if multi_subnet_failover {
                return Err(crate::error::Error::UsageError(
                    "MultiSubnetFailover is only supported with TCP connections. \
                     Named Pipes do not support MultiSubnetFailover."
                        .to_string(),
                ));
            }
            info!("Connecting to Named Pipe: {}", pipe_name);

            // Open Named Pipe with retry logic for ERROR_PIPE_BUSY
            let pipe_client = open_named_pipe_with_retry(pipe_name).await?;

            info!("Connected to Named Pipe: {}", pipe_name);
            Ok(Box::new(pipe_client))
        }
        #[cfg(not(windows))]
        TransportContext::NamedPipe { .. } => Err(crate::error::Error::from(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Named Pipes are only supported on Windows",
        ))),
        #[cfg(windows)]
        TransportContext::SharedMemory { instance_name } => {
            if multi_subnet_failover {
                return Err(crate::error::Error::UsageError(
                    "MultiSubnetFailover is only supported with TCP connections. \
                     Shared Memory does not support MultiSubnetFailover."
                        .to_string(),
                ));
            }
            // Shared Memory protocol is implemented as Named Pipes with a special path format.
            // For SQL Server 2005+, SharedMemory is actually LPC-over-Named-Pipes using the path:
            // \\.\pipe\SQLLocal\<INSTANCE_NAME>
            //
            // This only works for localhost connections and does not support clustered instances.

            // Default to MSSQLSERVER for empty instance name (matching SQL Server behavior)
            let actual_instance = if instance_name.is_empty() {
                "MSSQLSERVER"
            } else {
                instance_name.as_str()
            };

            info!(
                "Connecting via Shared Memory (LPC-over-Named-Pipes) to instance: {}",
                actual_instance
            );

            // Construct the pipe path: \\.\pipe\SQLLocal\<instance>
            let pipe_name = format!(r"\\.\pipe\SQLLocal\{actual_instance}");

            info!("Connecting to Shared Memory pipe: {}", pipe_name);

            // Open Named Pipe with retry logic for ERROR_PIPE_BUSY
            let pipe_client = open_named_pipe_with_retry(&pipe_name).await?;

            info!("Connected to Shared Memory (LPC-over-NP): {}", pipe_name);
            Ok(Box::new(pipe_client))
        }
        #[cfg(not(windows))]
        TransportContext::SharedMemory { .. } => {
            Err(crate::error::Error::from(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Shared Memory is only supported on Windows",
            )))
        }
        #[cfg(windows)]
        TransportContext::LocalDB { instance_name } => {
            if multi_subnet_failover {
                return Err(crate::error::Error::UsageError(
                    "MultiSubnetFailover is only supported with TCP connections. \
                     LocalDB does not support MultiSubnetFailover."
                        .to_string(),
                ));
            }
            info!("Connecting to LocalDB instance: {}", instance_name);

            // Resolve the LocalDB instance to a named pipe path
            // This will:
            // 1. Load the LocalDB API (sqluserinstance.dll)
            // 2. Call LocalDBStartInstance to start the instance if needed
            // 3. Get the named pipe path from the API
            let pipe_name = resolve_localdb_instance(instance_name).await?;

            info!("LocalDB instance resolved to pipe: {}", pipe_name);

            // Connect to the named pipe
            let pipe_client = open_named_pipe_with_retry(&pipe_name).await?;

            info!("Connected to LocalDB instance: {}", instance_name);
            Ok(Box::new(pipe_client))
        }
    }
}

/// Creates a TCP stream using sequential connection mode.
/// Tries each resolved IP address one at a time until one succeeds.
async fn create_base_stream_sequential(
    ipaddress_preference: IPAddressPreference,
    host: &str,
    port: u16,
    keep_alive_in_ms: u32,
    keep_alive_interval_in_ms: u32,
    connect_timeout_ms: u64,
) -> TdsResult<Box<dyn Stream>> {
    info!(
        "Connecting to TCP transport (sequential): {}:{}",
        host, port
    );

    // This will cause the DNS resolution of the addresses.
    let mut socket_addresses = (host, port).to_socket_addrs()?;

    let mut last_error = None;
    let mut tcp_stream = None;

    // Sort the address list based on the IP address preference
    match ipaddress_preference {
        IPAddressPreference::UsePlatformDefault => {
            // Do nothing. Use whatever the OS returns.
            trace!("Using platform default IP address preference");
        }
        IPAddressPreference::IPv4First => {
            let mut addresses: Vec<_> = socket_addresses.collect();
            // Sort IPv4 addresses first
            addresses.sort_by_key(|a| a.is_ipv6());
            socket_addresses = addresses.into_iter();
            trace!("IPv4 addresses first");
        }
        IPAddressPreference::IPv6First => {
            let mut addresses: Vec<_> = socket_addresses.collect();
            // Sort IPv6 addresses first
            addresses.sort_by_key(|b| std::cmp::Reverse(b.is_ipv6()));
            socket_addresses = addresses.into_iter();
            trace!("IPv6 addresses first");
        }
    }

    info!("Socket addresses: {:?}", socket_addresses);

    for socket_address in socket_addresses {
        let socket = if socket_address.is_ipv6() {
            net::TcpSocket::new_v6()?
        } else {
            net::TcpSocket::new_v4()?
        };

        // The defaults for the SQL Server clients are at
        // https://learn.microsoft.com/en-us/sql/tools/configuration-manager/client-protocols-tcp-ip-properties-protocol-tab?view=sql-server-ver16
        let keep_alive_settings = socket2::TcpKeepalive::new()
            .with_time(Duration::from_millis(keep_alive_in_ms as u64))
            .with_interval(Duration::from_millis(keep_alive_interval_in_ms as u64));

        let socket2_socket = socket2::SockRef::from(&socket);
        socket2_socket.set_tcp_keepalive(&keep_alive_settings)?;
        socket2_socket.set_nodelay(true)?;

        // Apply connection timeout to each connection attempt
        let connect_future = socket.connect(socket_address);
        tcp_stream = match timeout(Duration::from_millis(connect_timeout_ms), connect_future).await
        {
            Ok(Ok(stream)) => {
                info!("Connected to TCP transport: {}:{}", host, port);
                Some(stream)
            }
            Ok(Err(e)) => {
                last_error = Some(e);
                None
            }
            Err(_elapsed) => {
                last_error = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "Connection to {} timed out after {}ms",
                        socket_address, connect_timeout_ms
                    ),
                ));
                None
            }
        };
        if tcp_stream.is_some() {
            break;
        }
    }

    // We don't have a valid TCP stream, so we need to return the last error.
    if let Some(stream) = tcp_stream {
        Ok(Box::new(stream))
    } else {
        Err(crate::error::Error::from(last_error.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!("Failed to connect to {host}:{port}"),
            )
        })))
    }
}

/// Creates a TCP stream using parallel connection mode (MultiSubnetFailover).
/// Attempts to connect to all resolved IP addresses simultaneously.
/// The first successful connection wins.
async fn create_base_stream_parallel(
    host: &str,
    port: u16,
    keep_alive_in_ms: u32,
    keep_alive_interval_in_ms: u32,
    connect_timeout_ms: u64,
) -> TdsResult<Box<dyn Stream>> {
    info!(
        "Connecting to TCP transport (parallel/MultiSubnetFailover): {}:{}",
        host, port
    );

    let config = ParallelConnectConfig {
        timeout_ms: connect_timeout_ms,
        keep_alive_in_ms,
        keep_alive_interval_in_ms,
    };

    let result = parallel_connect(host, port, &config).await?;

    info!(
        "Parallel connection succeeded to {} (tried {} addresses, {} failed)",
        result.connected_address, result.total_addresses, result.failed_attempts
    );

    Ok(Box::new(result.stream))
}

/// Creates a NetworkTransport configured for the specified TDS version.
/// This function applies TDS version-specific logic uniformly across all transport types.
async fn create_transport_for_version(
    stream: Box<dyn Stream>,
    tds_version: TdsVersion,
    transport_context: &TransportContext,
    encryption_options: EncryptionOptions,
    encryption_mode: EncryptionSetting,
) -> TdsResult<NetworkTransport> {
    let ssl_handler = SslHandler {
        server_host_name: transport_context.get_server_name().to_string(),
        encryption_options,
    };

    match tds_version {
        TdsVersion::V7_4 => {
            // TDS 7.4 starts with unencrypted streams that could get encrypted as part of prelogin
            // negotiation. TLS must be wrapped in TDS packets for this version.
            info!("Creating NetworkTransport for TDS 7.4 with TLS wrapping");

            Ok(NetworkTransport::new(
                stream,
                ssl_handler,
                PRE_NEGOTIATED_PACKET_SIZE,
                encryption_mode,
                true, // Use TDS 7.4 TLS wrapping
            ))
        }
        TdsVersion::V8_0 => {
            // Enable TLS immediately for TDS 8.0 (before any TDS packets are exchanged)
            info!("Creating NetworkTransport for TDS 8.0 with immediate TLS");

            let encrypted_stream = ssl_handler
                .enable_ssl_async(stream, NegotiatedEncryptionSetting::Strict)
                .await?;

            Ok(NetworkTransport::new(
                encrypted_stream,
                ssl_handler,
                PRE_NEGOTIATED_PACKET_SIZE,
                encryption_mode,
                false, // TDS 8.0 uses standard TLS (no TDS wrapping)
            ))
        }
        TdsVersion::Unknown(version_value) => Err(crate::error::Error::ProtocolError(format!(
            "Unsupported TDS version: 0x{version_value:08X}. Only TDS 7.4 and TDS 8.0 are supported."
        ))),
    }
}

/// Creates a network transport for the specified parameters.
///
/// # Arguments
///
/// * `ipaddress_preference` - Preference for IPv4 or IPv6 addresses
/// * `tds_version` - The TDS protocol version to use
/// * `transport_context` - The transport context specifying connection type
/// * `encryption_options` - Encryption settings for the connection
/// * `keep_alive_in_ms` - TCP keep-alive idle time in milliseconds
/// * `keep_alive_interval_in_ms` - TCP keep-alive interval in milliseconds
/// * `multi_subnet_failover` - If true, enables parallel connection mode for TCP
/// * `connect_timeout_ms` - Connection timeout in milliseconds
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_transport(
    ipaddress_preference: IPAddressPreference,
    tds_version: TdsVersion,
    transport_context: &TransportContext,
    encryption_options: EncryptionOptions,
    keep_alive_in_ms: u32,
    keep_alive_interval_in_ms: u32,
    multi_subnet_failover: bool,
    connect_timeout_ms: u64,
) -> TdsResult<NetworkTransport> {
    let encryption_mode = encryption_options.mode;

    // Step 1: Create the base stream (transport-specific)
    let stream = create_base_stream(
        ipaddress_preference,
        transport_context,
        keep_alive_in_ms,
        keep_alive_interval_in_ms,
        multi_subnet_failover,
        connect_timeout_ms,
    )
    .await?;

    // Step 2: Apply TDS version-specific wrapping (uniform for all transports)
    create_transport_for_version(
        stream,
        tds_version,
        transport_context,
        encryption_options,
        encryption_mode,
    )
    .await
}

/// SSL/TLS lifecycle callbacks for a transport.
#[async_trait]
pub trait TransportSslHandler {
    /// Enables SSL on the transport.
    async fn enable_ssl(&mut self) -> TdsResult<()>;
    /// Disables SSL on the transport.
    async fn disable_ssl(&mut self) -> TdsResult<()>;
}

/// Async read/write stream with TLS handshake hooks.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + Sync {
    /// Called when a TLS handshake is about to begin.
    fn tls_handshake_starting(&mut self);
    /// Called after TLS handshake completes.
    fn tls_handshake_completed(&mut self);
    /// Check whether the underlying connection is dead via a non-blocking socket poll.
    /// Returns `true` if the connection is known to be dead, `false` if alive or unknown.
    fn is_connection_dead(&self) -> bool {
        false
    }

    /// Returns this connection's TLS channel binding token (`tls-unique`,
    /// RFC 5929 §3) if one is available.
    ///
    /// Used by integrated authentication (SSPI/GSSAPI) to participate in SQL
    /// Server Extended Protection for Authentication. The returned bytes are
    /// the full `SEC_CHANNEL_BINDINGS` structure produced by the TLS engine,
    /// ready to be passed verbatim to the platform auth provider.
    ///
    /// Returns `None` for plaintext streams and for TLS engines that do not
    /// expose the token (today, every engine except the Windows
    /// Schannel-direct one).
    fn channel_binding_token(&self) -> Option<Vec<u8>> {
        None
    }
}

impl Stream for TcpStream {
    fn tls_handshake_starting(&mut self) {
        // No-op for plain TCP streams
    }

    fn tls_handshake_completed(&mut self) {
        // No-op for plain TCP streams
    }

    fn is_connection_dead(&self) -> bool {
        // Non-blocking socket poll to detect a dead connection.
        // try_read will consume a byte if data is available, but this is safe because
        // is_connection_dead() is only called on idle connections (before sending a new
        // command, when no TDS response data is expected on the socket).
        // WouldBlock means the socket is alive (no data available, connection still open).
        // Ok(0) means EOF (server closed connection). Any other error means broken.
        // Ok(n > 0) means unexpected data — treat connection as alive; the data loss is
        // not an issue because the connection state is already invalid if unsolicited data
        // arrives.
        match self.try_read(&mut [0u8; 1]) {
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => false,
            Ok(0) => true,
            Err(_) => true,
            Ok(_) => false,
        }
    }
}

impl Stream for Box<dyn Stream> {
    fn tls_handshake_starting(&mut self) {
        (**self).tls_handshake_starting();
    }

    fn tls_handshake_completed(&mut self) {
        (**self).tls_handshake_completed();
    }

    fn is_connection_dead(&self) -> bool {
        (**self).is_connection_dead()
    }

    fn channel_binding_token(&self) -> Option<Vec<u8>> {
        (**self).channel_binding_token()
    }
}

pub(crate) struct NetworkTransport {
    encryption: Option<NegotiatedEncryptionSetting>,
    packet_size: u32,
    stream: Option<Box<dyn Stream>>,
    ssl_handler: SslHandler,
    encryption_setting: EncryptionSetting,
    tds_read_buffer: TdsReadBuffer,
    use_tds74_tls_wrapping: bool,
    /// Handle to extract the underlying stream when disabling TLS.
    /// This is set during enable_ssl and used during disable_ssl for "Login Only" mode.
    extractable_stream_handle: Option<extractable_stream::ExtractableStreamHandle>,
    /// Pending connection-reset request to apply to the next SQL Batch, RPC, or
    /// Transaction Manager request. Consumed by the packet writer.
    pending_reset: ResetConnectionMode,
    /// Set once the packet writer has put a header carrying `pending_reset` on
    /// the wire. `TdsClient` takes it to learn that the server now owes a
    /// `ResetConnection` ENVCHANGE, which is what makes the acknowledgement
    /// verifiable.
    reset_dispatched: bool,
    /// Cached liveness status. Set to `true` once the connection is explicitly
    /// closed or an I/O operation observes it broken. Surfaced by
    /// `connection_known_dead()` as a cheap, socket-free liveness check.
    known_dead: bool,
    /// Reusable NBCROW null-bitmap allocation. Refilled in place for every
    /// NBCROW row of a result set instead of reallocating per row; see
    /// `read_nbc_bitmap`.
    nbc_bitmap_scratch: Option<Arc<[u8]>>,
}

impl std::fmt::Debug for NetworkTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkTransport")
            .field("encryption", &self.encryption)
            .field("packet_size", &self.packet_size)
            .field("stream", &"<stream>")
            .field("ssl_handler", &self.ssl_handler)
            .field("encryption_setting", &self.encryption_setting)
            .finish()
    }
}

impl NetworkReaderWriter for NetworkTransport {
    fn notify_encryption_setting_change(&mut self, setting: NegotiatedEncryptionSetting) {
        self.notify_encryption_negotiation(setting);
    }

    fn notify_session_setting_change(&mut self, setting: &SessionSettings) {
        self.packet_size = setting.packet_size;
        // Note: The read buffer's max_packet_size is updated in reset_reader(),
        // which is called before each command execution. This ensures the buffer
        // is properly sized when we start reading the server's response.
    }

    fn as_writer(&mut self) -> &mut dyn NetworkWriter {
        self
    }
}

#[async_trait]
impl NetworkReader for NetworkTransport {
    fn packet_size(&self) -> u32 {
        self.packet_size
    }
}

#[async_trait]
impl NetworkWriter for NetworkTransport {
    async fn send(&mut self, data: &[u8]) -> TdsResult<()> {
        let stream = self.stream.as_mut().ok_or_else(|| {
            crate::error::Error::ConnectionClosed(
                "Cannot send: connection has been closed".to_string(),
            )
        })?;
        if let Err(e) = stream.write_all(data).await {
            // A write failure means the socket is broken; record it so the cached
            // liveness check reports the connection as dead.
            self.known_dead = true;
            return Err(e.into());
        }
        Ok(())
    }

    fn packet_size(&self) -> u32 {
        self.packet_size
    }

    fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
        self.encryption
            .unwrap_or(NegotiatedEncryptionSetting::NoEncryption)
    }

    fn set_reset_mode(&mut self, mode: ResetConnectionMode) {
        self.pending_reset = mode;
        // A newly armed reset supersedes any earlier one, so the record of a
        // previous dispatch must not be attributed to it.
        self.reset_dispatched = false;
    }

    fn take_reset_mode(&mut self) -> ResetConnectionMode {
        std::mem::replace(&mut self.pending_reset, ResetConnectionMode::None)
    }

    fn note_reset_dispatched(&mut self) {
        self.reset_dispatched = true;
    }

    fn take_reset_dispatched(&mut self) -> bool {
        std::mem::replace(&mut self.reset_dispatched, false)
    }

    fn channel_binding_token(&self) -> Option<Vec<u8>> {
        // After a successful TLS handshake `self.stream` holds the encrypted
        // stream; the call forwards through `Box<dyn Stream>` to the TLS
        // engine, which returns its `tls-unique` token (Windows Schannel-direct
        // only today). Plaintext / unencrypted connections return `None`.
        self.stream.as_ref()?.channel_binding_token()
    }
}

impl NetworkTransport {
    pub fn new(
        stream: Box<dyn Stream>,
        ssl_handler: SslHandler,
        packet_size: u32,
        encryption_setting: EncryptionSetting,
        use_tds74_tls_wrapping: bool,
    ) -> Self {
        Self {
            encryption: None,
            stream: Some(stream),
            ssl_handler,
            packet_size,
            encryption_setting,
            tds_read_buffer: TdsReadBuffer::new(packet_size as usize),
            use_tds74_tls_wrapping,
            extractable_stream_handle: None,
            pending_reset: ResetConnectionMode::None,
            reset_dispatched: false,
            known_dead: false,
            nbc_bitmap_scratch: None,
        }
    }

    pub(crate) fn notify_encryption_negotiation(
        &mut self,
        encryption: NegotiatedEncryptionSetting,
    ) {
        assert!(self.encryption.is_none());
        self.encryption = Some(encryption);
    }

    async fn enable_ssl_internal(&mut self) -> TdsResult<()> {
        // Take ownership of the stream temporarily
        let base_stream = self.stream.take().expect("Stream already taken");

        // For TDS 7.4, wrap the stream in TlsOverTdsStream before TLS handshake
        // This is required because TLS packets must be framed within TDS packets during the handshake
        let base_stream: Box<dyn Stream> = if self.use_tds74_tls_wrapping {
            #[cfg(target_os = "macos")]
            {
                // On macOS, wrap in BufferedTdsStream to handle Security.framework's
                // multiple small writes during TLS handshake. Security.framework makes
                // separate poll_write calls for ClientKeyExchange, ChangeCipherSpec, and
                // Finished messages, but SQL Server expects them as a single TDS packet.
                let tls_over_tds =
                    crate::connection::transport::ssl_handler::TlsOverTdsStream::new(base_stream);
                Box::new(
                    crate::connection::transport::ssl_handler::BufferedTdsStream::new(tls_over_tds),
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                Box::new(
                    crate::connection::transport::ssl_handler::TlsOverTdsStream::new(base_stream),
                )
            }
        } else {
            base_stream
        };

        // Wrap the stream in ExtractableStream so we can reclaim it when disabling TLS
        // This is needed for "Login Only" encryption mode where TLS is disabled after login
        let (handle, extractable_stream) =
            extractable_stream::ExtractableStreamHandle::new(base_stream);
        self.extractable_stream_handle = Some(handle);

        // Perform TLS handshake (consumes extractable_stream, returns TlsStream)
        // enable_ssl_async will call tls_handshake_starting and tls_handshake_completed internally
        let negotiated = self
            .encryption
            .unwrap_or(NegotiatedEncryptionSetting::Mandatory);
        let encrypted_stream = self
            .ssl_handler
            .enable_ssl_async(Box::new(extractable_stream), negotiated)
            .await?;

        // Put back the encrypted stream
        self.stream = Some(encrypted_stream);
        Ok(())
    }

    async fn disable_ssl_internal(&mut self) -> TdsResult<()> {
        // Take the current encrypted TLS stream
        let encrypted_stream = self.stream.take().ok_or_else(|| {
            crate::error::Error::ImplementationError(
                "disable_ssl called but stream is not available".to_string(),
            )
        })?;

        // Extract the underlying stream from the ExtractableStream wrapper.
        // We use mem::forget on the TLS stream to avoid sending TLS close_notify,
        // which would confuse SQL Server in "Login Only" mode.
        std::mem::forget(encrypted_stream);

        // Get the underlying stream from our stored handle.
        // This can fail if:
        // 1. enable_ssl was never called (extractable_stream_handle is None)
        // 2. disable_ssl was called twice (stream already extracted)
        let handle = self.extractable_stream_handle.take().ok_or_else(|| {
            error!("disable_ssl called but enable_ssl was never called");
            crate::error::Error::ImplementationError(
                "Cannot disable TLS: TLS was never enabled (no extractable stream handle)"
                    .to_string(),
            )
        })?;

        let base_stream = handle
            .extract()
            .map_err(|e| {
                error!("Failed to lock extractable stream: {e}");
                crate::error::Error::ImplementationError(format!("Cannot disable TLS: {e}"))
            })?
            .ok_or_else(|| {
                error!("Failed to extract underlying stream - was disable_ssl called twice?");
                crate::error::Error::ImplementationError(
                    "Cannot disable TLS: underlying stream was already extracted".to_string(),
                )
            })?;

        info!("Successfully disabled TLS, reverting to unencrypted stream");
        self.stream = Some(base_stream);
        Ok(())
    }

    pub(crate) async fn close_transport(&mut self) -> TdsResult<()> {
        if let Some(stream) = self.stream.as_mut() {
            stream.shutdown().await?;
        }
        // Drop the stream and record the closed state. `connection_known_dead()`
        // surfaces this to connection pools so they don't reuse a closed
        // connection; the live `is_connection_dead()` poll also reports dead
        // once the stream is gone.
        self.stream = None;
        self.known_dead = true;
        Ok(())
    }

    /// How many consecutive payload-free end-of-message packets a bulk read
    /// tolerates before giving up.
    ///
    /// One is legitimate: a read that begins exactly at a message boundary
    /// consumes the empty packet terminating the *previous* message before its
    /// own data arrives. A second in a row means the peer emitted two empty
    /// messages back to back, so nothing advanced between iterations and the
    /// loop would spin until the command timeout.
    const MAX_CONSECUTIVE_EMPTY_MESSAGES: u32 = 1;

    /// Error for a bulk read that kept receiving empty messages without
    /// advancing.
    #[cold]
    fn stalled_on_empty_messages_error(outstanding: usize) -> crate::error::Error {
        crate::error::Error::ProtocolError(format!(
            "TDS read stalled: {outstanding} more byte(s) required but the peer sent {} \
             consecutive payload-free end-of-message packets without advancing.",
            Self::MAX_CONSECUTIVE_EMPTY_MESSAGES + 1
        ))
    }

    /// Error for a bulk read that needed more bytes but got a packet carrying
    /// none.
    ///
    /// Unreachable as the framing layer stands today, and deliberately kept.
    /// [`get_new_tds_packet`](Self::get_new_tds_packet) rejects a payload-free
    /// packet unless it is end-of-message, so every successful refill leaves
    /// either payload in the buffer or `end_of_message` set. In all three bulk
    /// loops `to_read == 0` means the buffer is empty, which means the refill
    /// framed a payload-free packet, which therefore means the message ended —
    /// and that case is handled by the two branches below this one.
    ///
    /// It stays as a guard on that invariant. If the framing layer ever
    /// tolerates a payload-free non-EOM packet, the loop would re-enter the
    /// refill with the same unmet demand and block on the socket forever, which
    /// is the hang this whole rule exists to prevent. Failing loudly is the
    /// right answer if that day comes; the alternative is a silent infinite
    /// loop. It is also the one branch here with no scalar counterpart, so were
    /// it reachable the two reader families would genuinely disagree — bulk
    /// would error where scalar hangs.
    #[cold]
    fn no_progress_error(outstanding: usize) -> crate::error::Error {
        crate::error::Error::ProtocolError(format!(
            "TDS read made no progress: {outstanding} more byte(s) required but the packet \
             carried no payload. The value extends past the end of the message."
        ))
    }

    /// True when the request for `needed` bytes cannot possibly be satisfied.
    ///
    /// The terminating packet of the current message has already been framed,
    /// so the buffer holds every byte the server is going to send until the
    /// client issues a new request. If the caller wants more than what remains,
    /// waiting is futile — the refill would block on a socket that stays silent.
    fn message_cannot_satisfy(&self, needed: usize) -> bool {
        self.tds_read_buffer.end_of_message
            && needed > self.tds_read_buffer.get_remaining_byte_count()
    }

    /// True when a read that is already partway through a value cannot be
    /// completed, because the message it is being read from has ended.
    ///
    /// `consumed_any` reports whether the caller has already copied bytes of
    /// this value out of the buffer; bytes still sitting in the buffer count
    /// too. Either way the value has started, so the remainder can only arrive
    /// in the current message — and that message is over.
    ///
    /// A read that begins *exactly* at a boundary is deliberately not caught.
    /// The transport cannot tell a caller starting the next message apart from
    /// one over-reading the last, and both handshake and attention paths
    /// legitimately do the former: the login exchange is multi-leg, and the
    /// DONE carrying ATTN arrives in a message of its own. Distinguishing them
    /// needs a layer that knows whether another message is expected, which the
    /// transport is not. See the note in `refill_for`.
    fn value_truncated_by_message_end(&self, consumed_any: bool, needed: usize) -> bool {
        (consumed_any || self.tds_read_buffer.get_remaining_byte_count() > 0)
            && self.message_cannot_satisfy(needed)
    }

    /// Error for a read that extends past the end of the current message.
    #[cold]
    fn past_end_of_message_error(outstanding: usize, available: usize) -> crate::error::Error {
        crate::error::Error::ProtocolError(format!(
            "TDS read extends past the end of the message: {outstanding} more byte(s) required \
             but only {available} remain in the final packet of the message."
        ))
    }

    /// Refills the read buffer for a scalar read that still needs `needed`
    /// bytes, failing if the value is truncated by the end of the message.
    ///
    /// Every reader loops until the buffer holds enough bytes. When part of a
    /// value sits in the final packet of a message the rest can never arrive —
    /// the server sends nothing further until the client issues a new request —
    /// so the loop would go back to the socket forever instead of failing.
    ///
    /// The bulk loops apply the identical rule through
    /// [`Self::value_truncated_by_message_end`], so the two families agree on
    /// every input. That agreement leans on the framing invariant described on
    /// [`Self::no_progress_error`]: the extra branch the bulk loops carry, and
    /// the scalar readers do not, cannot fire while framing rejects
    /// payload-free non-EOM packets.
    ///
    /// Known gap: a read that begins exactly at a message boundary is still
    /// allowed through, so it can block if the server has nothing more to send.
    /// Catching that needs the caller to say whether another message is
    /// expected — the handshake and attention paths cross boundaries here on
    /// purpose — which is a layer above this one.
    async fn refill_for(&mut self, needed: usize) -> TdsResult<()> {
        if self.value_truncated_by_message_end(false, needed) {
            return Err(Self::past_end_of_message_error(
                needed,
                self.tds_read_buffer.get_remaining_byte_count(),
            ));
        }
        self.read_tds_packet().await
    }

    async fn read_tds_packet(&mut self) -> TdsResult<()> {
        let remaining_bytes = self.tds_read_buffer.get_remaining_byte_count();
        if remaining_bytes > 0 {
            // Move the remaining bytes to the beginning of the buffer.
            self.tds_read_buffer.shift_data_to_front();
            let new_packet_size = self.get_new_tds_packet().await?;
            self.tds_read_buffer
                .remove_header_from_packet(new_packet_size);
        } else {
            self.tds_read_buffer.reset_to_length(0);
            let new_packet_size = self.get_new_tds_packet().await?;
            self.tds_read_buffer
                .remove_header_from_packet(new_packet_size);
        }
        Ok(())
    }

    /// Reads a complete TDS packet from the network into the working buffer.
    ///
    /// This method handles the case where a single `read()` call returns data for multiple
    /// TDS packets (TCP coalescing, Named Pipes message boundaries, etc.). Extra bytes
    /// beyond the current packet are tracked in `pending_bytes` for the next call.
    ///
    /// # Buffer Layout
    ///
    /// ```text
    /// ┌─────────────────────────────────────────────────────────────────────────┐
    /// │                          working_buffer                                 │
    /// ├─────────────────────────────────────────────────────────────────────────┤
    /// │ [existing data]  │  [new packet starts at base_offset]                  │
    /// │ (buffer_length)  │                                                      │
    /// └─────────────────────────────────────────────────────────────────────────┘
    ///                    ▲
    ///                    base_offset = buffer_length (where new packet goes)
    /// ```
    ///
    /// # Scenario 1: Single Packet Read (Normal Case)
    ///
    /// ```text
    /// Network read() returns exactly one TDS packet:
    ///
    /// ┌────────────────────────────────────────┐
    /// │  TDS Packet (e.g., 200 bytes)          │
    /// │  [HDR 8B][    PAYLOAD 192B    ]        │
    /// └────────────────────────────────────────┘
    ///           ▲
    ///           bytes_available = 200
    ///           packet_size_from_header = 200
    ///           extra_bytes = 0  ← No pending bytes
    /// ```
    ///
    /// # Scenario 2: Multiple Packets in One Read (Coalescing)
    ///
    /// ```text
    /// Network read() returns TWO TDS packets at once (e.g., TCP coalescing):
    ///
    /// ┌────────────────────────────────────────┬────────────────────────────────┐
    /// │  TDS Packet 1 (200 bytes)              │  TDS Packet 2 (150 bytes)      │
    /// │  [HDR 8B][    PAYLOAD 192B    ]        │  [HDR 8B][ PAYLOAD 142B ]      │
    /// └────────────────────────────────────────┴────────────────────────────────┘
    ///           ▲                                         ▲
    ///           │                                         │
    ///           bytes_available = 350                     pending_bytes = 150
    ///           packet_size_from_header = 200             pending_bytes_offset = base_offset + 200
    ///           extra_bytes = 150
    ///
    /// After this call returns:
    ///   - Returns packet_size = 200 (first packet)
    ///   - pending_bytes = 150, pending_bytes_offset points to Packet 2
    /// ```
    ///
    /// # Scenario 3: Next Call Uses Pending Bytes
    ///
    /// ```text
    /// On next call, pending_bytes > 0, so we move them to base_offset first:
    ///
    /// BEFORE:
    /// ┌──────────────────────────────────────────────────────────────────────────┐
    /// │  [Packet 1 data - already processed]  │  [Packet 2 - pending]            │
    /// │                                       │  (pending_bytes_offset)          │
    /// └──────────────────────────────────────────────────────────────────────────┘
    ///
    /// AFTER copy_within():
    /// ┌──────────────────────────────────────────────────────────────────────────┐
    /// │  [Packet 2 moved to base_offset]      │  ...                             │
    /// │  bytes_available = 150                │                                  │
    /// └──────────────────────────────────────────────────────────────────────────┘
    ///
    /// If Packet 2 is complete (150 >= header's length), no network read needed!
    /// ```
    ///
    /// # Why This Matters
    ///
    /// Without tracking `pending_bytes`:
    /// - TCP: Rare data corruption on high-latency networks where packets coalesce
    /// - Named Pipes: **100% failure** - message mode returns multiple packets per read
    /// - Shared Memory: Same as Named Pipes (uses Named Pipes internally)
    ///
    /// The fix ensures all bytes from `read()` are accounted for, not just the first packet.
    async fn get_new_tds_packet(&mut self) -> TdsResult<usize> {
        let base_offset = self.tds_read_buffer.buffer_length;

        // Check if we have pending bytes from a previous read that included multiple packets
        let mut bytes_available = self.tds_read_buffer.pending_bytes;
        let pending_offset = self.tds_read_buffer.pending_bytes_offset;

        if bytes_available > 0 {
            // Validate bounds before copy_within to avoid panic on malformed data.
            // These values are derived from packet lengths on the wire, so we must
            // guard against corrupted or malicious packets.
            let src_end = pending_offset.saturating_add(bytes_available);
            let dest_end = base_offset.saturating_add(bytes_available);
            let buffer_len = self.tds_read_buffer.working_buffer.len();

            if src_end > buffer_len || dest_end > buffer_len {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Invalid pending bytes range: src {}..{}, dest {}, buffer_len {}",
                    pending_offset, src_end, base_offset, buffer_len
                )));
            }

            // We have pending bytes - move them to base_offset
            self.tds_read_buffer
                .working_buffer
                .copy_within(pending_offset..src_end, base_offset);
            self.tds_read_buffer.pending_bytes = 0;
            self.tds_read_buffer.pending_bytes_offset = 0;
        }

        let stream = self.stream.as_mut().ok_or_else(|| {
            crate::error::Error::ConnectionClosed(
                "Cannot read TDS packet: connection has been closed".to_string(),
            )
        })?;

        // Read more data if we don't have enough for the header
        while bytes_available < PacketWriter::PACKET_HEADER_SIZE {
            let bytes_read = match stream
                .read(&mut self.tds_read_buffer.working_buffer[base_offset + bytes_available..])
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    // A read failure means the socket is broken; record it so the
                    // cached liveness check reports the connection as dead.
                    self.known_dead = true;
                    return Err(e.into());
                }
            };
            if bytes_read == 0 {
                self.known_dead = true;
                return Err(crate::error::Error::ConnectionClosed(
                    "Connection closed by server while reading TDS packet header".to_string(),
                ));
            }
            bytes_available += bytes_read;
        }

        let length_from_packet_header = BigEndian::read_u16(
            &self.tds_read_buffer.working_buffer[base_offset + 2..base_offset + 4],
        );

        let packet_size_from_header: usize = length_from_packet_header as usize;

        // Validate packet_size_from_header against protocol constraints.
        // A malicious or corrupted server could send invalid lengths.
        if packet_size_from_header < PacketWriter::PACKET_HEADER_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid TDS packet length {}: must be at least {} bytes (header size)",
                packet_size_from_header,
                PacketWriter::PACKET_HEADER_SIZE
            )));
        }

        // A payload-free packet that is not the end of its message is
        // malformed: it neither carries payload nor terminates a message. An
        // empty EOM packet is legal (it terminates a message), so only non-EOM
        // ones are rejected.
        let is_end_of_message = self.tds_read_buffer.working_buffer[base_offset + 1]
            & PacketStatusFlags::Eom as u8
            != 0;
        self.tds_read_buffer.end_of_message = is_end_of_message;
        if packet_size_from_header == PacketWriter::PACKET_HEADER_SIZE && !is_end_of_message {
            return Err(crate::error::Error::ProtocolError(
                "Received a payload-free TDS packet that is not end-of-message".to_string(),
            ));
        }

        if packet_size_from_header > self.tds_read_buffer.max_packet_size {
            return Err(crate::error::Error::ProtocolError(format!(
                "TDS packet length {} exceeds negotiated max packet size {}",
                packet_size_from_header, self.tds_read_buffer.max_packet_size
            )));
        }

        // Also ensure we won't exceed buffer capacity
        let buffer_len = self.tds_read_buffer.working_buffer.len();
        if base_offset.saturating_add(packet_size_from_header) > buffer_len {
            return Err(crate::error::Error::ProtocolError(format!(
                "TDS packet length {} at offset {} exceeds buffer capacity {}",
                packet_size_from_header, base_offset, buffer_len
            )));
        }

        // Keep reading until we have the complete packet in memory.
        while bytes_available < packet_size_from_header {
            let bytes_read = match stream
                .read(&mut self.tds_read_buffer.working_buffer[base_offset + bytes_available..])
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    self.known_dead = true;
                    return Err(e.into());
                }
            };
            if bytes_read == 0 {
                self.known_dead = true;
                return Err(crate::error::Error::ConnectionClosed(
                    "Connection closed by server while reading TDS packet payload".to_string(),
                ));
            }
            bytes_available += bytes_read;
        }

        // Calculate how many extra bytes we read beyond this packet
        let extra_bytes = bytes_available - packet_size_from_header;

        if extra_bytes > 0 {
            // Track where the extra bytes are - they're right after this packet in the buffer
            self.tds_read_buffer.pending_bytes = extra_bytes;
            self.tds_read_buffer.pending_bytes_offset = base_offset + packet_size_from_header;
        } else {
            self.tds_read_buffer.pending_bytes = 0;
            self.tds_read_buffer.pending_bytes_offset = 0;
        }

        event!(
            tracing::Level::DEBUG,
            "Received packet of size: {:?}",
            packet_size_from_header
        );

        use pretty_hex::PrettyHex;

        event!(
            tracing::Level::DEBUG,
            "Packet content: {:?}",
            &mut self.tds_read_buffer.working_buffer
                [base_offset..base_offset + packet_size_from_header]
                .hex_dump()
        );
        Ok(packet_size_from_header)
    }

    /// Tells the server to stop sending tokens for the token stream being read
    /// and waits, for a bounded time, for the acknowledgement.
    ///
    /// # Contract
    ///
    /// Cancelling a read is bounded end to end: the ATTENTION write and the
    /// drain that follows it share a single [`ATTENTION_TIMEOUT_SECONDS`]
    /// deadline, matching `Microsoft.Data.SqlClient`'s
    /// `AttentionTimeoutSeconds`. Callers get their cancellation or timeout
    /// back within that bound whatever the server does, so no separate
    /// cancellation-cleanup deadline is needed.
    ///
    /// Acknowledged, the connection is left at a message boundary and stays
    /// reusable. Unacknowledged — the send failed or stalled, the drain
    /// errored, or the bound elapsed — the stream is parked at an unknown
    /// point, so the connection is marked known-dead and pools will not hand
    /// it out again. Once that verdict is in, a later cancelled read returns
    /// straight away rather than spending the bound over again.
    ///
    /// This reports nothing to the caller on purpose. It runs on a path that
    /// already has an error to deliver (the cancellation or the timeout), and
    /// replacing that with a cleanup failure would hide why the read stopped.
    /// The known-dead flag is how a failed cleanup is observed.
    async fn cancel_read_stream_and_wait(&mut self) {
        if self.known_dead {
            // An earlier cancellation already spent the bound and gave up on
            // this connection. There is nothing left to acknowledge, so
            // re-entering would just charge this caller the bound again.
            debug!("Skipping attention: the connection is already known dead");
            return;
        }

        let attention_timeout = Duration::from_secs(ATTENTION_TIMEOUT_SECONDS);
        if let Err(e) = self.send_attention_and_wait(attention_timeout).await {
            debug!("Failed to cancel the read stream: {e:?}");
        }
    }

    /// Sends ATTENTION and drains to the acknowledgement, both under a single
    /// `attention_timeout` deadline.
    ///
    /// The send is bounded too. Writing the packet parks until the socket
    /// accepts it, so a peer that has stopped reading stalls the send and the
    /// acknowledgement deadline would never be armed. The drain gets whatever
    /// the send left of the deadline.
    ///
    /// Anything other than an acknowledgement leaves the stream parked at an
    /// unknown point, so every such outcome marks the connection known-dead
    /// here. Callers can then ignore the return value and still not reuse a
    /// connection with an ATTENTION outstanding on it.
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - Attention acknowledged by server
    /// * `Ok(false)` - The bound elapsed, sending or waiting
    /// * `Err(_)` - Error sending attention or reading the response
    async fn send_attention_and_wait(&mut self, attention_timeout: Duration) -> TdsResult<bool> {
        let deadline = Instant::now() + attention_timeout;

        match timeout_at(deadline, self.cancel_read_stream()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                self.known_dead = true;
                return Err(e);
            }
            Err(_elapsed) => {
                // A stalled write leaves a partial packet on the wire, so the
                // stream can never be resynchronised.
                warn!(
                    timeout = ?attention_timeout,
                    "Timed out sending the attention packet; marking the connection dead"
                );
                self.known_dead = true;
                return Ok(false);
            }
        }

        match self.wait_for_attention_ack(deadline).await {
            Ok(true) => Ok(true),
            Ok(false) => {
                warn!(
                    timeout = ?attention_timeout,
                    "Attention went unacknowledged within the bound; marking the connection dead"
                );
                self.known_dead = true;
                Ok(false)
            }
            Err(e) => {
                self.known_dead = true;
                Err(e)
            }
        }
    }

    /// Wait for attention acknowledgment from server until `deadline`.
    ///
    /// Reads tokens until a DONE token with the ATTN flag arrives, discarding
    /// everything else the server was still sending.
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - Attention acknowledged by server
    /// * `Ok(false)` - `deadline` passed before the acknowledgement
    /// * `Err(_)` - Error reading response
    async fn wait_for_attention_ack(&mut self, deadline: Instant) -> TdsResult<bool> {
        let start = Instant::now();

        match timeout_at(deadline, self.drain_to_attention_ack()).await {
            Ok(Ok(())) => {
                debug!("Attention ACK received after {:?}", start.elapsed());
                Ok(true)
            }
            Ok(Err(e)) => Err(e),
            Err(_elapsed) => {
                debug!("Attention ACK timeout after {:?}", start.elapsed());
                Ok(false)
            }
        }
    }

    /// Reads and discards tokens until the DONE carrying ATTN arrives.
    ///
    /// Unbounded on its own — every caller wraps it in a timeout.
    async fn drain_to_attention_ack(&mut self) -> TdsResult<()> {
        let dummy_context = ParserContext::None(());

        loop {
            let token = receive_token_internal(self, &*PARSER_REGISTRY, &dummy_context).await?;
            if let Tokens::Done(done_token) = token
                && done_token.status.contains(DoneStatus::ATTN)
            {
                return Ok(());
            }
        }
    }
}

#[async_trait]
impl TransportSslHandler for NetworkTransport {
    async fn enable_ssl(&mut self) -> TdsResult<()> {
        self.enable_ssl_internal().await
    }

    async fn disable_ssl(&mut self) -> TdsResult<()> {
        if self.encryption_setting == EncryptionSetting::Strict {
            return Err(crate::error::Error::from(Error::new(
                std::io::ErrorKind::InvalidInput,
                "Under strict mode the client must communicate over TLS",
            )));
        }

        self.disable_ssl_internal().await
    }
}

impl TdsPacketReader for NetworkTransport {
    fn reset_reader(&mut self) {
        // Callers reset before starting a new message, so the buffer is
        // expected to be fully consumed. Log a violation instead of asserting:
        // `buffer_length` comes from the packet header the peer sent, so a
        // malformed or truncated response must not be able to abort the
        // process. Discarding the leftover bytes is what a reset means, so the
        // outcome is the same either way.
        let unread = self
            .tds_read_buffer
            .buffer_length
            .saturating_sub(self.tds_read_buffer.buffer_position);
        if unread > 0 {
            warn!(
                unread_bytes = unread,
                "Discarding unread bytes while resetting the packet reader"
            );
        }
        self.tds_read_buffer
            .change_packet_size(NetworkReader::packet_size(self));
        self.tds_read_buffer.reset_to_length(0);
    }

    #[inline(always)]
    fn try_read_byte(&mut self) -> Option<u8> {
        self.tds_read_buffer.try_read_byte()
    }

    #[inline(always)]
    fn try_read_int16(&mut self) -> Option<i16> {
        self.tds_read_buffer.try_read_int16()
    }

    #[inline(always)]
    fn try_read_uint16(&mut self) -> Option<u16> {
        self.tds_read_buffer.try_read_uint16()
    }

    #[inline(always)]
    fn try_read_uint24(&mut self) -> Option<u32> {
        self.tds_read_buffer.try_read_uint24()
    }

    #[inline(always)]
    fn try_read_int32(&mut self) -> Option<i32> {
        self.tds_read_buffer.try_read_int32()
    }

    #[inline(always)]
    fn try_read_uint32(&mut self) -> Option<u32> {
        self.tds_read_buffer.try_read_uint32()
    }

    #[inline(always)]
    fn try_read_uint40(&mut self) -> Option<u64> {
        self.tds_read_buffer.try_read_uint40()
    }

    #[inline(always)]
    fn try_read_int64(&mut self) -> Option<i64> {
        self.tds_read_buffer.try_read_int64()
    }

    #[inline(always)]
    fn try_read_float32(&mut self) -> Option<f32> {
        self.tds_read_buffer.try_read_float32()
    }

    #[inline(always)]
    fn try_read_float64(&mut self) -> Option<f64> {
        self.tds_read_buffer.try_read_float64()
    }

    async fn read_byte(&mut self) -> TdsResult<u8> {
        loop {
            if let Some(value) = self.try_read_byte() {
                return Ok(value);
            }
            self.refill_for(1).await?;
        }
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        while !self.tds_read_buffer.do_we_have_enough_data(2) {
            self.refill_for(2).await?;
        }
        let result = BigEndian::read_i16(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(2)?;
        Ok(result)
    }
    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        while !self.tds_read_buffer.do_we_have_enough_data(4) {
            self.refill_for(4).await?;
        }
        let result = BigEndian::read_i32(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(4)?;
        Ok(result)
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        loop {
            if let Some(value) = self.try_read_uint40() {
                return Ok(value);
            }
            self.refill_for(5).await?;
        }
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        loop {
            if let Some(value) = self.try_read_float32() {
                return Ok(value);
            }
            self.refill_for(4).await?;
        }
    }
    async fn read_float64(&mut self) -> TdsResult<f64> {
        loop {
            if let Some(value) = self.try_read_float64() {
                return Ok(value);
            }
            self.refill_for(8).await?;
        }
    }
    async fn read_int16(&mut self) -> TdsResult<i16> {
        loop {
            if let Some(value) = self.try_read_int16() {
                return Ok(value);
            }
            self.refill_for(2).await?;
        }
    }
    async fn read_uint16(&mut self) -> TdsResult<u16> {
        loop {
            if let Some(value) = self.try_read_uint16() {
                return Ok(value);
            }
            self.refill_for(2).await?;
        }
    }
    async fn read_uint24(&mut self) -> TdsResult<u32> {
        loop {
            if let Some(value) = self.try_read_uint24() {
                return Ok(value);
            }
            self.refill_for(3).await?;
        }
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        loop {
            if let Some(value) = self.try_read_int32() {
                return Ok(value);
            }
            self.refill_for(4).await?;
        }
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        loop {
            if let Some(value) = self.try_read_uint32() {
                return Ok(value);
            }
            self.refill_for(4).await?;
        }
    }
    async fn read_int64(&mut self) -> TdsResult<i64> {
        loop {
            if let Some(value) = self.try_read_int64() {
                return Ok(value);
            }
            self.refill_for(8).await?;
        }
    }
    async fn read_uint64(&mut self) -> TdsResult<u64> {
        while !self.tds_read_buffer.do_we_have_enough_data(8) {
            self.refill_for(8).await?;
        }
        let result = LittleEndian::read_u64(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(8)?;
        Ok(result)
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        let mut total_read = 0;
        let mut length_to_read = buffer.len();
        let mut offset = 0;
        let mut empty_messages = 0u32;
        while length_to_read > 0 {
            if self.value_truncated_by_message_end(total_read > 0, length_to_read) {
                return Err(Self::past_end_of_message_error(
                    length_to_read,
                    self.tds_read_buffer.get_remaining_byte_count(),
                ));
            }
            if !self
                .tds_read_buffer
                .do_we_have_enough_data(min(self.tds_read_buffer.max_packet_size, length_to_read))
            {
                self.read_tds_packet().await?;
            }
            let available = self.tds_read_buffer.get_remaining_byte_count();

            // We can read the minimum of what is available, or the actual length needed or the packet size.
            let to_read = min(
                available,
                min(length_to_read, self.tds_read_buffer.max_packet_size - 8),
            );

            if to_read == 0 {
                if !self.tds_read_buffer.end_of_message {
                    return Err(Self::no_progress_error(length_to_read));
                }
                if total_read > 0 {
                    return Err(Self::past_end_of_message_error(length_to_read, available));
                }
                // Nothing of this value has been read yet, so the empty
                // end-of-message packet belongs to the previous message rather
                // than truncating this read. Refill and carry on, matching what
                // the scalar readers do at the same boundary. Only so many in a
                // row can be legitimate; past that nothing is advancing.
                empty_messages += 1;
                if empty_messages > Self::MAX_CONSECUTIVE_EMPTY_MESSAGES {
                    return Err(Self::stalled_on_empty_messages_error(length_to_read));
                }
                continue;
            }

            // Copy from self.working_buffer to buffer from self.buffer_position to offset.
            buffer[offset..offset + to_read].copy_from_slice(
                &self.tds_read_buffer.working_buffer[self.tds_read_buffer.buffer_position
                    ..self.tds_read_buffer.buffer_position + to_read],
            );
            offset += to_read;
            length_to_read -= to_read;
            total_read += to_read;

            self.tds_read_buffer.consume_bytes(to_read)?;
        }
        Ok(total_read)
    }

    async fn read_bytes_uninit(
        &mut self,
        buffer: &mut [std::mem::MaybeUninit<u8>],
    ) -> TdsResult<usize> {
        let mut total_read = 0;
        let mut length_to_read = buffer.len();
        let mut offset = 0;
        let mut empty_messages = 0u32;
        while length_to_read > 0 {
            if self.value_truncated_by_message_end(total_read > 0, length_to_read) {
                return Err(Self::past_end_of_message_error(
                    length_to_read,
                    self.tds_read_buffer.get_remaining_byte_count(),
                ));
            }
            if !self
                .tds_read_buffer
                .do_we_have_enough_data(min(self.tds_read_buffer.max_packet_size, length_to_read))
            {
                self.read_tds_packet().await?;
            }
            let available = self.tds_read_buffer.get_remaining_byte_count();
            let to_read = min(
                available,
                min(length_to_read, self.tds_read_buffer.max_packet_size - 8),
            );

            if to_read == 0 {
                if !self.tds_read_buffer.end_of_message {
                    return Err(Self::no_progress_error(length_to_read));
                }
                if total_read > 0 {
                    return Err(Self::past_end_of_message_error(length_to_read, available));
                }
                empty_messages += 1;
                if empty_messages > Self::MAX_CONSECUTIVE_EMPTY_MESSAGES {
                    return Err(Self::stalled_on_empty_messages_error(length_to_read));
                }
                continue;
            }

            let source = &self.tds_read_buffer.working_buffer[self.tds_read_buffer.buffer_position
                ..self.tds_read_buffer.buffer_position + to_read];
            // SAFETY: `source` and the destination are valid for `to_read`
            // non-overlapping bytes. Writing initializes those destination elements.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.as_ptr(),
                    buffer.as_mut_ptr().cast::<u8>().add(offset),
                    to_read,
                );
            }
            offset += to_read;
            length_to_read -= to_read;
            total_read += to_read;
            self.tds_read_buffer.consume_bytes(to_read)?;
        }
        Ok(total_read)
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let length: u8 = self.read_byte().await?;
        let mut result: Vec<u8> = vec![0; length as usize];
        self.read_bytes(&mut result[0..]).await?;
        Ok(result)
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        let length: u16 = self.read_uint16().await?;
        let mut result: Vec<u8> = vec![0; length as usize];
        self.read_bytes(&mut result[0..]).await?;
        Ok(result)
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        let length: u16 = self.read_uint16().await?;
        if length == LENGTH_NULL {
            return Ok(None);
        }

        let string = self
            .read_unicode_with_byte_length((length as usize) << 1)
            .await?;
        Ok(Some(string))
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        let length: u8 = self.read_byte().await?;
        let string = self
            .read_unicode_with_byte_length((length as usize) << 1)
            .await?;
        Ok(string)
    }
    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
        let result = self
            .read_unicode_with_byte_length(string_length * 2)
            .await?;
        Ok(result)
    }
    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String> {
        let mut byte_buffer: Vec<u8> = vec![0; byte_length];
        let _ = self.read_bytes(&mut byte_buffer[0..]).await?;

        // TODO: This smells like a performance problem. We are copy from a u8 vector to u16.
        // We will revisit this and fix it. Needs some rust research.
        let mut u16_buffer = Vec::with_capacity(byte_buffer.len() / 2);
        for chunk in byte_buffer.chunks(2) {
            let value = u16::from_le_bytes([chunk[0], chunk[1]]);
            u16_buffer.push(value);
        }
        // Convert byte_buffer to a unicode string
        let string =
            String::from_utf16(&u16_buffer).map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
        Ok(string)
    }

    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()> {
        let mut length_to_read = skip_count;
        let mut empty_messages = 0u32;
        while length_to_read > 0 {
            let skipped_any = length_to_read < skip_count;
            if self.value_truncated_by_message_end(skipped_any, length_to_read) {
                return Err(Self::past_end_of_message_error(
                    length_to_read,
                    self.tds_read_buffer.get_remaining_byte_count(),
                ));
            }
            if !self.tds_read_buffer.do_we_have_enough_data(min(
                self.tds_read_buffer.max_packet_size - 8,
                length_to_read,
            )) {
                self.read_tds_packet().await?;
            }
            let available = self.tds_read_buffer.get_remaining_byte_count();

            // We can read the minimum of what is available, or the actual length needed or the packet size.
            let to_read = min(
                available,
                min(length_to_read, self.tds_read_buffer.max_packet_size - 8),
            );

            if to_read == 0 {
                if !self.tds_read_buffer.end_of_message {
                    return Err(Self::no_progress_error(length_to_read));
                }
                if skipped_any {
                    return Err(Self::past_end_of_message_error(length_to_read, available));
                }
                empty_messages += 1;
                if empty_messages > Self::MAX_CONSECUTIVE_EMPTY_MESSAGES {
                    return Err(Self::stalled_on_empty_messages_error(length_to_read));
                }
                continue;
            }

            length_to_read -= to_read;
            self.tds_read_buffer.consume_bytes(to_read)?;
        }
        Ok(())
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        let attention = AttentionRequest::new();
        let mut packet_writer = attention.create_packet_writer(self.as_writer(), None, None);
        attention.serialize(&mut packet_writer).await?;
        Ok(())
    }
}

impl NetworkTransport {
    pub(crate) async fn receive_token(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Tokens> {
        let cancellable_receive_token = CancelHandle::run_until_cancelled(
            cancel_handle,
            receive_token_internal(self, &*PARSER_REGISTRY, context),
        );
        let token_result = match remaining_request_timeout.as_ref() {
            Some(remaining_request_timeout) => {
                match timeout(*remaining_request_timeout, cancellable_receive_token).await {
                    Ok(result) => result,
                    Err(elapsed) => Err(TimeoutError(TimeoutErrorType::Elapsed(elapsed))),
                }
            }
            None => cancellable_receive_token.await,
        };

        match &token_result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    Box::pin(self.cancel_read_stream_and_wait()).await;
                }
                _ => {}
            },
        }
        token_result
    }

    pub(crate) async fn receive_row_into<W>(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut W,
    ) -> TdsResult<RowReadResult>
    where
        W: RowWriter + Send + ?Sized,
    {
        // `self` is the packet reader, so the scratch slot has to be moved out
        // for the duration of the read and put back afterwards. The restore
        // below must stay unconditional, and no `?` may be introduced between
        // these two points: an early return would drop the cached bitmap and
        // silently cost an allocation on every subsequent row.
        let mut nbc_bitmap_scratch = self.nbc_bitmap_scratch.take();
        let result = await_within_request_timeout!(
            remaining_request_timeout,
            CancelHandle::run_until_cancelled(
                cancel_handle,
                receive_row_into_internal(
                    self,
                    &*PARSER_REGISTRY,
                    context,
                    plan,
                    writer,
                    &mut nbc_bitmap_scratch,
                ),
            )
        );
        self.nbc_bitmap_scratch = nbc_bitmap_scratch;

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    Box::pin(self.cancel_read_stream_and_wait()).await;
                }
                _ => {}
            },
        }
        result
    }

    pub(crate) async fn receive_row_header(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<RowHeader> {
        // Same take/restore as `receive_row_into`: unconditional restore, no `?`
        // between the two points.
        let mut nbc_bitmap_scratch = self.nbc_bitmap_scratch.take();
        let result = await_within_request_timeout!(
            remaining_request_timeout,
            CancelHandle::run_until_cancelled(
                cancel_handle,
                receive_row_header_internal(
                    self,
                    &*PARSER_REGISTRY,
                    context,
                    &mut nbc_bitmap_scratch,
                ),
            )
        );
        self.nbc_bitmap_scratch = nbc_bitmap_scratch;

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    Box::pin(self.cancel_read_stream_and_wait()).await;
                }
                _ => {}
            },
        }
        result
    }

    pub(crate) async fn resume_row_into<W>(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut W,
    ) -> TdsResult<RowReadResult>
    where
        W: RowWriter + Send + ?Sized,
    {
        let result = await_within_request_timeout!(
            remaining_request_timeout,
            CancelHandle::run_until_cancelled(
                cancel_handle,
                resume_row_into_internal(self, pause_state, plan, writer),
            )
        );

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    Box::pin(self.cancel_read_stream_and_wait()).await;
                }
                _ => {}
            },
        }
        result
    }

    pub(crate) async fn read_active_plp_bytes(
        &mut self,
        plp_state: &mut PlpPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        out: &mut [u8],
    ) -> TdsResult<usize> {
        let result = await_within_request_timeout!(
            remaining_request_timeout,
            CancelHandle::run_until_cancelled(
                cancel_handle,
                read_active_plp_bytes_internal(self, plp_state, out),
            )
        );

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    Box::pin(self.cancel_read_stream_and_wait()).await;
                }
                _ => {}
            },
        }
        result
    }
}

#[async_trait]
impl TdsTokenStreamReader for NetworkTransport {
    async fn receive_token(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<Tokens> {
        NetworkTransport::receive_token(self, context, remaining_request_timeout, cancel_handle)
            .await
    }

    async fn receive_row_into(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        NetworkTransport::receive_row_into(
            self,
            context,
            remaining_request_timeout,
            cancel_handle,
            plan,
            writer,
        )
        .await
    }

    async fn receive_row_header(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> TdsResult<RowHeader> {
        NetworkTransport::receive_row_header(
            self,
            context,
            remaining_request_timeout,
            cancel_handle,
        )
        .await
    }

    async fn resume_row_into(
        &mut self,
        pause_state: RowPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        plan: ColumnPolicy,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        NetworkTransport::resume_row_into(
            self,
            pause_state,
            remaining_request_timeout,
            cancel_handle,
            plan,
            writer,
        )
        .await
    }

    async fn read_active_plp_bytes(
        &mut self,
        plp_state: &mut PlpPauseState,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        out: &mut [u8],
    ) -> TdsResult<usize> {
        NetworkTransport::read_active_plp_bytes(
            self,
            plp_state,
            remaining_request_timeout,
            cancel_handle,
            out,
        )
        .await
    }
}

#[async_trait]
impl crate::connection::transport::tds_transport::TdsTransport for NetworkTransport {
    fn as_writer(&mut self) -> &mut dyn NetworkWriter {
        self
    }

    fn reset_reader(&mut self) {
        self.tds_read_buffer.change_packet_size(self.packet_size);
        self.tds_read_buffer.reset_to_length(0);
    }

    fn packet_size(&self) -> u32 {
        self.packet_size
    }

    async fn close_transport(&mut self) -> TdsResult<()> {
        if let Some(stream) = self.stream.as_mut() {
            stream.shutdown().await?;
        }
        // Drop the stream and record the closed state. `connection_known_dead()`
        // surfaces this to connection pools so they don't reuse a closed
        // connection; the live `is_connection_dead()` poll also reports dead
        // once the stream is gone.
        self.stream = None;
        self.known_dead = true;
        Ok(())
    }

    /// Send an attention packet and wait for acknowledgment with a timeout.
    ///
    /// This implements the attention sending flow with a configurable timeout:
    /// 1. Send MT_ATTN (0x06) packet to the server
    /// 2. Wait for DONE token with ATTN (0x0020) status flag
    /// 3. If no acknowledgment within timeout, return false
    ///
    /// `attention_timeout` covers the whole flow, not just step 2. Writing the
    /// packet parks until the socket accepts it, so a peer that has stopped
    /// reading stalls the send and the acknowledgement deadline would never be
    /// armed. Both steps therefore share one deadline, and the drain gets
    /// whatever the send left of it.
    ///
    /// This is used by bulk copy timeout handling to implement the 5-second
    /// attention ACK timeout per SqlClient behavior.
    async fn send_attention_with_timeout(
        &mut self,
        attention_timeout: Duration,
    ) -> TdsResult<bool> {
        self.send_attention_and_wait(attention_timeout).await
    }

    fn is_connection_dead(&self) -> bool {
        match &self.stream {
            Some(stream) => stream.is_connection_dead(),
            None => true,
        }
    }

    fn connection_known_dead(&self) -> bool {
        self.known_dead
    }

    fn mark_known_dead(&mut self) {
        self.known_dead = true;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*; // Brings in NetworkTransport, SslHandler, StreamRecoverer, etc.
    use crate::connection::client_context::ClientContext;
    use crate::connection::transport::network_transport::Stream;
    use crate::connection::transport::ssl_handler::SslHandler;
    use crate::core::EncryptionOptions;
    use crate::message::messages::PacketType;
    use crate::test_packet_support::{
        TestPacketBuilder, build_duplex_transport, create_network_transport_with_chunked_data,
        create_network_transport_with_data, create_network_transport_with_live_peer,
        create_network_transport_with_live_peer_capturing_writes, encode_utf16_le,
    };
    use bytes::Bytes;
    use futures::SinkExt;
    use futures::StreamExt;
    use rand::Rng;
    use tokio::io::{DuplexStream, duplex};
    use tokio_util::codec::{BytesCodec, FramedRead, FramedWrite};

    // The choice of 8192 is large enough for sending data. This stream should have a buffer large enough for send.
    // The test would keep the payload lower than this size to make sure that the duplex stream can handle it.
    pub(crate) const MAX_BUFFER_SIZE: usize = 8192;

    impl Stream for DuplexStream {
        fn tls_handshake_starting(&mut self) {
            // No-op for duplex streams
        }

        fn tls_handshake_completed(&mut self) {
            // No-op for duplex streams
        }
    }

    pub(crate) fn create_readable_network_transport(
        context: &ClientContext,
    ) -> (NetworkTransport, DuplexStream) {
        let (client_side, server_side) = duplex(MAX_BUFFER_SIZE);

        let ssl_handler = SslHandler {
            server_host_name: context.transport_context.get_server_name().clone(),
            encryption_options: context.encryption_options.clone(),
        };

        (
            NetworkTransport::new(
                Box::new(client_side),
                ssl_handler,
                context.packet_size as u32,
                context.encryption_options.mode,
                false,
            ),
            server_side,
        )
    }

    #[tokio::test]
    async fn test_network_transport_send() {
        let context = ClientContext {
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };
        let (mut transport, server_side) = create_readable_network_transport(&context);

        // Fill data_to_send with random values
        let mut rng = rand::rng();
        let data_vector: Vec<u8> = (0..MAX_BUFFER_SIZE).map(|_| rng.random()).collect();

        // Setup the reader to read the data.
        let mut framed_reader = FramedRead::new(server_side, BytesCodec::new());

        // Send the data and read it from the other end of the pipe.
        let result = transport.send(&data_vector[..]).await;
        match result {
            Ok(_) => {}
            Err(e) => panic!("Error sending data: {e}"),
        }

        let received = framed_reader
            .next()
            .await
            .expect("No data")
            .expect("Decode error");

        assert_eq!(received.as_ref(), &data_vector[..]);
    }

    /// Test that TdsTransport::reset_reader() properly resizes the buffer after packet size change.
    ///
    /// This test validates the fix for the buffer overflow bug that occurred when:
    /// 1. Connection starts with packet_size = 4096 (buffer = 8192 bytes)
    /// 2. Login negotiates packet_size = 8000
    /// 3. reset_reader() is called before first command
    /// 4. Without the fix, buffer stayed at 8192 bytes, causing panic on 8000-byte packets
    ///
    /// The fix ensures reset_reader() calls change_packet_size() to resize the buffer.
    #[test]
    fn test_tds_transport_reset_reader_resizes_buffer_after_packet_size_change() {
        use crate::connection::transport::tds_transport::TdsTransport;

        let initial_packet_size: u32 = 4096;
        let negotiated_packet_size: u32 = 8000;

        let context = ClientContext {
            packet_size: initial_packet_size as u16,
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };

        let (mut transport, _server_side) = create_readable_network_transport(&context);

        // Verify initial state: buffer sized for 4096 packets
        assert_eq!(transport.packet_size, initial_packet_size);
        assert_eq!(transport.tds_read_buffer.working_buffer.len(), 8192); // 4096 * 2
        assert_eq!(transport.tds_read_buffer.max_packet_size, 4096);

        // Simulate packet size negotiation (what happens after login)
        transport.packet_size = negotiated_packet_size;

        // Call reset_reader via TdsTransport trait - this is what TdsClient does
        TdsTransport::reset_reader(&mut transport);

        // Verify the fix: buffer should now be sized for 8000-byte packets
        assert_eq!(
            transport.tds_read_buffer.working_buffer.len(),
            16000,
            "Buffer should be resized to 8000 * 2 = 16000 bytes after reset_reader()"
        );
        assert_eq!(
            transport.tds_read_buffer.max_packet_size, 8000,
            "max_packet_size should be updated to 8000"
        );
        assert_eq!(
            transport.tds_read_buffer.buffer_position, 0,
            "buffer_position should be reset to 0"
        );
        assert_eq!(
            transport.tds_read_buffer.buffer_length, 0,
            "buffer_length should be reset to 0"
        );
    }

    /// Test that reset_reader() is idempotent when packet size hasn't changed.
    #[test]
    fn test_tds_transport_reset_reader_same_size_preserves_buffer() {
        use crate::connection::transport::tds_transport::TdsTransport;

        let packet_size: u32 = 4096;

        let context = ClientContext {
            packet_size: packet_size as u16,
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };

        let (mut transport, _server_side) = create_readable_network_transport(&context);

        // Verify initial buffer size
        let initial_buffer_len = transport.tds_read_buffer.working_buffer.len();
        assert_eq!(initial_buffer_len, 8192);

        // Call reset_reader - packet size hasn't changed
        TdsTransport::reset_reader(&mut transport);

        // Buffer size should remain the same (no unnecessary reallocation)
        assert_eq!(
            transport.tds_read_buffer.working_buffer.len(),
            initial_buffer_len
        );
        assert_eq!(transport.tds_read_buffer.max_packet_size, 4096);
    }

    /// Test that get_new_tds_packet correctly handles multiple TDS packets arriving in a single read.
    ///
    /// This test validates the pending_bytes fix:
    /// - When a single network read returns data for multiple TDS packets (e.g., due to TCP coalescing
    ///   or Named Pipes message mode), the extra bytes beyond the current packet must be preserved.
    /// - Without the fix, these extra bytes would be lost, causing data corruption or hangs.
    ///
    /// The test simulates:
    /// 1. Two complete TDS packets (32 bytes each) sent together in one write
    /// 2. The transport should correctly read both packets, using pending_bytes to track the second
    #[tokio::test]
    async fn test_get_new_tds_packet_handles_multiple_packets_in_single_read() {
        use byteorder::{BigEndian, ByteOrder};

        // Use a small packet size for testing
        let packet_size: u32 = 512;

        let context = ClientContext {
            packet_size: packet_size as u16,
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };

        let (mut transport, server_side) = create_readable_network_transport(&context);

        // Create two TDS packets with distinct payload patterns
        // TDS packet header is 8 bytes:
        //   [0]: packet type
        //   [1]: status (0x01 = EOM)
        //   [2-3]: length (big endian, includes header)
        //   [4-5]: SPID
        //   [6]: packet ID
        //   [7]: window

        let packet1_payload = vec![0xAA; 24]; // 24 bytes of 0xAA
        let packet2_payload = vec![0xBB; 24]; // 24 bytes of 0xBB

        let packet1_total_len: u16 = 8 + packet1_payload.len() as u16; // 32 bytes
        let packet2_total_len: u16 = 8 + packet2_payload.len() as u16; // 32 bytes

        // Build packet 1
        let mut packet1 = vec![0u8; packet1_total_len as usize];
        packet1[0] = 0x04; // TDS_TABULAR_RESULT
        packet1[1] = 0x00; // Not EOM (more packets coming)
        BigEndian::write_u16(&mut packet1[2..4], packet1_total_len);
        packet1[4] = 0x00; // SPID low
        packet1[5] = 0x00; // SPID high
        packet1[6] = 0x01; // Packet ID
        packet1[7] = 0x00; // Window
        packet1[8..].copy_from_slice(&packet1_payload);

        // Build packet 2
        let mut packet2 = vec![0u8; packet2_total_len as usize];
        packet2[0] = 0x04; // TDS_TABULAR_RESULT
        packet2[1] = 0x01; // EOM (end of message)
        BigEndian::write_u16(&mut packet2[2..4], packet2_total_len);
        packet2[4] = 0x00;
        packet2[5] = 0x00;
        packet2[6] = 0x02; // Packet ID
        packet2[7] = 0x00;
        packet2[8..].copy_from_slice(&packet2_payload);

        // Concatenate both packets - simulating TCP coalescing or a transport
        // that returns multiple packets in a single read
        let mut combined_data = packet1.clone();
        combined_data.extend_from_slice(&packet2);

        // Send both packets at once
        let mut framed_writer = FramedWrite::new(server_side, BytesCodec::new());
        framed_writer
            .send(Bytes::copy_from_slice(&combined_data))
            .await
            .expect("Failed to send test data");

        // First call to get_new_tds_packet should return packet 1
        let size1 = transport
            .get_new_tds_packet()
            .await
            .expect("Failed to read first packet");
        assert_eq!(
            size1, packet1_total_len as usize,
            "First packet size mismatch"
        );

        // Verify packet 1 payload is correct (starts at buffer_length which is 0 initially)
        let packet1_in_buffer = &transport.tds_read_buffer.working_buffer[0..size1];
        assert_eq!(
            packet1_in_buffer,
            &packet1[..],
            "First packet content mismatch"
        );

        // Check that pending_bytes was set correctly for the second packet
        assert_eq!(
            transport.tds_read_buffer.pending_bytes, packet2_total_len as usize,
            "pending_bytes should track the second packet"
        );

        // Reset buffer state to simulate processing the first packet
        transport.tds_read_buffer.buffer_length = 0;
        transport.tds_read_buffer.buffer_position = 0;

        // Second call to get_new_tds_packet should return packet 2 from pending bytes
        let size2 = transport
            .get_new_tds_packet()
            .await
            .expect("Failed to read second packet");
        assert_eq!(
            size2, packet2_total_len as usize,
            "Second packet size mismatch"
        );

        // Verify packet 2 payload is correct
        let packet2_in_buffer = &transport.tds_read_buffer.working_buffer[0..size2];
        assert_eq!(
            packet2_in_buffer,
            &packet2[..],
            "Second packet content mismatch"
        );

        // After reading both packets, pending_bytes should be 0
        assert_eq!(
            transport.tds_read_buffer.pending_bytes, 0,
            "pending_bytes should be 0 after consuming all data"
        );
    }

    /// Test that get_new_tds_packet works correctly when packets arrive one at a time.
    /// This is the normal case and should work with or without the pending_bytes fix.
    #[tokio::test]
    async fn test_get_new_tds_packet_single_packet_per_read() {
        use byteorder::{BigEndian, ByteOrder};

        let packet_size: u32 = 512;

        let context = ClientContext {
            packet_size: packet_size as u16,
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };

        let (mut transport, server_side) = create_readable_network_transport(&context);

        // Create a single TDS packet
        let payload = vec![0xCC; 100];
        let total_len: u16 = 8 + payload.len() as u16;

        let mut packet = vec![0u8; total_len as usize];
        packet[0] = 0x04; // TDS_TABULAR_RESULT
        packet[1] = 0x01; // EOM
        BigEndian::write_u16(&mut packet[2..4], total_len);
        packet[4] = 0x00;
        packet[5] = 0x00;
        packet[6] = 0x01;
        packet[7] = 0x00;
        packet[8..].copy_from_slice(&payload);

        // Send just one packet
        let mut framed_writer = FramedWrite::new(server_side, BytesCodec::new());
        framed_writer
            .send(Bytes::copy_from_slice(&packet))
            .await
            .expect("Failed to send test data");

        // Read the packet
        let size = transport
            .get_new_tds_packet()
            .await
            .expect("Failed to read packet");
        assert_eq!(size, total_len as usize);

        // Verify content
        let packet_in_buffer = &transport.tds_read_buffer.working_buffer[0..size];
        assert_eq!(packet_in_buffer, &packet[..]);

        // No pending bytes
        assert_eq!(transport.tds_read_buffer.pending_bytes, 0);
    }

    /// Test that demonstrates the multi-packet read bug WITHOUT checking internal fields.
    ///
    /// This test verifies the observable behavior: when two TDS packets arrive in a single
    /// network read, BOTH packets must be readable. This test does NOT check `pending_bytes`
    /// or any other internal tracking fields - it only verifies the actual packet data.
    ///
    /// Bug demonstration:
    /// - UNFIXED CODE: The second `get_new_tds_packet()` call will HANG indefinitely because
    ///   the extra bytes from the first read were discarded. The read() call waits for new
    ///   data that will never arrive.
    /// - FIXED CODE: Both packets are correctly read because pending bytes are preserved.
    ///
    /// This test uses a timeout to detect the hang condition in unfixed code.
    #[tokio::test]
    async fn test_multi_packet_coalescing_behavior_only() {
        use byteorder::{BigEndian, ByteOrder};
        use tokio::time::{Duration, timeout};

        let packet_size: u32 = 512;

        let context = ClientContext {
            packet_size: packet_size as u16,
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };

        let (mut transport, server_side) = create_readable_network_transport(&context);

        // Create two TDS packets with DIFFERENT payloads so we can verify correct data
        let packet1_payload: Vec<u8> = (0..24).map(|i| i as u8).collect(); // 0, 1, 2, ... 23
        let packet2_payload: Vec<u8> = (0..24).map(|i| (100 + i) as u8).collect(); // 100, 101, ... 123

        let packet1_total_len: u16 = 8 + packet1_payload.len() as u16;
        let packet2_total_len: u16 = 8 + packet2_payload.len() as u16;

        // Build packet 1
        let mut packet1 = vec![0u8; packet1_total_len as usize];
        packet1[0] = 0x04; // TDS_TABULAR_RESULT
        packet1[1] = 0x00; // Not EOM
        BigEndian::write_u16(&mut packet1[2..4], packet1_total_len);
        packet1[6] = 0x01; // Packet ID = 1
        packet1[8..].copy_from_slice(&packet1_payload);

        // Build packet 2
        let mut packet2 = vec![0u8; packet2_total_len as usize];
        packet2[0] = 0x04; // TDS_TABULAR_RESULT
        packet2[1] = 0x01; // EOM
        BigEndian::write_u16(&mut packet2[2..4], packet2_total_len);
        packet2[6] = 0x02; // Packet ID = 2
        packet2[8..].copy_from_slice(&packet2_payload);

        // Send BOTH packets in a single write - simulating TCP coalescing
        let mut combined_data = packet1.clone();
        combined_data.extend_from_slice(&packet2);

        let mut framed_writer = FramedWrite::new(server_side, BytesCodec::new());
        framed_writer
            .send(Bytes::copy_from_slice(&combined_data))
            .await
            .expect("Failed to send test data");

        // ============================================================
        // READ FIRST PACKET - should always work
        // ============================================================
        let size1 = transport
            .get_new_tds_packet()
            .await
            .expect("Failed to read first packet");

        assert_eq!(size1, packet1_total_len as usize, "First packet size wrong");

        // Verify first packet content (especially the payload bytes 8..32)
        // Copy data to owned values to avoid borrow issues
        let read_packet1_id = transport.tds_read_buffer.working_buffer[6];
        let read_packet1_payload: Vec<u8> =
            transport.tds_read_buffer.working_buffer[8..size1].to_vec();

        assert_eq!(
            &read_packet1_payload[..],
            &packet1_payload[..],
            "First packet payload corrupted"
        );
        assert_eq!(read_packet1_id, 0x01, "First packet should have ID=1");

        // Reset buffer to prepare for second packet read.
        // We use reset_to_length(0) to properly reset position and length via the API.
        // Note: pending_bytes/pending_bytes_offset are preserved - they track data
        // that hasn't been processed yet (the second packet).
        transport.tds_read_buffer.reset_to_length(0);

        // ============================================================
        // READ SECOND PACKET - THIS IS WHERE THE BUG MANIFESTS
        // ============================================================
        // With UNFIXED code: This will HANG because the second packet's bytes
        // were discarded, and read() waits for data that never comes.
        //
        // With FIXED code: The pending bytes are used, no network read needed.

        let read_result = timeout(
            Duration::from_millis(500), // 500ms is plenty for in-memory data
            transport.get_new_tds_packet(),
        )
        .await;

        // Check if we timed out (BUG) or got a result (FIXED)
        let size2 = match read_result {
            Ok(Ok(size)) => size,
            Ok(Err(e)) => panic!("Error reading second packet: {:?}", e),
            Err(_elapsed) => {
                panic!(
                    "BUG DETECTED: Timed out waiting for second packet!\n\
                     The second packet's bytes were discarded after the first read.\n\
                     This is the multi-packet coalescing bug."
                );
            }
        };

        assert_eq!(
            size2, packet2_total_len as usize,
            "Second packet size wrong"
        );

        // Verify second packet content - this catches data corruption bugs
        let read_packet2_id = transport.tds_read_buffer.working_buffer[6];
        let read_packet2_payload: Vec<u8> =
            transport.tds_read_buffer.working_buffer[8..size2].to_vec();

        assert_eq!(
            &read_packet2_payload[..],
            &packet2_payload[..],
            "Second packet payload corrupted - got wrong data!"
        );
        assert_eq!(read_packet2_id, 0x02, "Second packet should have ID=2");
    }

    /// Test that get_new_tds_packet returns an error (not panic) when pending_bytes
    /// fields contain invalid values that would cause an out-of-bounds access.
    ///
    /// This protects against corrupted or malicious packet data that could cause
    /// the pending_bytes_offset or pending_bytes to point outside the buffer.
    ///
    /// NOTE: This test intentionally mutates internal buffer fields directly to simulate
    /// corrupted state that cannot occur through normal API usage. This is necessary
    /// because we're testing defensive bounds checking against malformed wire data.
    #[tokio::test]
    async fn test_get_new_tds_packet_bounds_check_on_pending_bytes() {
        let packet_size: u32 = 512;

        let context = ClientContext {
            packet_size: packet_size as u16,
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };

        let (mut transport, _server_side) = create_readable_network_transport(&context);

        // Simulate corrupted state: pending_bytes_offset points way past buffer end.
        // Direct field mutation is intentional here - we're testing defense against
        // invalid state that could only arise from malformed packet data.
        let buffer_len = transport.tds_read_buffer.working_buffer.len();
        transport.tds_read_buffer.pending_bytes = 100;
        transport.tds_read_buffer.pending_bytes_offset = buffer_len + 1000; // Way out of bounds

        // This should return an error, not panic
        let result = transport.get_new_tds_packet().await;
        assert!(
            result.is_err(),
            "Expected error for out-of-bounds pending_bytes_offset"
        );

        let err = result.unwrap_err();
        assert!(
            matches!(err, crate::error::Error::ProtocolError(_)),
            "Expected ProtocolError, got {:?}",
            err
        );

        // Reset buffer to clean state, then inject another invalid scenario.
        // Direct mutation is intentional - testing defense against malformed data.
        transport.tds_read_buffer.reset_to_length(0);
        transport.tds_read_buffer.pending_bytes = buffer_len + 500; // Larger than buffer
        transport.tds_read_buffer.pending_bytes_offset = 0;

        let result = transport.get_new_tds_packet().await;
        assert!(
            result.is_err(),
            "Expected error for oversized pending_bytes"
        );

        // Reset buffer, then test: src range valid but dest would overflow.
        // We set buffer_length near the end so base_offset leaves no room for pending bytes.
        transport.tds_read_buffer.reset_to_length(buffer_len - 10);
        transport.tds_read_buffer.pending_bytes = 100; // 100 bytes won't fit at dest
        transport.tds_read_buffer.pending_bytes_offset = 0; // src is valid

        let result = transport.get_new_tds_packet().await;
        assert!(
            result.is_err(),
            "Expected error when dest range exceeds buffer"
        );
    }

    /// Test that get_new_tds_packet validates packet_size_from_header from the wire.
    ///
    /// A malicious or corrupted server could send invalid packet lengths that would
    /// cause panics or incorrect state. This test verifies we return errors instead.
    #[tokio::test]
    async fn test_get_new_tds_packet_validates_packet_length_from_header() {
        use byteorder::{BigEndian, ByteOrder};

        let packet_size: u32 = 512;

        let context = ClientContext {
            packet_size: packet_size as u16,
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };

        // Test 1: Packet length smaller than header size (8 bytes)
        {
            let (mut transport, server_side) = create_readable_network_transport(&context);

            // Create a malformed packet with length = 4 (less than 8-byte header)
            let mut malformed_packet = vec![0u8; 16];
            malformed_packet[0] = 0x04; // TDS_TABULAR_RESULT
            malformed_packet[1] = 0x01; // EOM
            BigEndian::write_u16(&mut malformed_packet[2..4], 4); // Invalid: only 4 bytes
            malformed_packet[6] = 0x01;

            let mut framed_writer = FramedWrite::new(server_side, BytesCodec::new());
            framed_writer
                .send(Bytes::copy_from_slice(&malformed_packet))
                .await
                .expect("Failed to send test data");

            let result = transport.get_new_tds_packet().await;
            assert!(
                result.is_err(),
                "Expected error for packet length < header size"
            );
            let err = result.unwrap_err();
            assert!(
                matches!(err, crate::error::Error::ProtocolError(_)),
                "Expected ProtocolError, got {:?}",
                err
            );
        }

        // Test 2: Packet length larger than negotiated max_packet_size
        {
            let (mut transport, server_side) = create_readable_network_transport(&context);

            // Create a packet claiming to be 60000 bytes (way larger than 512 max)
            let mut oversized_packet = vec![0u8; 16];
            oversized_packet[0] = 0x04;
            oversized_packet[1] = 0x01;
            BigEndian::write_u16(&mut oversized_packet[2..4], 60000); // Way too large
            oversized_packet[6] = 0x01;

            let mut framed_writer = FramedWrite::new(server_side, BytesCodec::new());
            framed_writer
                .send(Bytes::copy_from_slice(&oversized_packet))
                .await
                .expect("Failed to send test data");

            let result = transport.get_new_tds_packet().await;
            assert!(
                result.is_err(),
                "Expected error for packet length > max_packet_size"
            );
            let err = result.unwrap_err();
            assert!(
                matches!(err, crate::error::Error::ProtocolError(_)),
                "Expected ProtocolError, got {:?}",
                err
            );
        }

        // Test 3: Valid packet should still work
        {
            let (mut transport, server_side) = create_readable_network_transport(&context);

            let payload = vec![0xAA; 24];
            let total_len: u16 = 8 + payload.len() as u16;
            let mut valid_packet = vec![0u8; total_len as usize];
            valid_packet[0] = 0x04;
            valid_packet[1] = 0x01;
            BigEndian::write_u16(&mut valid_packet[2..4], total_len);
            valid_packet[6] = 0x01;
            valid_packet[8..].copy_from_slice(&payload);

            let mut framed_writer = FramedWrite::new(server_side, BytesCodec::new());
            framed_writer
                .send(Bytes::copy_from_slice(&valid_packet))
                .await
                .expect("Failed to send test data");

            let result = transport.get_new_tds_packet().await;
            assert!(result.is_ok(), "Valid packet should succeed");
            assert_eq!(result.unwrap(), total_len as usize);
        }
    }

    mod is_connection_dead_tests {
        use super::*;
        use crate::connection::transport::tds_transport::TdsTransport;
        use tokio::net::TcpListener;

        #[tokio::test]
        async fn tcp_stream_alive_returns_false() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).await.unwrap();
            let _server = listener.accept().await.unwrap();

            assert!(!client.is_connection_dead());
        }

        #[tokio::test]
        async fn tcp_stream_server_closed_returns_true() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).await.unwrap();
            let (server, _) = listener.accept().await.unwrap();

            // Close the server side
            drop(server);

            // Give the OS a moment to propagate the TCP FIN
            tokio::time::sleep(Duration::from_millis(50)).await;

            assert!(client.is_connection_dead());
        }

        #[tokio::test]
        async fn network_transport_no_stream_returns_true() {
            let ssl_handler = SslHandler {
                server_host_name: "test".to_string(),
                encryption_options: EncryptionOptions::new(),
            };

            let mut transport = NetworkTransport::new(
                Box::new(tokio::io::duplex(64).0),
                ssl_handler,
                4096,
                EncryptionSetting::Strict,
                false,
            );

            // Remove the stream to simulate a closed transport
            transport.stream = None;

            assert!(transport.is_connection_dead());
        }

        #[tokio::test]
        async fn network_transport_alive_tcp_returns_false() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).await.unwrap();
            let _server = listener.accept().await.unwrap();

            let ssl_handler = SslHandler {
                server_host_name: "test".to_string(),
                encryption_options: EncryptionOptions::new(),
            };

            let transport = NetworkTransport::new(
                Box::new(client),
                ssl_handler,
                4096,
                EncryptionSetting::Strict,
                false,
            );

            assert!(!transport.is_connection_dead());
        }

        #[tokio::test]
        async fn network_transport_dead_tcp_returns_true() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).await.unwrap();
            let (server, _) = listener.accept().await.unwrap();

            let ssl_handler = SslHandler {
                server_host_name: "test".to_string(),
                encryption_options: EncryptionOptions::new(),
            };

            let transport = NetworkTransport::new(
                Box::new(client),
                ssl_handler,
                4096,
                EncryptionSetting::Strict,
                false,
            );

            drop(server);
            tokio::time::sleep(Duration::from_millis(50)).await;

            assert!(transport.is_connection_dead());
        }

        #[tokio::test]
        async fn duplex_stream_uses_default_false() {
            // DuplexStream doesn't override is_connection_dead, so it returns false (default)
            let (client_side, _server_side) = duplex(64);
            assert!(!client_side.is_connection_dead());
        }

        #[tokio::test]
        async fn box_dyn_stream_delegates_to_inner() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = TcpStream::connect(addr).await.unwrap();
            let (server, _) = listener.accept().await.unwrap();

            let boxed: Box<dyn Stream> = Box::new(client);

            // Alive
            assert!(!boxed.is_connection_dead());

            drop(server);
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Dead
            assert!(boxed.is_connection_dead());
        }
    }

    mod payload_eof_marks_dead_tests {
        use super::*;
        use crate::connection::transport::tds_transport::TdsTransport;
        use tokio::io::AsyncWriteExt;

        /// A packet whose header declares more bytes than are delivered before
        /// the peer closes the socket must mark the connection known-dead, so the
        /// cached liveness check (`connection_known_dead`) reports it without a
        /// fresh socket probe.
        #[tokio::test]
        async fn partial_packet_then_eof_sets_known_dead() {
            let (client_side, mut server_side) = duplex(MAX_BUFFER_SIZE);

            let ssl_handler = SslHandler {
                server_host_name: "test".to_string(),
                encryption_options: EncryptionOptions::new(),
            };

            let mut transport = NetworkTransport::new(
                Box::new(client_side),
                ssl_handler,
                4096,
                EncryptionSetting::On,
                false,
            );

            // Send a complete 8-byte TDS header that declares a 16-byte packet,
            // then close the writer without sending the 8-byte payload. The header
            // loop completes; the payload loop hits EOF.
            let header = [0x04u8, 0x01, 0x00, 0x10, 0x00, 0x00, 0x01, 0x00];
            server_side.write_all(&header).await.unwrap();
            server_side.flush().await.unwrap();
            drop(server_side); // signal EOF to the reader

            let result = transport.read_tds_packet().await;

            assert!(
                result.is_err(),
                "reading a truncated packet must return an error"
            );
            assert!(
                transport.connection_known_dead(),
                "payload EOF must mark the connection known-dead"
            );
        }
    }

    fn generate_random_bytes(length: usize) -> Vec<u8> {
        let mut rng = rand::rng();
        let mut bytes = vec![0u8; length];
        rng.fill(&mut bytes[..]);
        bytes
    }

    #[tokio::test]
    async fn test_read_byte() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let byte_value = rand::rng().random::<u8>();
        let builder = binding.append_byte(byte_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_byte().await.unwrap(), byte_value);
    }

    #[tokio::test]
    async fn test_read_int16() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let int16_value = rand::rng().random::<i16>();
        let builder = binding.append_i16(int16_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_int16().await.unwrap(), int16_value);
    }

    #[tokio::test]
    async fn test_read_uint16() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let uint16_value = rand::rng().random::<u16>();
        let builder = binding.append_u16(uint16_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_uint16().await.unwrap(), uint16_value);
    }

    #[tokio::test]
    async fn test_read_int32() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let int32_value = rand::rng().random::<i32>();
        let builder = binding.append_i32(int32_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_int32().await.unwrap(), int32_value);
    }

    #[tokio::test]
    async fn test_read_uint32() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let uint32_value = rand::rng().random::<u32>();
        let builder = binding.append_u32(uint32_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_uint32().await.unwrap(), uint32_value);
    }

    #[tokio::test]
    async fn test_read_int64() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let int64_value = rand::rng().random::<i64>();
        let builder = binding.append_i64(int64_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_int64().await.unwrap(), int64_value);
    }

    #[tokio::test]
    async fn test_read_uint64() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let uint64_value = rand::rng().random::<u64>();
        let builder = binding.append_u64(uint64_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_uint64().await.unwrap(), uint64_value);
    }

    #[tokio::test]
    async fn test_read_float32() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let float32_value = rand::rng().random::<f32>();
        let builder = binding.append_f32(float32_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_float32().await.unwrap(), float32_value);
    }

    #[tokio::test]
    async fn test_read_float64() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let float64_value = rand::rng().random::<f64>();
        let builder = binding.append_f64(float64_value);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_float64().await.unwrap(), float64_value);
    }

    #[tokio::test]
    async fn test_read_unicode() {
        let unicode_string = "Hello, world";
        let char_count = unicode_string.encode_utf16().count();

        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let builder = binding.append_bytes(&encode_utf16_le(unicode_string));

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(
            reader.read_unicode(char_count).await.unwrap(),
            unicode_string
        );
    }

    #[tokio::test]
    async fn test_read_bytes() {
        let bytes_len = 2000;
        let bytes = generate_random_bytes(bytes_len);
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let builder = binding.append_bytes(&bytes);

        let mut reader = create_network_transport_with_data(&builder.build());

        let mut buffer = vec![0; bytes_len];
        assert_eq!(reader.read_bytes(&mut buffer).await.unwrap(), bytes_len);
        assert_eq!(buffer, bytes);
    }

    #[tokio::test]
    async fn test_read_u8_varbyte() {
        let bytes_len: u8 = 200;
        let data_bytes = generate_random_bytes(bytes_len as usize);
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        binding.append_byte(bytes_len);
        let builder = binding.append_bytes(&data_bytes);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_u8_varbyte().await.unwrap(), data_bytes);
    }

    #[tokio::test]
    async fn test_read_u16_varbyte() {
        let bytes_len: u16 = 1000;
        let data_bytes = generate_random_bytes(bytes_len as usize);
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        binding.append_u16(bytes_len);
        let builder = binding.append_bytes(&data_bytes);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_u16_varbyte().await.unwrap(), data_bytes);
    }

    #[tokio::test]
    async fn test_read_varchar_u16_length() {
        let unicode_string = "Hello, world";
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        binding.append_u16(unicode_string.encode_utf16().count() as u16);
        let builder = binding.append_bytes(&encode_utf16_le(unicode_string));

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(
            reader.read_varchar_u16_length().await.unwrap(),
            Some(unicode_string.to_string())
        );
    }

    #[tokio::test]
    async fn test_read_varchar_u16_length_null() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let builder = binding.append_u16(LENGTH_NULL);

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(reader.read_varchar_u16_length().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_read_varchar_u8_length() {
        let unicode_string = "Hello, world";
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        binding.append_byte(unicode_string.encode_utf16().count() as u8);
        let builder = binding.append_bytes(&encode_utf16_le(unicode_string));

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(
            reader.read_varchar_u8_length().await.unwrap(),
            unicode_string
        );
    }

    #[tokio::test]
    async fn test_read_varchar_u8_length_long_string() {
        // A length above 127 truncates if the shift is applied before widening
        // to usize: `200u8 << 1 == 144`, which would read 144 bytes instead of
        // 400 and desynchronize the stream.
        let unicode_string = "a".repeat(200);
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        binding.append_byte(unicode_string.encode_utf16().count() as u8);
        let builder = binding.append_bytes(&encode_utf16_le(&unicode_string));

        let mut reader = create_network_transport_with_data(&builder.build());

        assert_eq!(
            reader.read_varchar_u8_length().await.unwrap(),
            unicode_string
        );
    }

    #[tokio::test]
    async fn test_packet_header_split_across_reads() {
        let unicode_string = "Hello, world";
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        binding.append_byte(unicode_string.encode_utf16().count() as u8);
        let builder = binding.append_bytes(&encode_utf16_le(unicode_string));

        // 3-byte chunks split the 8-byte header across three reads.
        let mut reader = create_network_transport_with_chunked_data(&builder.build(), 3);

        assert_eq!(
            reader.read_varchar_u8_length().await.unwrap(),
            unicode_string
        );
    }

    #[tokio::test]
    async fn test_truncated_packet_reports_error() {
        let mut binding = TestPacketBuilder::new(PacketType::TabularResult);
        let mut packet = binding
            .append_bytes(&[0xAB; 100 - PacketWriter::PACKET_HEADER_SIZE])
            .build();
        assert_eq!(packet.len(), 100);

        // Claim 200 bytes but supply only the 100 actually built, so the reader
        // hits EOF mid-packet.
        BigEndian::write_u16(&mut packet[2..4], 200);

        let mut reader = create_network_transport_with_data(&packet);

        assert!(reader.read_byte().await.is_err());
    }

    /// Two packets, with a single `u32` split across the boundary. This is the
    /// case the deleted reader mishandled: it returned the raw byte count from
    /// the transport rather than the header length, so the second packet's
    /// header leaked into the payload.
    #[tokio::test]
    async fn test_read_value_spanning_packet_boundary() {
        let mut first = TestPacketBuilder::new(PacketType::TabularResult);
        let mut second = TestPacketBuilder::new(PacketType::TabularResult);
        let mut stream = first.continuation().append_bytes(&[0x11, 0x22]).build();
        stream.extend_from_slice(&second.append_bytes(&[0x33, 0x44]).build());

        let mut reader = create_network_transport_with_data(&stream);

        assert_eq!(reader.read_uint32().await.unwrap(), 0x4433_2211);
    }

    #[tokio::test]
    async fn test_read_value_spanning_packet_boundary_fragmented() {
        let mut first = TestPacketBuilder::new(PacketType::TabularResult);
        let mut second = TestPacketBuilder::new(PacketType::TabularResult);
        let mut stream = first.continuation().append_bytes(&[0x11, 0x22]).build();
        stream.extend_from_slice(&second.append_bytes(&[0x33, 0x44]).build());

        let mut reader = create_network_transport_with_chunked_data(&stream, 3);

        assert_eq!(reader.read_uint32().await.unwrap(), 0x4433_2211);
    }

    #[tokio::test]
    async fn test_sync_scalar_probe_fallback_across_packet_boundaries() {
        let expected_uint16 = 0x1234u16;
        let expected_int16 = -0x1234i16;
        let expected_uint24 = 0x00A1_B2C3u32;
        let expected_int32 = -0x0123_4567i32;
        let expected_uint32 = 0x89AB_CDEFu32;
        let expected_uint40 = 0xAB_CDEF_0123u64;
        let expected_int64 = -0x0102_0304_0506_0708i64;
        let expected_float32 = 1.5f32;
        let expected_float64 = -2.25f64;

        let uint16 = expected_uint16.to_le_bytes();
        let int16 = expected_int16.to_le_bytes();
        let uint24 = expected_uint24.to_le_bytes();
        let int32 = expected_int32.to_le_bytes();
        let uint32 = expected_uint32.to_le_bytes();
        let uint40 = expected_uint40.to_le_bytes();
        let int64 = expected_int64.to_le_bytes();
        let float32 = expected_float32.to_le_bytes();
        let float64 = expected_float64.to_le_bytes();

        let payloads = [
            vec![0xAB, uint16[0]],
            vec![uint16[1], int16[0]],
            vec![int16[1], uint24[0], uint24[1]],
            vec![uint24[2], int32[0], int32[1], int32[2]],
            vec![int32[3], uint32[0], uint32[1], uint32[2]],
            vec![uint32[3], uint40[0], uint40[1], uint40[2], uint40[3]],
            vec![
                uint40[4], int64[0], int64[1], int64[2], int64[3], int64[4], int64[5], int64[6],
            ],
            vec![int64[7], float32[0], float32[1], float32[2]],
            vec![
                float32[3], float64[0], float64[1], float64[2], float64[3], float64[4], float64[5],
                float64[6],
            ],
            vec![float64[7]],
        ];

        let mut stream = Vec::new();
        let last_index = payloads.len() - 1;
        for (index, payload) in payloads.iter().enumerate() {
            let mut packet = TestPacketBuilder::new(PacketType::TabularResult);
            // Every value below spans a packet boundary *inside one message*,
            // so only the final packet carries EOM.
            if index != last_index {
                packet.continuation();
            }
            stream.extend_from_slice(&packet.append_bytes(payload).build());
        }

        let mut reader = create_network_transport_with_data(&stream);

        assert_eq!(reader.try_read_byte(), None);
        assert_eq!(reader.read_byte().await.unwrap(), 0xAB);

        assert_eq!(reader.try_read_uint16(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 1);
        assert_eq!(reader.read_uint16().await.unwrap(), expected_uint16);

        assert_eq!(reader.try_read_int16(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 1);
        assert_eq!(reader.read_int16().await.unwrap(), expected_int16);

        assert_eq!(reader.try_read_uint24(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 2);
        assert_eq!(reader.read_uint24().await.unwrap(), expected_uint24);

        assert_eq!(reader.try_read_int32(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 3);
        assert_eq!(reader.read_int32().await.unwrap(), expected_int32);

        assert_eq!(reader.try_read_uint32(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 3);
        assert_eq!(reader.read_uint32().await.unwrap(), expected_uint32);

        assert_eq!(reader.try_read_uint40(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 4);
        assert_eq!(reader.read_uint40().await.unwrap(), expected_uint40);

        assert_eq!(reader.try_read_int64(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 7);
        assert_eq!(reader.read_int64().await.unwrap(), expected_int64);

        assert_eq!(reader.try_read_float32(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 3);
        assert_eq!(reader.read_float32().await.unwrap(), expected_float32);

        assert_eq!(reader.try_read_float64(), None);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 7);
        assert_eq!(reader.read_float64().await.unwrap(), expected_float64);
    }

    /// A payload-free non-EOM packet is malformed: it neither carries payload
    /// nor terminates a message.
    #[tokio::test]
    async fn test_payload_free_non_eom_packet_is_rejected() {
        let mut packet = TestPacketBuilder::new(PacketType::TabularResult).build();
        assert_eq!(packet.len(), PacketWriter::PACKET_HEADER_SIZE);
        packet[1] = 0x00; // clear EOM

        let mut reader = create_network_transport_with_data(&packet);

        assert!(matches!(
            reader.read_byte().await,
            Err(crate::error::Error::ProtocolError(_))
        ));
    }

    /// The same packet *with* EOM set is legal — an empty end-of-message packet
    /// terminates a message — so it is consumed and the reader carries on to
    /// the payload of the next packet.
    ///
    /// The bulk loops must reach the same verdict on the same bytes: a read
    /// that has not yet consumed anything is starting a new message, not
    /// over-reading the last one.
    #[tokio::test]
    async fn test_payload_free_eom_packet_is_accepted() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult).build();
        let mut next = TestPacketBuilder::new(PacketType::TabularResult);
        stream.extend_from_slice(&next.append_byte(0x7F).build());

        let mut reader = create_network_transport_with_data(&stream);
        assert_eq!(reader.read_byte().await.unwrap(), 0x7F);

        let mut reader = create_network_transport_with_data(&stream);
        let mut one = [0u8; 1];
        assert_eq!(reader.read_bytes(&mut one).await.unwrap(), 1);
        assert_eq!(one[0], 0x7F, "read_bytes must agree with read_byte");

        let mut reader = create_network_transport_with_data(&stream);
        reader
            .skip_bytes(1)
            .await
            .expect("skip_bytes must agree with read_byte");
    }

    // ---------------------------------------------------------------------
    // No-progress edge cases in the bulk read loops
    //
    // An empty end-of-message packet is legal framing but carries no payload.
    // A bulk read still short of its target treats it as a refill that yielded
    // nothing, and — before the no-progress check — went straight back to the
    // socket. A real server has finished its message at that point and sends
    // nothing more until the client issues a new request, so the read blocked
    // forever.
    //
    // These use `create_network_transport_with_live_peer` so the connection
    // stays open after the data is drained: with a dropped writer the reader
    // would hit EOF and error out no matter how broken the loop is, which is
    // exactly the masking these tests exist to avoid. Each is bounded by an
    // outer timeout, so a regression shows up as an elapsed timeout.
    // ---------------------------------------------------------------------

    /// One payload byte, then an empty EOM packet, against a request for two
    /// bytes. The second byte can never arrive.
    #[tokio::test]
    async fn read_bytes_past_end_of_message_errors_instead_of_hanging() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(0x11)
            .build();
        stream.extend_from_slice(&TestPacketBuilder::new(PacketType::TabularResult).build());

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [0u8; 2];
        let result = timeout(Duration::from_secs(5), reader.read_bytes(&mut destination))
            .await
            .expect("read_bytes hung: the refill loop consumed an empty EOM packet forever");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// `read_bytes_uninit` carries its own copy of the loop and needs the same
    /// check.
    #[tokio::test]
    async fn read_bytes_uninit_past_end_of_message_errors_instead_of_hanging() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(0x22)
            .build();
        stream.extend_from_slice(&TestPacketBuilder::new(PacketType::TabularResult).build());

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [std::mem::MaybeUninit::<u8>::uninit(); 2];
        let result = timeout(
            Duration::from_secs(5),
            reader.read_bytes_uninit(&mut destination),
        )
        .await
        .expect("read_bytes_uninit hung: the refill loop consumed an empty EOM packet forever");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// The scalar readers carry their own copies of the refill loop. A value
    /// that is *partially* delivered at the end of a message can never be
    /// completed, so it must error rather than park on a socket that will stay
    /// silent until the client sends a new request. `read_uint32` stands in for
    /// the whole family.
    ///
    /// This is the shape a drain hits after a mid-value error: the token-stream
    /// reader is parked partway through a value, and before this check the
    /// drain blocked here forever instead of surfacing the error.
    #[tokio::test]
    async fn read_uint32_spanning_end_of_message_errors_instead_of_hanging() {
        let stream = TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(0x01)
            .append_byte(0x02)
            .build();

        let mut reader = create_network_transport_with_live_peer(&stream);

        let result = timeout(Duration::from_secs(5), reader.read_uint32())
            .await
            .expect("read_uint32 hung on a value truncated by the end of the message");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// Skipping a value that runs past the end of the message must terminate
    /// too — this is the path taken when a column is discarded rather than
    /// decoded.
    #[tokio::test]
    async fn skip_bytes_past_end_of_message_errors_instead_of_hanging() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(0x33)
            .build();
        stream.extend_from_slice(&TestPacketBuilder::new(PacketType::TabularResult).build());

        let mut reader = create_network_transport_with_live_peer(&stream);

        let result = timeout(Duration::from_secs(5), reader.skip_bytes(2))
            .await
            .expect("skip_bytes hung: the refill loop consumed an empty EOM packet forever");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// Neither check must fire on legitimate traffic: a value split across
    /// packets still reads through, and an empty EOM packet that arrives when
    /// nothing is outstanding is simply consumed.
    ///
    /// The first packet is framed as a continuation — a multi-packet message
    /// sets EOM only on its final packet — so this exercises the real shape of
    /// a value that spans packets.
    #[tokio::test]
    async fn bulk_reads_still_span_packets_and_tolerate_a_trailing_empty_eom() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult)
            .continuation()
            .append_bytes(&[0xA1, 0xA2])
            .build();
        stream.extend_from_slice(
            &TestPacketBuilder::new(PacketType::TabularResult)
                .append_bytes(&[0xA3, 0xA4])
                .build(),
        );
        stream.extend_from_slice(&TestPacketBuilder::new(PacketType::TabularResult).build());

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [0u8; 4];
        let read = timeout(Duration::from_secs(5), reader.read_bytes(&mut destination))
            .await
            .expect("a value spanning two packets must not hang")
            .expect("a value spanning two packets must read successfully");

        assert_eq!(read, 4);
        assert_eq!(destination, [0xA1, 0xA2, 0xA3, 0xA4]);
    }

    // ---------------------------------------------------------------------
    // Consecutive payload-free messages.
    //
    // A bulk read that has not yet consumed a byte tolerates an empty EOM
    // packet, because a read starting exactly at a message boundary consumes
    // the packet terminating the *previous* message. That `continue` re-enters
    // the loop with nothing changed, so a peer that keeps sending empty
    // messages spins the loop with no state advancing and no bytes delivered —
    // bounded only by the command timeout.
    //
    // One in a row is legitimate; two is not. No server sends two empty
    // messages back to back, and the attention acknowledgement carries a DONE
    // token, so it is not payload-free either.
    // ---------------------------------------------------------------------

    /// Two empty messages in a row, then a peer that stays open and silent.
    /// Without the cap this spins until the caller's timeout.
    #[tokio::test]
    async fn read_bytes_errors_on_consecutive_empty_messages_instead_of_spinning() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult).build();
        stream.extend_from_slice(&TestPacketBuilder::new(PacketType::TabularResult).build());

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [0u8; 2];
        let result = timeout(Duration::from_secs(5), reader.read_bytes(&mut destination))
            .await
            .expect("read_bytes spun: consecutive empty messages advanced nothing");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// `read_bytes_uninit` carries its own copy of the loop and needs the same
    /// cap.
    #[tokio::test]
    async fn read_bytes_uninit_errors_on_consecutive_empty_messages_instead_of_spinning() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult).build();
        stream.extend_from_slice(&TestPacketBuilder::new(PacketType::TabularResult).build());

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [std::mem::MaybeUninit::<u8>::uninit(); 2];
        let result = timeout(
            Duration::from_secs(5),
            reader.read_bytes_uninit(&mut destination),
        )
        .await
        .expect("read_bytes_uninit spun: consecutive empty messages advanced nothing");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// `skip_bytes` spins the same way, and it runs on the discard path that
    /// large-value reads use, so a stall there is just as unbounded.
    #[tokio::test]
    async fn skip_bytes_errors_on_consecutive_empty_messages_instead_of_spinning() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult).build();
        stream.extend_from_slice(&TestPacketBuilder::new(PacketType::TabularResult).build());

        let mut reader = create_network_transport_with_live_peer(&stream);

        let result = timeout(Duration::from_secs(5), reader.skip_bytes(2))
            .await
            .expect("skip_bytes spun: consecutive empty messages advanced nothing");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// Control: the cap must stay quiet on the legitimate case it tolerates —
    /// a read that begins exactly at a message boundary, so the first packet it
    /// sees is the empty one terminating the previous message.
    #[tokio::test]
    async fn bulk_reads_tolerate_a_single_leading_empty_message() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult).build();
        stream.extend_from_slice(
            &TestPacketBuilder::new(PacketType::TabularResult)
                .append_bytes(&[0xB1, 0xB2])
                .build(),
        );

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [0u8; 2];
        let read = timeout(Duration::from_secs(5), reader.read_bytes(&mut destination))
            .await
            .expect("a read starting at a message boundary must not hang")
            .expect("a leading empty message must be consumed, not rejected");

        assert_eq!(read, 2);
        assert_eq!(destination, [0xB1, 0xB2]);
    }

    // ---------------------------------------------------------------------
    // End-of-message tracking.
    //
    // The tests above all append an explicit empty EOM packet after the short
    // value, which is what lets a purely reactive `to_read == 0` check notice
    // the problem. Real servers never send that packet: they finish the message
    // on the packet that carries the last payload byte and then go quiet.
    //
    // Without recording the EOM flag, a reader still short of its target walks
    // back into the socket and blocks there forever, so the reactive check is
    // never reached. These tests pin the realistic shape — a complete message
    // with payload and no trailing packet.
    // ---------------------------------------------------------------------

    /// A single EOM packet carrying one byte, against a request for two. No
    /// trailing packet follows, exactly as a real server would leave it.
    #[tokio::test]
    async fn read_bytes_past_end_of_message_without_a_trailing_packet_errors_instead_of_hanging() {
        let stream = TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(0x11)
            .build();

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [0u8; 2];
        let result = timeout(Duration::from_secs(5), reader.read_bytes(&mut destination))
            .await
            .expect("read_bytes hung: the reader re-entered the socket after end-of-message");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// Same shape through the uninitialised-buffer loop.
    #[tokio::test]
    async fn read_bytes_uninit_past_end_of_message_without_a_trailing_packet_errors() {
        let stream = TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(0x22)
            .build();

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [std::mem::MaybeUninit::<u8>::uninit(); 2];
        let result = timeout(
            Duration::from_secs(5),
            reader.read_bytes_uninit(&mut destination),
        )
        .await
        .expect("read_bytes_uninit hung: the reader re-entered the socket after end-of-message");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// Same shape through the skip loop, taken when a column is discarded.
    #[tokio::test]
    async fn skip_bytes_past_end_of_message_without_a_trailing_packet_errors() {
        let stream = TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(0x33)
            .build();

        let mut reader = create_network_transport_with_live_peer(&stream);

        let result = timeout(Duration::from_secs(5), reader.skip_bytes(2))
            .await
            .expect("skip_bytes hung: the reader re-entered the socket after end-of-message");

        assert!(
            matches!(result, Err(crate::error::Error::ProtocolError(_))),
            "expected a protocol error, got {result:?}"
        );
    }

    /// A value that ends exactly on the message boundary must still succeed —
    /// the check fires on demand exceeding what remains, not on EOM alone.
    #[tokio::test]
    async fn a_value_ending_exactly_at_end_of_message_reads_successfully() {
        let mut stream = TestPacketBuilder::new(PacketType::TabularResult)
            .continuation()
            .append_bytes(&[0xB1, 0xB2])
            .build();
        stream.extend_from_slice(
            &TestPacketBuilder::new(PacketType::TabularResult)
                .append_bytes(&[0xB3])
                .build(),
        );

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut destination = [0u8; 3];
        let read = timeout(Duration::from_secs(5), reader.read_bytes(&mut destination))
            .await
            .expect("a value ending at the message boundary must not hang")
            .expect("a value ending at the message boundary must read successfully");

        assert_eq!(read, 3);
        assert_eq!(destination, [0xB1, 0xB2, 0xB3]);
    }

    /// `reset_reader` used to `assert!` that the buffer was fully consumed.
    /// `buffer_length` is taken from the packet header the peer sent, so that
    /// assertion put a peer-controlled value behind a process abort that stays
    /// active in release builds. Resetting with bytes still unread now just
    /// discards them, which is what a reset means.
    #[tokio::test]
    async fn reset_reader_discards_unread_bytes_instead_of_aborting() {
        let stream = TestPacketBuilder::new(PacketType::TabularResult)
            .append_bytes(&[0xC1, 0xC2, 0xC3, 0xC4])
            .build();

        let mut reader = create_network_transport_with_live_peer(&stream);

        let mut first = [0u8; 1];
        reader
            .read_bytes(&mut first)
            .await
            .expect("the first byte must read");
        assert_eq!(first, [0xC1]);
        assert_eq!(reader.tds_read_buffer.get_remaining_byte_count(), 3);

        TdsPacketReader::reset_reader(&mut reader);

        assert_eq!(
            reader.tds_read_buffer.get_remaining_byte_count(),
            0,
            "reset_reader must leave an empty buffer"
        );
    }

    // ---------------------------------------------------------------------
    // Attention acknowledgement (DONE_ATTN) after cancellation or timeout.
    //
    // Cancelling or timing out a read sends TDS ATTENTION and then drains the
    // response until the server answers with a DONE carrying ATTN. Neither half
    // used to be bounded, so a server that never answers — or one that has
    // stopped reading, stalling the send — parked the caller inside its own
    // cancellation forever, past any deadline the caller had set.
    //
    // Most of these use `create_network_transport_with_live_peer_capturing_writes`
    // so the socket stays open and silent after the scripted bytes are drained.
    // A helper that drops its writer would hand the drain an EOF and end it no
    // matter how unbounded it is, which is exactly the masking these tests
    // exist to avoid. The peer also reads, so the ATTENTION write only blocks
    // in the one test that wants it to.
    //
    // Time is paused, so the bound elapses the moment the runtime goes idle: a
    // regression that arms no timer leaves only the outer guard, which then
    // fires and fails the test instead of running for real minutes. Every wait
    // here is under such a guard, including the channel receives — an
    // unguarded one would hang on the very regressions these tests target.
    // ---------------------------------------------------------------------

    /// A DONE token with `status`, framed as a message of its own — the shape
    /// of an attention acknowledgement on the wire.
    fn done_token_message(status: u16) -> Vec<u8> {
        TestPacketBuilder::new(PacketType::TabularResult)
            .append_byte(crate::token::tokens::TokenType::Done as u8)
            .append_u16(status)
            .append_u16(0) // CurCmd, unused by this path
            .append_u64(0) // RowCount
            .build()
    }

    /// A handle that is already cancelled, so the read it guards is abandoned
    /// before a byte is consumed.
    fn cancelled_handle() -> CancelHandle {
        let parent = CancelHandle::new();
        let child = parent.child_handle();
        parent.cancel();
        child
    }

    /// Reads the flag connection pools consult before handing a connection out
    /// again.
    fn is_known_dead(transport: &NetworkTransport) -> bool {
        use crate::connection::transport::tds_transport::TdsTransport;
        TdsTransport::connection_known_dead(transport)
    }

    /// Runs one cancelled read under the outer guard and reports how long the
    /// cancellation cleanup took.
    async fn time_one_cancelled_read(transport: &mut NetworkTransport) -> Duration {
        let started = Instant::now();
        let result = timeout(
            Duration::from_secs(600),
            transport.receive_token(&ParserContext::None(()), None, Some(&cancelled_handle())),
        )
        .await
        .expect("cancellation hung waiting for a DONE_ATTN the server never sent");

        assert!(
            matches!(result, Err(OperationCancelledError(_))),
            "the caller must still see its cancellation, got {result:?}"
        );
        started.elapsed()
    }

    /// The bug: cancellation waits for a `DONE_ATTN` that never comes.
    ///
    /// The peer acknowledges nothing, so before the bound the drain parked on
    /// the socket and the caller never got its cancellation back.
    #[tokio::test(start_paused = true)]
    async fn cancellation_does_not_wait_forever_for_an_attention_acknowledgement() {
        let (mut transport, mut written) =
            create_network_transport_with_live_peer_capturing_writes(&[]);

        let result = timeout(
            Duration::from_secs(600),
            transport.receive_token(&ParserContext::None(()), None, Some(&cancelled_handle())),
        )
        .await
        .expect("cancellation hung waiting for a DONE_ATTN the server never sent");

        assert!(
            matches!(result, Err(OperationCancelledError(_))),
            "the caller must still see its cancellation, got {result:?}"
        );

        let sent = timeout(Duration::from_secs(600), written.recv())
            .await
            .expect("cancellation returned without ever putting ATTENTION on the wire")
            .expect("the peer must observe a write");
        assert_eq!(
            sent[0],
            PacketType::Attention as u8,
            "cancellation must put an ATTENTION packet on the wire"
        );
    }

    /// The bound has to cover the ATTENTION write, not just the drain that
    /// follows it.
    ///
    /// The duplex is one byte wide and the peer end is held without ever being
    /// read, so the client's ATTENTION packet fills that byte and the write
    /// parks — a peer whose receive window has closed, in miniature. Timing
    /// only the drain leaves the caller stuck here, before any deadline is
    /// armed.
    ///
    /// The sizing lives in the test rather than in a shared helper: borrowed,
    /// it could be widened for some other test's benefit, the write would then
    /// succeed, the drain would time out instead, and both assertions below
    /// would still hold — leaving a passing test that no longer covers the
    /// send. The last assertion pins the mechanism for the same reason.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_attention_write_does_not_park_cancellation() {
        /// A TDS header with no payload, which is all an ATTENTION packet is.
        const ATTENTION_PACKET_LEN: usize = 8;

        let (client_side, mut peer) = duplex(1);
        let mut transport = build_duplex_transport(client_side);

        let result = timeout(
            Duration::from_secs(600),
            transport.receive_token(&ParserContext::None(()), None, Some(&cancelled_handle())),
        )
        .await
        .expect("cancellation hung writing ATTENTION to a peer that had stopped reading");

        assert!(
            matches!(result, Err(OperationCancelledError(_))),
            "the caller must still see its cancellation, got {result:?}"
        );
        assert!(
            is_known_dead(&transport),
            "a half-written attention leaves the stream unresynchronisable"
        );

        let mut buffer = [0u8; ATTENTION_PACKET_LEN];
        let delivered = timeout(Duration::from_secs(600), peer.read(&mut buffer))
            .await
            .expect("the write left nothing with the peer, so nothing stalled")
            .expect("reading the peer end must not fail");
        assert!(
            delivered < ATTENTION_PACKET_LEN,
            "the write must have stalled part-way; a completed one would have \
             delivered all {ATTENTION_PACKET_LEN} bytes, so this test would be \
             measuring the drain instead"
        );
    }

    /// The bound belongs to the connection, not to each call.
    ///
    /// Nothing consulted the known-dead flag on the way in, so a caller that
    /// re-entered `receive_token` after a cancellation — closing a cursor,
    /// draining what is left of a batch — paid the full bound again every
    /// time, and the advertised ceiling multiplied by however many reads
    /// followed.
    #[tokio::test(start_paused = true)]
    async fn a_second_cancellation_does_not_spend_the_bound_again() {
        let bound = Duration::from_secs(ATTENTION_TIMEOUT_SECONDS);
        let (mut transport, _written) =
            create_network_transport_with_live_peer_capturing_writes(&[]);

        let first = time_one_cancelled_read(&mut transport).await;
        assert!(
            first >= bound,
            "the first cancellation waits out the whole bound for an \
             acknowledgement that never comes, took {first:?}"
        );
        assert!(
            is_known_dead(&transport),
            "an unacknowledged attention must leave the connection dead"
        );

        let second = time_one_cancelled_read(&mut transport).await;
        assert_eq!(
            second,
            Duration::ZERO,
            "a connection already given up on has nothing left to acknowledge, \
             so a later cancellation must return without waiting at all"
        );
    }

    /// The same unbounded wait is reachable from a request timeout, which is
    /// the shape a caller hits without ever holding a cancellation handle.
    #[tokio::test(start_paused = true)]
    async fn request_timeout_does_not_wait_forever_for_an_attention_acknowledgement() {
        let (mut transport, _written) =
            create_network_transport_with_live_peer_capturing_writes(&[]);

        let result = timeout(
            Duration::from_secs(600),
            transport.receive_token(
                &ParserContext::None(()),
                Some(Duration::from_millis(50)),
                None,
            ),
        )
        .await
        .expect("the timeout path hung waiting for a DONE_ATTN the server never sent");

        assert!(
            matches!(result, Err(TimeoutError(_))),
            "the caller must still see its timeout, got {result:?}"
        );
    }

    /// An unacknowledged ATTENTION leaves the TDS stream at an unknown point,
    /// so the connection must become observably unusable rather than be handed
    /// back to a pool.
    #[tokio::test(start_paused = true)]
    async fn an_unacknowledged_attention_marks_the_connection_dead() {
        let (mut transport, _written) =
            create_network_transport_with_live_peer_capturing_writes(&[]);

        let _ = timeout(
            Duration::from_secs(600),
            transport.receive_token(&ParserContext::None(()), None, Some(&cancelled_handle())),
        )
        .await
        .expect("cancellation hung waiting for a DONE_ATTN the server never sent");

        assert!(
            is_known_dead(&transport),
            "a connection whose attention went unacknowledged must not be reused"
        );
    }

    /// The check must stay quiet on a healthy cancellation: a server that
    /// acknowledges keeps its connection usable. Without this, bounding the
    /// wait could just condemn every cancelled connection and still pass the
    /// tests above.
    #[tokio::test(start_paused = true)]
    async fn an_acknowledged_attention_leaves_the_connection_usable() {
        let (mut transport, _written) = create_network_transport_with_live_peer_capturing_writes(
            &done_token_message(DoneStatus::ATTN.bits()),
        );

        let result = timeout(
            Duration::from_secs(600),
            transport.receive_token(&ParserContext::None(()), None, Some(&cancelled_handle())),
        )
        .await
        .expect("an acknowledged cancellation must not hang");

        assert!(
            matches!(result, Err(OperationCancelledError(_))),
            "the caller must still see its cancellation, got {result:?}"
        );
        assert!(
            !is_known_dead(&transport),
            "an acknowledged attention leaves the connection reusable"
        );
    }

    /// The drain discards whatever the server was still sending and stops at
    /// the acknowledgement — tokens queued behind the ATTENTION must not be
    /// mistaken for it, and must not push the wait past its bound either.
    #[tokio::test(start_paused = true)]
    async fn the_drain_discards_trailing_tokens_before_the_acknowledgement() {
        // A plain DONE for the cancelled statement, then the acknowledgement.
        let mut stream = done_token_message(DoneStatus::FINAL.bits());
        stream.extend_from_slice(&done_token_message(DoneStatus::ATTN.bits()));

        let (mut transport, _written) =
            create_network_transport_with_live_peer_capturing_writes(&stream);

        let result = timeout(
            Duration::from_secs(600),
            transport.receive_token(&ParserContext::None(()), None, Some(&cancelled_handle())),
        )
        .await
        .expect("the drain hung instead of skipping past the trailing DONE");

        assert!(
            matches!(result, Err(OperationCancelledError(_))),
            "the caller must still see its cancellation, got {result:?}"
        );
        assert!(
            !is_known_dead(&transport),
            "the acknowledgement arrived, so the connection stays usable"
        );
    }
}
