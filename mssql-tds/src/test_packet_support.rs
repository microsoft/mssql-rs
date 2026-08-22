// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test-only plumbing for feeding hand-built TDS packets to a real
//! [`NetworkTransport`].
//!
//! This lives outside both `io` and `connection::transport` on purpose: it
//! spans the two layers (it builds `io`-shaped packets and hands them to a
//! `connection::transport` reader), so parking it in either one makes that
//! module's tests depend on the other. Reader tests, transport tests, and the
//! token parser tests all pull from here instead.

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use tokio::io::{AsyncWriteExt, DuplexStream, duplex};

use crate::connection::client_context::ClientContext;
use crate::connection::transport::network_transport::NetworkTransport;
use crate::connection::transport::ssl_handler::SslHandler;
use crate::message::messages::PacketType;

macro_rules! append_method {
    ($name:ident, $type:ty, $size:expr_2021, $write_fn:ident) => {
        pub(crate) fn $name(&mut self, number: $type) -> &mut TestPacketBuilder {
            let mut buffer = [0u8; $size];
            LittleEndian::$write_fn(&mut buffer, number);
            self.data.extend_from_slice(&buffer);
            self
        }
    };
}

/// Builds a single well-formed TDS packet: an 8-byte header (EOM status,
/// big-endian length) followed by the appended payload bytes.
pub(crate) struct TestPacketBuilder {
    data: Vec<u8>,
}

impl TestPacketBuilder {
    pub(crate) fn new(packet_type: PacketType) -> TestPacketBuilder {
        let mut data: Vec<u8> = vec![0; 8];
        // Set status to EOM by default
        data[1] = 0x1;
        data[0] = packet_type as u8;

        TestPacketBuilder { data }
    }

    pub(crate) fn append_byte(&mut self, byte: u8) -> &mut TestPacketBuilder {
        self.data.push(byte);
        self
    }

    pub(crate) fn append_bytes(&mut self, bytes: &[u8]) -> &mut TestPacketBuilder {
        self.data.extend_from_slice(bytes);
        self
    }

    append_method!(append_u16, u16, 2, write_u16);
    append_method!(append_i16, i16, 2, write_i16);
    append_method!(append_f32, f32, 4, write_f32);
    append_method!(append_f64, f64, 8, write_f64);
    append_method!(append_i64, i64, 8, write_i64);
    append_method!(append_u32, u32, 4, write_u32);
    append_method!(append_i32, i32, 4, write_i32);
    append_method!(append_u64, u64, 8, write_u64);

    /// Writes the total packet length (header + payload) into the header's
    /// big-endian length field, per TDS.
    pub(crate) fn build(&mut self) -> Vec<u8> {
        let total = u16::try_from(self.data.len()).expect("test packet exceeds u16 length");
        BigEndian::write_u16(&mut self.data[2..4], total);
        self.data.clone()
    }
}

/// Encodes `value` as little-endian UTF-16 bytes, the on-the-wire form of
/// TDS unicode strings.
pub(crate) fn encode_utf16_le(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect()
}

fn build_duplex_transport(client_side: DuplexStream) -> NetworkTransport {
    let context = ClientContext::default();
    NetworkTransport::new(
        Box::new(client_side),
        SslHandler {
            server_host_name: context.transport_context.get_server_name().clone(),
            encryption_options: context.encryption_options.clone(),
        },
        context.packet_size as u32,
        context.encryption_options.mode,
        false,
    )
}

/// Builds a `NetworkTransport` whose read side is pre-loaded with `data`.
/// The writer half is dropped once `data` is written, so reads observe EOF
/// after it is drained.
///
/// The duplex buffer holds all of `data`, so it arrives in as few reads as the
/// reader asks for — use [`create_network_transport_with_chunked_data`] to
/// force fragmented reads. Every TDS packet in `data` must be at most 8000
/// bytes in total (the default negotiated packet size), or `get_new_tds_packet`
/// rejects it.
pub(crate) fn create_network_transport_with_data(data: &[u8]) -> NetworkTransport {
    create_network_transport_with_chunked_data(data, data.len().max(1))
}

/// Builds a `NetworkTransport` fed `data` in `chunk_size` pieces, so reads
/// observe the header and payload splits a real socket can produce.
///
/// The duplex buffer is sized to one chunk, so the writer blocks until the
/// reader drains each piece. Supplying fewer bytes than a packet header
/// advertises leaves the reader at EOF mid-packet.
pub(crate) fn create_network_transport_with_chunked_data(
    data: &[u8],
    chunk_size: usize,
) -> NetworkTransport {
    let chunk_size = chunk_size.max(1);
    let (client_side, mut server_side) = duplex(chunk_size);
    let owned = data.to_vec();
    tokio::spawn(async move {
        for chunk in owned.chunks(chunk_size) {
            if server_side.write_all(chunk).await.is_err() {
                return;
            }
        }
    });

    build_duplex_transport(client_side)
}

/// Builds a `NetworkTransport` fed `data` whose peer stays **connected** after
/// the data is drained.
///
/// [`create_network_transport_with_data`] drops its writer, so a starved reader
/// observes EOF and errors out. That masks no-progress bugs: a loop that fails
/// to advance is rescued by the EOF rather than spinning. Holding the peer open
/// reproduces real server behavior — the socket simply goes quiet — so a reader
/// that cannot make progress blocks forever instead of erroring. Tests using
/// this helper must therefore bound themselves with `tokio::time::timeout`.
pub(crate) fn create_network_transport_with_live_peer(data: &[u8]) -> NetworkTransport {
    let (client_side, mut server_side) = duplex(data.len().max(1));
    let owned = data.to_vec();
    tokio::spawn(async move {
        let _ = server_side.write_all(&owned).await;
        // Parks forever, keeping `server_side` alive so the reader never sees EOF.
        std::future::pending::<()>().await;
    });

    build_duplex_transport(client_side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_builder_writes_total_length_in_header() {
        let mut builder = TestPacketBuilder::new(PacketType::PreLogin);
        builder.append_bytes(&[0u8; 12]);
        let packet = builder.build();

        assert_eq!(packet.len(), 20);
        assert_eq!(
            BigEndian::read_u16(&packet[2..4]),
            20,
            "header must carry total length, not payload length"
        );
    }

    #[test]
    fn test_packet_builder_empty_payload_length_is_header_only() {
        let packet = TestPacketBuilder::new(PacketType::TabularResult).build();

        assert_eq!(packet.len(), 8);
        assert_eq!(BigEndian::read_u16(&packet[2..4]), 8);
    }
}
