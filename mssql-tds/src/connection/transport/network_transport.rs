// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::client_context::{IPAddressPreference, TransportContext};
use crate::connection::transport::buffers::TdsReadBuffer;
use crate::connection::transport::extractable_stream;
use crate::connection::transport::parallel_connect::{ParallelConnectConfig, parallel_connect};
use crate::connection::transport::ssl_handler::SslHandler;
use crate::connection_provider::tds_connection_provider::PARSER_REGISTRY;
use crate::core::{
    CancelHandle, EncryptionOptions, EncryptionSetting, NegotiatedEncryptionSetting, TdsResult,
};
use crate::datatypes::row_writer::RowWriter;
use crate::error::Error::{OperationCancelledError, TimeoutError};
use crate::error::TimeoutErrorType;
use crate::handler::handler_factory::SessionSettings;
use crate::io::packet_reader::{PacketReader, TdsPacketReader};
use crate::io::packet_writer::PacketWriter;
use crate::io::reader_writer::{NetworkReader, NetworkReaderWriter, NetworkWriter};
use crate::io::token_stream::{
    ParserContext, RowReadResult, TdsTokenStreamReader, receive_row_into_internal,
    receive_token_internal,
};
use crate::message::attention::AttentionRequest;
use crate::message::login_options::TdsVersion;
use crate::message::messages::Request;
use crate::token::tokens::{DoneStatus, Tokens};
use async_trait::async_trait;
use byteorder::{BigEndian, ByteOrder, LittleEndian};
use std::cmp::min;
use std::io::Error;
use std::io::ErrorKind::{self, UnexpectedEof};
use std::net::ToSocketAddrs;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{self, TcpStream};
use tokio::time::timeout;
use tracing::{debug, error, event, info, trace};

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
    if tcp_stream.is_none() {
        return Err(crate::error::Error::from(last_error.unwrap()));
    }

    Ok(Box::new(tcp_stream.unwrap()))
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
) -> TdsResult<Box<NetworkTransport>> {
    let ssl_handler = SslHandler {
        server_host_name: transport_context.get_server_name().to_string(),
        encryption_options,
    };

    match tds_version {
        TdsVersion::V7_4 => {
            // TDS 7.4 starts with unencrypted streams that could get encrypted as part of prelogin
            // negotiation. TLS must be wrapped in TDS packets for this version.
            info!("Creating NetworkTransport for TDS 7.4 with TLS wrapping");

            Ok(Box::new(NetworkTransport::new(
                stream,
                ssl_handler,
                PRE_NEGOTIATED_PACKET_SIZE,
                encryption_mode,
                true, // Use TDS 7.4 TLS wrapping
            )))
        }
        TdsVersion::V8_0 => {
            // Enable TLS immediately for TDS 8.0 (before any TDS packets are exchanged)
            info!("Creating NetworkTransport for TDS 8.0 with immediate TLS");

            let encrypted_stream = ssl_handler
                .enable_ssl_async(stream, NegotiatedEncryptionSetting::Strict)
                .await?;

            Ok(Box::new(NetworkTransport::new(
                encrypted_stream,
                ssl_handler,
                PRE_NEGOTIATED_PACKET_SIZE,
                encryption_mode,
                false, // TDS 8.0 uses standard TLS (no TDS wrapping)
            )))
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
) -> TdsResult<Box<NetworkTransport>> {
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
}

impl Stream for TcpStream {
    fn tls_handshake_starting(&mut self) {
        // No-op for plain TCP streams
    }

    fn tls_handshake_completed(&mut self) {
        // No-op for plain TCP streams
    }
}

impl Stream for Box<dyn Stream> {
    fn tls_handshake_starting(&mut self) {
        (**self).tls_handshake_starting();
    }

    fn tls_handshake_completed(&mut self) {
        (**self).tls_handshake_completed();
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
    async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        Ok(self.receive(buffer).await?)
    }

    fn packet_size(&self) -> u32 {
        self.packet_size
    }
}

#[async_trait]
impl NetworkWriter for NetworkTransport {
    async fn send(&mut self, data: &[u8]) -> TdsResult<()> {
        let stream = self.stream.as_mut().expect("Stream not available");
        stream.write_all(data).await?;
        Ok(())
    }

    fn packet_size(&self) -> u32 {
        self.packet_size
    }

    fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
        assert!(self.encryption.is_some());
        self.encryption.unwrap()
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
        }
    }

    pub(crate) fn notify_encryption_negotiation(
        &mut self,
        encryption: NegotiatedEncryptionSetting,
    ) {
        assert!(self.encryption.is_none());
        self.encryption = Some(encryption);
    }

    pub(crate) async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        if buffer.is_empty() {
            return Err(crate::error::Error::UsageError(
                "Buffer length must be greater than 0".to_string(),
            ));
        }
        let bytes_read = self
            .stream
            .as_mut()
            .expect("Stream not available")
            .read(buffer)
            .await?;
        if bytes_read == 0 {
            Err(crate::error::Error::from(std::io::Error::from(
                UnexpectedEof,
            )))
        } else {
            Ok(bytes_read)
        }
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

        let base_stream = handle.extract().ok_or_else(|| {
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
        Ok(())
    }

    async fn read_tds_packet(&mut self) -> TdsResult<()> {
        // let remaining_bytes = self.buffer_length - self.buffer_position;
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

        let stream = self.stream.as_mut().expect("Stream not available");

        // Read more data if we don't have enough for the header
        while bytes_available < PacketWriter::PACKET_HEADER_SIZE {
            let bytes_read = stream
                .read(&mut self.tds_read_buffer.working_buffer[base_offset + bytes_available..])
                .await?;
            if bytes_read == 0 {
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
            let bytes_read = stream
                .read(&mut self.tds_read_buffer.working_buffer[base_offset + bytes_available..])
                .await?;
            if bytes_read == 0 {
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

    /// Tells the server to stop sending tokens for the token stream being read and waits for
    /// an acknowledgement.
    async fn cancel_read_stream_and_wait(&mut self) -> TdsResult<()> {
        self.cancel_read_stream().await?;
        // Wait indefinitely for attention ACK
        let _ = self.wait_for_attention_ack(None).await?;
        Ok(())
    }

    /// Wait for attention acknowledgment from server with optional timeout.
    ///
    /// This helper reads tokens until it receives a DONE token with the ATTN flag set,
    /// discarding any other tokens. If a timeout is specified and expires before
    /// receiving the ACK, returns `Ok(false)`.
    ///
    /// # Arguments
    ///
    /// * `attention_timeout` - Optional timeout. If `None`, waits indefinitely.
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - Attention acknowledged by server
    /// * `Ok(false)` - Timeout expired waiting for ACK (only when timeout specified)
    /// * `Err(_)` - Error reading response
    async fn wait_for_attention_ack(
        &mut self,
        attention_timeout: Option<Duration>,
    ) -> TdsResult<bool> {
        let dummy_context = ParserContext::None(());
        let start = std::time::Instant::now();

        loop {
            // Check timeout if specified
            if let Some(timeout_duration) = attention_timeout
                && start.elapsed() >= timeout_duration
            {
                debug!("Attention ACK timeout after {:?}", start.elapsed());
                return Ok(false);
            }

            // Read next token, with timeout if specified
            let token_result = if let Some(timeout_duration) = attention_timeout {
                let remaining = timeout_duration.saturating_sub(start.elapsed());
                match timeout(
                    remaining,
                    receive_token_internal(self, &*PARSER_REGISTRY, &dummy_context),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_elapsed) => {
                        debug!(
                            "Attention ACK timeout (elapsed) after {:?}",
                            start.elapsed()
                        );
                        return Ok(false);
                    }
                }
            } else {
                // No timeout - wait indefinitely
                receive_token_internal(self, &*PARSER_REGISTRY, &dummy_context).await
            };

            match token_result {
                Ok(token) => {
                    if let Tokens::Done(done_token) = token
                        && done_token.status.contains(DoneStatus::ATTN)
                    {
                        debug!("Attention ACK received after {:?}", start.elapsed());
                        return Ok(true);
                    }
                    // Discard other tokens and continue waiting
                }
                Err(e) => {
                    if attention_timeout.is_none() {
                        // When waiting indefinitely, errors just break the loop (original behavior)
                        break;
                    }
                    // When using timeout, propagate errors
                    debug!(
                        "Error reading token while waiting for attention ACK: {:?}",
                        e
                    );
                    return Err(e);
                }
            }
        }
        Ok(true)
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

#[async_trait]
impl TdsPacketReader for NetworkTransport {
    fn reset_reader(&mut self) {
        // Make sure that we have read all the data from the buffer.
        assert!(self.tds_read_buffer.buffer_length == self.tds_read_buffer.buffer_position);
        self.tds_read_buffer
            .change_packet_size(NetworkReader::packet_size(self));
        self.tds_read_buffer.reset_to_length(0);
    }

    async fn read_byte(&mut self) -> TdsResult<u8> {
        if !self.tds_read_buffer.do_we_have_enough_data(1) {
            self.read_tds_packet().await?;
        }
        let result: u8 = self.tds_read_buffer.working_buffer[self.tds_read_buffer.buffer_position];
        self.tds_read_buffer.consume_bytes(1);
        Ok(result)
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        if !self.tds_read_buffer.do_we_have_enough_data(2) {
            self.read_tds_packet().await?;
        }
        let result = BigEndian::read_i16(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(2);
        Ok(result)
    }
    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        if !self.tds_read_buffer.do_we_have_enough_data(4) {
            self.read_tds_packet().await?;
        }
        let result = BigEndian::read_i32(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(4);
        Ok(result)
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        if !self.tds_read_buffer.do_we_have_enough_data(5) {
            self.read_tds_packet().await?;
        }

        let result = LittleEndian::read_uint(self.tds_read_buffer.get_slice(), 5);
        self.tds_read_buffer.consume_bytes(5);
        Ok(result)
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        if !self.tds_read_buffer.do_we_have_enough_data(4) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_f32(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(4);
        Ok(result)
    }
    async fn read_float64(&mut self) -> TdsResult<f64> {
        if !self.tds_read_buffer.do_we_have_enough_data(8) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_f64(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(8);
        Ok(result)
    }
    async fn read_int16(&mut self) -> TdsResult<i16> {
        if !self.tds_read_buffer.do_we_have_enough_data(2) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_i16(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(2);
        Ok(result)
    }
    async fn read_uint16(&mut self) -> TdsResult<u16> {
        if !self.tds_read_buffer.do_we_have_enough_data(2) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_u16(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(2);
        Ok(result)
    }
    async fn read_uint24(&mut self) -> TdsResult<u32> {
        if !self.tds_read_buffer.do_we_have_enough_data(3) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_u24(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(3);
        Ok(result)
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        if !self.tds_read_buffer.do_we_have_enough_data(4) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_i32(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(4);
        Ok(result)
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        if !self.tds_read_buffer.do_we_have_enough_data(4) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_u32(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(4);
        Ok(result)
    }
    async fn read_int64(&mut self) -> TdsResult<i64> {
        if !self.tds_read_buffer.do_we_have_enough_data(8) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_i64(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(8);
        Ok(result)
    }
    async fn read_uint64(&mut self) -> TdsResult<u64> {
        if !self.tds_read_buffer.do_we_have_enough_data(8) {
            self.read_tds_packet().await?;
        }
        let result = LittleEndian::read_u64(self.tds_read_buffer.get_slice());
        self.tds_read_buffer.consume_bytes(8);
        Ok(result)
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        let mut total_read = 0;
        let mut length_to_read = buffer.len();
        let mut offset = 0;
        while length_to_read > 0 {
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

            if to_read > 0 {
                // Copy from self.working_buffer to buffer from self.buffer_position to offset.
                buffer[offset..offset + to_read].copy_from_slice(
                    &self.tds_read_buffer.working_buffer[self.tds_read_buffer.buffer_position
                        ..self.tds_read_buffer.buffer_position + to_read],
                );
                offset += to_read;
                length_to_read -= to_read;
                total_read += to_read;

                self.tds_read_buffer.consume_bytes(to_read);
            }
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
        if length == PacketReader::LENGTHNULL {
            return Ok(None);
        }

        let string = self
            .read_unicode_with_byte_length((length << 1) as usize)
            .await?;
        Ok(Some(string))
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        let length: u8 = self.read_byte().await?;
        let string = self
            .read_unicode_with_byte_length((length << 1) as usize)
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
        while length_to_read > 0 {
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

            if to_read > 0 {
                length_to_read -= to_read;
                self.tds_read_buffer.consume_bytes(to_read);
            }
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

#[async_trait]
impl TdsTokenStreamReader for NetworkTransport {
    async fn receive_token(
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
                    self.cancel_read_stream_and_wait().await?;
                }
                _ => {}
            },
        }
        token_result
    }

    async fn receive_row_into(
        &mut self,
        context: &ParserContext,
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
        writer: &mut (dyn RowWriter + Send),
    ) -> TdsResult<RowReadResult> {
        let cancellable = CancelHandle::run_until_cancelled(
            cancel_handle,
            receive_row_into_internal(self, &*PARSER_REGISTRY, context, writer),
        );
        let result = match remaining_request_timeout.as_ref() {
            Some(t) => match timeout(*t, cancellable).await {
                Ok(r) => r,
                Err(elapsed) => Err(TimeoutError(TimeoutErrorType::Elapsed(elapsed))),
            },
            None => cancellable.await,
        };

        match &result {
            Ok(_) => {}
            Err(err) => match err {
                OperationCancelledError(_) | TimeoutError(_) => {
                    self.cancel_read_stream_and_wait().await?;
                }
                _ => {}
            },
        }
        result
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
        Ok(())
    }

    /// Send an attention packet and wait for acknowledgment with a timeout.
    ///
    /// This implements the attention sending flow with a configurable timeout:
    /// 1. Send MT_ATTN (0x06) packet to the server
    /// 2. Wait for DONE token with ATTN (0x0020) status flag
    /// 3. If no acknowledgment within timeout, return false
    ///
    /// This is used by bulk copy timeout handling to implement the 5-second
    /// attention ACK timeout per SqlClient behavior.
    async fn send_attention_with_timeout(
        &mut self,
        attention_timeout: Duration,
    ) -> TdsResult<bool> {
        // Send attention packet
        self.cancel_read_stream().await?;

        // Wait for ACK with timeout using the shared helper
        self.wait_for_attention_ack(Some(attention_timeout)).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*; // Brings in NetworkTransport, SslHandler, StreamRecoverer, etc.
    use crate::connection::client_context::ClientContext;
    use crate::connection::transport::network_transport::Stream;
    use crate::connection::transport::ssl_handler::SslHandler;
    use crate::core::EncryptionOptions;
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

    pub(crate) fn create_client_server_network_transport(
        context: &ClientContext,
    ) -> (NetworkTransport, NetworkTransport) {
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
            NetworkTransport::new(
                Box::new(server_side),
                SslHandler {
                    server_host_name: context.transport_context.get_server_name().clone(),
                    encryption_options: context.encryption_options.clone(),
                },
                context.packet_size as u32,
                context.encryption_options.mode,
                false,
            ),
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

    /// A basic test showing that `receive` can read data from the transport's reader.
    #[tokio::test]
    async fn test_network_transport_receive() -> TdsResult<()> {
        // 1) Create an in-memory duplex stream (client_side, server_side).
        // Data will be written on the server_side and the network transport will read from the client side.
        let (client_side, server_side) = duplex(1024);

        // Mocks and defaults.
        let context = ClientContext {
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
            ..Default::default()
        };
        let ssl_handler = SslHandler {
            server_host_name: context.transport_context.get_server_name().clone(),
            encryption_options: EncryptionOptions {
                mode: EncryptionSetting::On,
                trust_server_certificate: true,
                ..EncryptionOptions::default()
            },
        };

        // Optionally, shut down the writer so the reader sees EOF if all data is read
        // client_writer.shutdown().await?;

        // Build our transport
        let mut transport = NetworkTransport::new(
            Box::new(client_side),
            ssl_handler,
            context.packet_size as u32,
            context.encryption_options.mode,
            false,
        );

        let mut rng = rand::rng();
        let data_size = 128;
        let data_written: Vec<u8> = (0..data_size).map(|_| rng.random()).collect();
        let mut framed_writer = FramedWrite::new(server_side, BytesCodec::new());
        framed_writer
            .send(Bytes::copy_from_slice(&data_written[..]))
            .await?;

        // 5) Attempt to read from the transport into a buffer
        let mut buffer = vec![0u8; data_size];
        let bytes_read = transport.receive(&mut buffer).await?;

        // Verify we read exactly `data_size` bytes and that they match what was written
        assert_eq!(bytes_read, data_size);
        assert_eq!(buffer, data_written);

        Ok(())
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
}
