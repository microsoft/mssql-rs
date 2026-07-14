// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TdsTransport trait provides an abstraction over the transport layer for TDS communication.
//! This allows for different implementations (real network, mock for testing/fuzzing, etc.)

use crate::core::TdsResult;
use crate::io::packet_reader::TdsPacketReader;
use crate::io::reader_writer::NetworkWriter;
use crate::io::token_stream::TdsTokenStreamReader;
use async_trait::async_trait;
use std::time::Duration;

/// TdsTransport abstracts the transport layer for TDS communication.
/// It combines token stream reading capabilities with writer access and reader management.
///
/// This trait is implemented by:
/// - `NetworkTransport` for real network communication
/// - `MockTransport` (in fuzzing mode) for testing without network I/O
#[async_trait]
pub(crate) trait TdsTransport: TdsTokenStreamReader + Send + Sync + std::fmt::Debug {
    /// Get a mutable reference to the network writer.
    /// Used to create packet writers for sending messages to the server.
    fn as_writer(&mut self) -> &mut dyn NetworkWriter;

    /// Reset the internal reader state.
    /// This should clear any buffered data and reset the reader position.
    fn reset_reader(&mut self);

    /// Get the configured packet size for this transport.
    #[allow(dead_code)]
    fn packet_size(&self) -> u32;

    /// Close the transport connection.
    /// This should cleanly shut down any underlying network connections.
    async fn close_transport(&mut self) -> TdsResult<()>;

    /// Send an attention packet and wait for acknowledgment with a timeout.
    ///
    /// This method implements the attention sending flow:
    /// 1. Send MT_ATTN (0x06) packet to the server
    /// 2. Wait for DONE token with ATTN (0x0020) status flag
    /// 3. If no acknowledgment within timeout, return error
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum time to wait for acknowledgment
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - Attention acknowledged by server
    /// * `Ok(false)` - Attention sent but timeout expired waiting for ACK
    /// * `Err(_)` - Error sending attention or reading response
    async fn send_attention_with_timeout(&mut self, timeout: Duration) -> TdsResult<bool>;

    /// Probe whether the underlying connection is dead via a non-blocking socket
    /// poll. Returns `true` if dead, `false` if alive or unknown.
    ///
    /// **Internal use only.** This reads from the socket and will consume a byte
    /// if unsolicited data is present, so it is only safe to call on an idle
    /// connection with no outstanding request or unread results. It is used by
    /// the idle-connection-resiliency path, which calls it at a known-idle
    /// point. Pool/liveness consumers must use [`connection_known_dead`] instead.
    ///
    /// [`connection_known_dead`]: TdsTransport::connection_known_dead
    fn is_connection_dead(&self) -> bool;

    /// Expose the transport as a `TdsPacketReader` for low-level streaming operations
    /// such as incremental PLP column reads via [`PlpColumnStream`].
    fn as_packet_reader(&mut self) -> &mut (dyn TdsPacketReader + Send + Sync);

    /// Returns the connection's last-known liveness status **without touching the
    /// socket**.
    ///
    /// Returns `true` once the connection has been explicitly closed or an I/O
    /// operation has observed it broken. Unlike [`is_connection_dead`], this is a
    /// cached read: it never reads from the socket, so it cannot consume
    /// in-flight data and is always safe to call regardless of connection state.
    ///
    /// A `false` result means the connection has not been observed dead; it may
    /// still have failed silently while idle, which idle connection resiliency
    /// detects and recovers on the next operation.
    ///
    /// [`is_connection_dead`]: TdsTransport::is_connection_dead
    fn connection_known_dead(&self) -> bool {
        false
    }
}
