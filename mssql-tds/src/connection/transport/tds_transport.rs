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

    /// Check whether the underlying connection is dead via a non-blocking socket poll.
    /// Returns `true` if the connection is known to be dead, `false` if alive or unknown.
    fn is_connection_dead(&self) -> bool;

    /// Expose the transport as a `TdsPacketReader` for low-level streaming operations
    /// such as incremental PLP column reads via [`PlpColumnStream`].
    fn as_packet_reader(&mut self) -> &mut (dyn TdsPacketReader + Send + Sync);
}
