// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use tracing::event;

use super::packet_writer::PacketWriter;
use crate::core::TdsResult;
use crate::io::packet_buffer::PacketBuffer;
use crate::io::reader_writer::NetworkReaderWriter;
use crate::message::attention::AttentionRequest;
use crate::message::messages::Request;
use std::io::{Error, ErrorKind};

#[async_trait]
#[cfg(not(fuzzing))]
pub(crate) trait TdsPacketReader {
    async fn read_byte(&mut self) -> TdsResult<u8>;
    async fn read_int16_big_endian(&mut self) -> TdsResult<i16>;
    async fn read_int32_big_endian(&mut self) -> TdsResult<i32>;
    async fn read_uint40(&mut self) -> TdsResult<u64>;

    async fn read_float32(&mut self) -> TdsResult<f32>;
    async fn read_float64(&mut self) -> TdsResult<f64>;
    async fn read_int16(&mut self) -> TdsResult<i16>;
    async fn read_uint16(&mut self) -> TdsResult<u16>;
    async fn read_uint24(&mut self) -> TdsResult<u32>;
    async fn read_int32(&mut self) -> TdsResult<i32>;
    async fn read_uint32(&mut self) -> TdsResult<u32>;
    async fn read_int64(&mut self) -> TdsResult<i64>;
    async fn read_uint64(&mut self) -> TdsResult<u64>;

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize>;
    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>>;
    #[allow(dead_code)]
    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>>;
    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>>;
    async fn read_varchar_u8_length(&mut self) -> TdsResult<String>;
    #[allow(dead_code)]
    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String>;
    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String>;
    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()>;
    async fn cancel_read_stream(&mut self) -> TdsResult<()>;
    fn reset_reader(&mut self);
}

/// Low-level TDS packet reading operations (public under `fuzzing` cfg).
#[async_trait]
#[cfg(fuzzing)]
pub trait TdsPacketReader {
    async fn read_byte(&mut self) -> TdsResult<u8>;
    async fn read_int16_big_endian(&mut self) -> TdsResult<i16>;
    async fn read_int32_big_endian(&mut self) -> TdsResult<i32>;
    async fn read_uint40(&mut self) -> TdsResult<u64>;

    async fn read_float32(&mut self) -> TdsResult<f32>;
    async fn read_float64(&mut self) -> TdsResult<f64>;
    async fn read_int16(&mut self) -> TdsResult<i16>;
    async fn read_uint16(&mut self) -> TdsResult<u16>;
    async fn read_uint24(&mut self) -> TdsResult<u32>;
    async fn read_int32(&mut self) -> TdsResult<i32>;
    async fn read_uint32(&mut self) -> TdsResult<u32>;
    async fn read_int64(&mut self) -> TdsResult<i64>;
    async fn read_uint64(&mut self) -> TdsResult<u64>;

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize>;
    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>>;
    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>>;
    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>>;
    async fn read_varchar_u8_length(&mut self) -> TdsResult<String>;
    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String>;
    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String>;
    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()>;
    async fn cancel_read_stream(&mut self) -> TdsResult<()>;
    fn reset_reader(&mut self);
}

/// Buffered reader that reassembles TDS packets from the network stream.
///
/// The buffer math and every scalar/byte read live in the I/O-free
/// [`PacketBuffer`]; this type only adds the one thing the buffer cannot do
/// itself — pull more bytes off the socket when a read needs them.
pub struct PacketReader<'a> {
    network_reader_writer: &'a mut dyn NetworkReaderWriter,
    buffer: PacketBuffer,
}

impl<'a> PacketReader<'a> {
    pub const LENGTHNULL: u16 = 0xffff;

    #[cfg(test)]
    pub(crate) fn new(network_reader_writer: &'a mut dyn NetworkReaderWriter) -> PacketReader<'a> {
        let packet_size: usize = network_reader_writer.as_writer().packet_size() as usize;
        PacketReader {
            network_reader_writer,
            buffer: PacketBuffer::with_packet_size(packet_size),
        }
    }

    /// Ensures at least `byte_count` bytes are readable, refilling once if not.
    /// A scalar never exceeds one packet, so a single refill always suffices.
    async fn ensure(&mut self, byte_count: usize) -> TdsResult<()> {
        if !self.buffer.has(byte_count) {
            self.read_tds_packet().await?;
        }
        Ok(())
    }

    async fn read_tds_packet(&mut self) -> TdsResult<()> {
        let (base, already) = self.buffer.begin_refill()?;
        let received = self.receive_packet(base, already).await?;
        self.buffer.strip_header(received);
        Ok(())
    }

    /// Public test helper to read TDS packets in unit tests
    #[cfg(test)]
    pub(crate) async fn read_tds_packet_for_test(&mut self) -> TdsResult<()> {
        self.read_tds_packet().await
    }

    /// Reads one TDS packet's raw bytes into the buffer and returns the total
    /// number of bytes read (header + payload). The header's declared length is
    /// used only as a lower bound on how much to read; the caller strips the
    /// header from whatever was actually received.
    async fn receive_packet(&mut self, base: usize, already: usize) -> TdsResult<usize> {
        let mut received = already;

        // The 8-byte header may arrive split across reads; keep reading until it
        // is complete before trusting its declared length.
        while received < PacketWriter::PACKET_HEADER_SIZE {
            let bytes_read = self
                .network_reader_writer
                .receive(self.buffer.refill_window(base, received))
                .await?;
            if bytes_read == 0 {
                return Err(crate::error::Error::ConnectionClosed(
                    "Connection closed by server while reading TDS packet header".to_string(),
                ));
            }
            received += bytes_read;
        }

        let packet_size_from_header = self.buffer.packet_header_length(base);
        while received < packet_size_from_header {
            let bytes_read = self
                .network_reader_writer
                .receive(self.buffer.refill_window(base, received))
                .await?;
            if bytes_read == 0 {
                return Err(crate::error::Error::ConnectionClosed(
                    "Connection closed by server while reading TDS packet payload".to_string(),
                ));
            }
            received += bytes_read;
        }

        event!(
            tracing::Level::DEBUG,
            "Received packet of size: {:?}",
            received
        );

        use pretty_hex::PrettyHex;
        event!(
            tracing::Level::DEBUG,
            "Packet content: {:?}",
            self.buffer.raw_packet(base, received).hex_dump()
        );

        Ok(received)
    }
}

#[async_trait]
impl TdsPacketReader for PacketReader<'_> {
    fn reset_reader(&mut self) {
        // Make sure that we have read all the data from the buffer.
        assert!(self.buffer.is_drained());
        // No Op after this.
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        let attention = AttentionRequest::new();
        let mut packet_writer =
            attention.create_packet_writer(self.network_reader_writer.as_writer(), None, None);
        attention.serialize(&mut packet_writer).await?;
        Ok(())
    }

    async fn read_byte(&mut self) -> TdsResult<u8> {
        self.ensure(1).await?;
        self.buffer.take_u8()
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        self.ensure(2).await?;
        self.buffer.take_i16_be()
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        self.ensure(4).await?;
        self.buffer.take_i32_be()
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        self.ensure(5).await?;
        self.buffer.take_uint40_le()
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        self.ensure(4).await?;
        self.buffer.take_f32_le()
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        self.ensure(8).await?;
        self.buffer.take_f64_le()
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        self.ensure(2).await?;
        self.buffer.take_i16_le()
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        self.ensure(2).await?;
        self.buffer.take_u16_le()
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        self.ensure(3).await?;
        self.buffer.take_u24_le()
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        self.ensure(4).await?;
        self.buffer.take_i32_le()
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        self.ensure(4).await?;
        self.buffer.take_u32_le()
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        self.ensure(8).await?;
        self.buffer.take_i64_le()
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        self.ensure(8).await?;
        self.buffer.take_u64_le()
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        let mut total_read = 0;
        while total_read < buffer.len() {
            if self.buffer.available() == 0 {
                self.read_tds_packet().await?;
            }
            total_read += self.buffer.copy_out(&mut buffer[total_read..]);
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
        if length as usize > u16::MAX as usize {
            return Err(crate::error::Error::UsageError(format!(
                "Varbyte length {} exceeds maximum allowed size of {} bytes",
                length,
                u16::MAX
            )));
        }

        let mut result: Vec<u8> = vec![0; length as usize];
        self.read_bytes(&mut result[0..]).await?;
        Ok(result)
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        let length: u16 = self.read_uint16().await?;
        if length == Self::LENGTHNULL {
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
        // Prevent OOM by limiting maximum string allocation to twice the u8 length.
        const MAX_STRING_BYTE_LENGTH: usize = u8::MAX as usize * 2;
        if byte_length > MAX_STRING_BYTE_LENGTH {
            return Err(crate::error::Error::UsageError(format!(
                "Unicode string byte length {byte_length} exceeds maximum allowed size of {MAX_STRING_BYTE_LENGTH} bytes"
            )));
        }

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

    /// Skips a specified number of bytes in the packet stream.
    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()> {
        let mut remaining = skip_count;
        while remaining > 0 {
            if self.buffer.available() == 0 {
                self.read_tds_packet().await?;
            }
            remaining -= self.buffer.skip_available(remaining);
        }
        Ok(())
    }
}

// Blanket implementation for Box<dyn TdsPacketReader> to enable dynamic dispatch
#[async_trait]
impl TdsPacketReader for Box<dyn TdsPacketReader + Send + Sync> {
    async fn read_byte(&mut self) -> TdsResult<u8> {
        (**self).read_byte().await
    }

    async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
        (**self).read_int16_big_endian().await
    }

    async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
        (**self).read_int32_big_endian().await
    }

    async fn read_uint40(&mut self) -> TdsResult<u64> {
        (**self).read_uint40().await
    }

    async fn read_float32(&mut self) -> TdsResult<f32> {
        (**self).read_float32().await
    }

    async fn read_float64(&mut self) -> TdsResult<f64> {
        (**self).read_float64().await
    }

    async fn read_int16(&mut self) -> TdsResult<i16> {
        (**self).read_int16().await
    }

    async fn read_uint16(&mut self) -> TdsResult<u16> {
        (**self).read_uint16().await
    }

    async fn read_uint24(&mut self) -> TdsResult<u32> {
        (**self).read_uint24().await
    }

    async fn read_int32(&mut self) -> TdsResult<i32> {
        (**self).read_int32().await
    }

    async fn read_uint32(&mut self) -> TdsResult<u32> {
        (**self).read_uint32().await
    }

    async fn read_int64(&mut self) -> TdsResult<i64> {
        (**self).read_int64().await
    }

    async fn read_uint64(&mut self) -> TdsResult<u64> {
        (**self).read_uint64().await
    }

    async fn read_bytes(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        (**self).read_bytes(buffer).await
    }

    async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        (**self).read_u8_varbyte().await
    }

    async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
        (**self).read_u16_varbyte().await
    }

    async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
        (**self).read_varchar_u16_length().await
    }

    async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
        (**self).read_varchar_u8_length().await
    }

    async fn read_unicode(&mut self, string_length: usize) -> TdsResult<String> {
        (**self).read_unicode(string_length).await
    }

    async fn read_unicode_with_byte_length(&mut self, byte_length: usize) -> TdsResult<String> {
        (**self).read_unicode_with_byte_length(byte_length).await
    }

    async fn skip_bytes(&mut self, skip_count: usize) -> TdsResult<()> {
        (**self).skip_bytes(skip_count).await
    }

    async fn cancel_read_stream(&mut self) -> TdsResult<()> {
        (**self).cancel_read_stream().await
    }

    fn reset_reader(&mut self) {
        (**self).reset_reader()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::message::messages::PacketType;

    use super::*;
    use crate::connection::transport::network_transport::TransportSslHandler;
    use crate::core::NegotiatedEncryptionSetting;
    use crate::handler::handler_factory::SessionSettings;
    use crate::io::reader_writer::{NetworkReader, NetworkWriter};
    use async_trait::async_trait;
    use byteorder::{BigEndian, ByteOrder, LittleEndian};
    use rand::Rng;
    use std::cmp::min;

    //append_method!(append_i64, i64, 8, write_i64);
    macro_rules! append_method {
        ($name:ident, $type:ty, $size:expr_2021, $write_fn:ident) => {
            pub(crate) fn $name(&mut self, number: $type) -> &mut TestPacketBuilder {
                let mut buffer = [0u8; $size];
                LittleEndian::$write_fn(&mut buffer, number);
                self.data.extend_from_slice(&buffer);
                self.payload_length += $size as u16;
                self
            }
        };
    }

    pub(crate) struct TestPacketBuilder {
        data: Vec<u8>,
        payload_length: u16,
    }

    /// A builder for creating test packets with specified data and packet type.
    ///
    /// # Fields
    /// - `data`: A vector of bytes representing the packet data.
    /// - `length`: The length of the packet data.
    ///
    /// # Methods
    /// - `new(packet_type: PacketType) -> TestPacketBuilder`:
    ///   Creates a new `TestPacketBuilder` with the specified packet type.
    ///   The packet data is initialized with a default size of 8 bytes, and the status is set to EOM by default.
    /// - `append_byte(&mut self, byte: u8)`:
    ///   Appends a single byte to the packet data and increments the length by 1.
    /// - `append_u16(&mut self, number: u16)`:
    ///   Appends a 16-bit unsigned integer to the packet data in little-endian format and increments the length by 2.
    /// - `build(&mut self) -> Vec<u8>`:
    ///   Finalizes the packet by writing the length in big-endian format to the appropriate position in the data,
    ///   and returns a clone of the packet data.
    impl TestPacketBuilder {
        pub(crate) fn new(packet_type: PacketType) -> TestPacketBuilder {
            let mut data: Vec<u8> = vec![0; 8];
            // Set status to EOM by default
            data[1] = 0x1;
            data[0] = packet_type as u8;

            TestPacketBuilder {
                data,
                payload_length: 0,
            }
        }

        pub(crate) fn append_byte(&mut self, byte: u8) -> &mut TestPacketBuilder {
            self.data.push(byte);
            self.payload_length += 1;
            self
        }

        pub(crate) fn append_bytes(&mut self, bytes: &[u8]) -> &mut TestPacketBuilder {
            self.data.extend_from_slice(bytes);
            self.payload_length += bytes.len() as u16;
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

        pub(crate) fn build(&mut self) -> Vec<u8> {
            BigEndian::write_u16(&mut self.data[2..4], self.payload_length);
            self.data.clone()
        }
    }

    pub(crate) struct MockNetworkReaderWriter {
        pub(crate) read_data: Vec<u8>,
        pub(crate) position: usize,
        pub(crate) write_data: Vec<u8>,
    }

    impl MockNetworkReaderWriter {
        pub(crate) fn new(read_data: Vec<u8>, position: usize) -> MockNetworkReaderWriter {
            MockNetworkReaderWriter {
                read_data,
                position,
                write_data: Vec::new(),
            }
        }

        pub(crate) fn get_written_data(&self) -> &[u8] {
            self.write_data.as_slice()
        }
    }

    #[async_trait]
    impl TdsPacketReader for MockNetworkReaderWriter {
        fn reset_reader(&mut self) {}

        async fn read_byte(&mut self) -> TdsResult<u8> {
            todo!()
        }

        async fn read_int16_big_endian(&mut self) -> TdsResult<i16> {
            todo!()
        }

        async fn read_int32_big_endian(&mut self) -> TdsResult<i32> {
            todo!()
        }

        async fn read_uint40(&mut self) -> TdsResult<u64> {
            todo!()
        }

        async fn read_float32(&mut self) -> TdsResult<f32> {
            todo!()
        }

        async fn read_float64(&mut self) -> TdsResult<f64> {
            todo!()
        }

        async fn read_int16(&mut self) -> TdsResult<i16> {
            todo!()
        }

        async fn read_uint16(&mut self) -> TdsResult<u16> {
            todo!()
        }

        async fn read_uint24(&mut self) -> TdsResult<u32> {
            todo!()
        }

        async fn read_int32(&mut self) -> TdsResult<i32> {
            todo!()
        }

        async fn read_uint32(&mut self) -> TdsResult<u32> {
            todo!()
        }

        async fn read_int64(&mut self) -> TdsResult<i64> {
            todo!()
        }

        async fn read_uint64(&mut self) -> TdsResult<u64> {
            todo!()
        }

        async fn read_bytes(&mut self, _buffer: &mut [u8]) -> TdsResult<usize> {
            todo!()
        }

        async fn read_u8_varbyte(&mut self) -> TdsResult<Vec<u8>> {
            todo!()
        }

        async fn read_u16_varbyte(&mut self) -> TdsResult<Vec<u8>> {
            todo!()
        }

        async fn read_varchar_u16_length(&mut self) -> TdsResult<Option<String>> {
            todo!()
        }

        async fn read_varchar_u8_length(&mut self) -> TdsResult<String> {
            todo!()
        }

        async fn read_unicode(&mut self, _string_length: usize) -> TdsResult<String> {
            todo!()
        }

        async fn read_unicode_with_byte_length(
            &mut self,
            _byte_length: usize,
        ) -> TdsResult<String> {
            todo!()
        }

        async fn skip_bytes(&mut self, _skip_count: usize) -> TdsResult<()> {
            todo!()
        }

        async fn cancel_read_stream(&mut self) -> TdsResult<()> {
            todo!()
        }
    }
    impl Default for MockNetworkReaderWriter {
        fn default() -> Self {
            MockNetworkReaderWriter::new(Vec::new(), 0)
        }
    }

    #[async_trait]
    impl NetworkWriter for MockNetworkReaderWriter {
        async fn send(&mut self, _data: &[u8]) -> TdsResult<()> {
            self.write_data.extend_from_slice(_data);
            Ok(())
        }

        fn packet_size(&self) -> u32 {
            4096 // Dummy value
        }

        fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
            todo!()
        }
    }

    #[async_trait]
    impl TransportSslHandler for MockNetworkReaderWriter {
        async fn enable_ssl(&mut self) -> TdsResult<()> {
            todo!()
        }

        async fn disable_ssl(&mut self) -> TdsResult<()> {
            todo!()
        }
    }

    #[async_trait]
    impl NetworkReaderWriter for MockNetworkReaderWriter {
        fn notify_encryption_setting_change(&mut self, _setting: NegotiatedEncryptionSetting) {
            todo!()
        }

        fn notify_session_setting_change(&mut self, _settings: &SessionSettings) {
            todo!()
        }

        fn as_writer(&mut self) -> &mut dyn NetworkWriter {
            self
        }
    }

    #[async_trait]
    impl NetworkReader for MockNetworkReaderWriter {
        async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
            let remaining = self.read_data.len() - self.position;
            let to_read = min(buffer.len(), remaining);
            buffer[..to_read]
                .copy_from_slice(&self.read_data[self.position..self.position + to_read]);
            self.position += to_read;
            Ok(to_read)
        }

        fn packet_size(&self) -> u32 {
            4096
        }
    }

    fn generate_random_bytes(length: usize) -> Vec<u8> {
        let mut rng = rand::rng();
        let mut bytes = vec![0u8; length];
        rng.fill(&mut bytes[..]);
        bytes
    }

    // Regression guard for the refill spin loop: when the peer has no more bytes,
    // the mock's `receive` returns `Ok(0)`. Without the zero-byte EOF guard the
    // header/payload refill loops would re-poll a ready future forever and never
    // make progress. Each of these must return `ConnectionClosed` deterministically
    // (the test completing at all proves the loop terminates).
    #[tokio::test]
    async fn test_read_tds_packet_empty_input_returns_connection_closed() {
        let mut mock_reader = MockNetworkReaderWriter::new(Vec::new(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader);

        let result = packet_reader.read_tds_packet().await;

        assert!(
            matches!(result, Err(crate::error::Error::ConnectionClosed(_))),
            "empty input must surface ConnectionClosed, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_read_tds_packet_truncated_header_returns_connection_closed() {
        // Fewer bytes than the 8-byte header forces a second `receive` that hits
        // exhaustion (`Ok(0)`) mid-header — the exact packet-boundary edge case.
        let partial_header = vec![0u8; PacketWriter::PACKET_HEADER_SIZE - 4];
        let mut mock_reader = MockNetworkReaderWriter::new(partial_header, 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader);

        let result = packet_reader.read_tds_packet().await;

        assert!(
            matches!(result, Err(crate::error::Error::ConnectionClosed(_))),
            "truncated header must surface ConnectionClosed, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_read_byte() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let byte_value = rng.random::<u8>();
        let builder = binding.append_byte(byte_value);

        let mut mock_reader = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader);
        packet_reader.read_tds_packet().await.unwrap();

        let byte = packet_reader.read_byte().await.unwrap();

        assert_eq!(byte, byte_value);
    }

    #[tokio::test]
    async fn test_read_int16() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let int16_value = rng.random::<i16>();
        let builder = binding.append_i16(int16_value);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        packet_reader.read_tds_packet().await.unwrap();

        let int16 = packet_reader.read_int16().await.unwrap();
        assert_eq!(int16, int16_value);
    }

    #[tokio::test]
    async fn test_read_uint16() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let uint16_value = rng.random::<u16>();
        let builder = binding.append_u16(uint16_value);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        packet_reader.read_tds_packet().await.unwrap();

        let uint16 = packet_reader.read_uint16().await.unwrap();
        assert_eq!(uint16, uint16_value);
    }

    #[tokio::test]
    async fn test_read_int32() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let int32_value = rng.random::<i32>();
        let builder = binding.append_i32(int32_value);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        packet_reader.read_tds_packet().await.unwrap();

        let int32 = packet_reader.read_int32().await.unwrap();
        assert_eq!(int32, int32_value);
    }

    #[tokio::test]
    async fn test_read_uint32() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let uint32_value = rng.random::<u32>();
        let builder = binding.append_u32(uint32_value);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        packet_reader.read_tds_packet().await.unwrap();

        let uint32 = packet_reader.read_uint32().await.unwrap();
        assert_eq!(uint32, uint32_value);
    }

    #[tokio::test]
    async fn test_read_int64() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let int64_value = rng.random::<i64>();
        let builder = binding.append_i64(int64_value);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        packet_reader.read_tds_packet().await.unwrap();

        let int64 = packet_reader.read_int64().await.unwrap();
        assert_eq!(int64, int64_value);
    }

    #[tokio::test]
    async fn test_read_uint64() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let uint64_value = rng.random::<u64>();
        let builder = binding.append_u64(uint64_value);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        packet_reader.read_tds_packet().await.unwrap();

        let uint64 = packet_reader.read_uint64().await.unwrap();
        assert_eq!(uint64, uint64_value);
    }

    #[tokio::test]
    async fn test_read_float32() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let float32_value = rng.random::<f32>();
        let builder = binding.append_f32(float32_value);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        packet_reader.read_tds_packet().await.unwrap();

        let float32 = packet_reader.read_float32().await.unwrap();
        assert_eq!(float32, float32_value);
    }

    #[tokio::test]
    async fn test_read_float64() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut rng = rand::rng();
        let float64_value = rng.random::<f64>();
        let builder = binding.append_f64(float64_value);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        packet_reader.read_tds_packet().await.unwrap();

        let float64 = packet_reader.read_float64().await.unwrap();
        assert_eq!(float64, float64_value);
    }

    #[tokio::test]
    async fn test_read_unicode() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let unicode_string = "Hello, world";
        let utf16_units: Vec<u16> = unicode_string.encode_utf16().collect();

        let utf16_byte_len = utf16_units.len();
        let mut byte_array: Vec<u8> = Vec::with_capacity(utf16_byte_len * 2);

        for unit in utf16_units {
            byte_array.push((unit & 0xFF) as u8); // Low byte
            byte_array.push((unit >> 8) as u8); // High byte
        }

        let builder = binding.append_bytes(&byte_array[0..]);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);
        let unicode = packet_reader.read_unicode(utf16_byte_len).await.unwrap();
        assert_eq!(unicode, unicode_string);
    }

    #[tokio::test]
    async fn test_read_bytes() {
        let bytes_len = 2000;
        let bytes = generate_random_bytes(bytes_len);
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let builder = binding.append_bytes(&bytes[0..]);
        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);

        let mut buffer = vec![0; bytes_len];
        let bytes_read = packet_reader.read_bytes(&mut buffer).await.unwrap();
        assert_eq!(bytes_read, bytes_len);
        assert_eq!(buffer, bytes);
    }

    #[tokio::test]
    async fn test_read_u8_varbyte() {
        let bytes_len: u8 = 200;
        let data_bytes: Vec<u8> = generate_random_bytes(bytes_len as usize);
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut payload_bytes: Vec<u8> = Vec::new();
        payload_bytes.push(bytes_len);
        payload_bytes.extend_from_slice(&data_bytes[0..]);

        let builder = binding.append_bytes(&payload_bytes[0..]);
        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);

        packet_reader.read_tds_packet().await.unwrap();

        let varbyte = packet_reader.read_u8_varbyte().await.unwrap();
        assert_eq!(varbyte, Vec::from(&data_bytes[0..]));
    }

    #[tokio::test]
    async fn test_read_u16_varbyte() {
        let bytes_len: u16 = 1000;
        let data_bytes: Vec<u8> = generate_random_bytes(bytes_len as usize);
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let mut payload_bytes: Vec<u8> = vec![0; 2];
        LittleEndian::write_u16(&mut payload_bytes, bytes_len);
        payload_bytes.extend_from_slice(&data_bytes[0..]);

        let builder = binding.append_bytes(&payload_bytes[0..]);
        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);

        packet_reader.read_tds_packet().await.unwrap();

        let varbyte = packet_reader.read_u16_varbyte().await.unwrap();
        assert_eq!(varbyte, Vec::from(&data_bytes[0..]));
    }

    #[tokio::test]
    async fn test_read_varchar_with_byte_length() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let unicode_string = "Hello, world";
        let utf16_units: Vec<u16> = unicode_string.encode_utf16().collect();

        let utf16_byte_len: u16 = utf16_units.len() as u16;
        let mut byte_array: Vec<u8> = vec![0; 2];
        LittleEndian::write_u16(&mut byte_array[0..], utf16_byte_len);
        for unit in utf16_units {
            byte_array.push((unit & 0xFF) as u8); // Low byte
            byte_array.push((unit >> 8) as u8); // High byte
        }

        let builder = binding.append_bytes(&byte_array[0..]);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);
        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);

        let varchar = packet_reader.read_varchar_u16_length().await.unwrap();
        // assert_eq!(varchar, Some("ab".to_string()));
        assert_eq!(varchar, Some(unicode_string.to_string()));
    }

    #[tokio::test]
    async fn test_read_u8_varchar() {
        let mut binding = TestPacketBuilder::new(PacketType::PreLogin);
        let unicode_string = "Hello, world";
        let utf16_units: Vec<u16> = unicode_string.encode_utf16().collect();

        let utf16_byte_len: u8 = utf16_units.len() as u8;
        let mut byte_array: Vec<u8> = Vec::new();
        byte_array.push(utf16_byte_len);

        for unit in utf16_units {
            byte_array.push((unit & 0xFF) as u8); // Low byte
            byte_array.push((unit >> 8) as u8); // High byte
        }

        let builder = binding.append_bytes(&byte_array[0..]);

        let mut mock_reader_writer = MockNetworkReaderWriter::new(builder.build(), 0);

        let mut packet_reader = PacketReader::new(&mut mock_reader_writer);

        let varchar = packet_reader.read_varchar_u8_length().await.unwrap();
        // assert_eq!(varchar, Some("ab".to_string()));
        assert_eq!(varchar, unicode_string.to_string());
    }
}
