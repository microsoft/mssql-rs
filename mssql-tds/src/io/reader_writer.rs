// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::transport::network_transport::TransportSslHandler;
use crate::core::{NegotiatedEncryptionSetting, TdsResult};
use crate::handler::handler_factory::SessionSettings;
use crate::message::messages::ResetConnectionMode;
use async_trait::async_trait;

#[async_trait]
pub(crate) trait NetworkWriter: Send + Sync + TransportSslHandler {
    async fn send(&mut self, data: &[u8]) -> TdsResult<()>;
    fn packet_size(&self) -> u32;
    fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting;

    /// Records that the next SQL Batch, RPC, or Transaction Manager request
    /// sent on this connection should carry a connection-reset request in its
    /// packet header. Connection-level state, consumed by the packet writer.
    ///
    /// The default implementation is a no-op so that transports which do not
    /// support connection pooling (e.g. test mocks) need not implement it.
    fn set_reset_mode(&mut self, _mode: ResetConnectionMode) {}

    /// Atomically reads and clears any pending connection-reset request set via
    /// [`set_reset_mode`](Self::set_reset_mode). Returns
    /// [`ResetConnectionMode::None`] when no reset is pending.
    fn take_reset_mode(&mut self) -> ResetConnectionMode {
        ResetConnectionMode::None
    }

    /// Records that a packet header carrying the reset request taken by
    /// [`take_reset_mode`](Self::take_reset_mode) reached the wire, so the
    /// server now owes a `ResetConnection` ENVCHANGE for it.
    ///
    /// The packet writer, not the client, consumes the armed mode, so this is
    /// how `TdsClient` learns that the bit was actually sent and that the
    /// acknowledgement is now verifiable.
    fn note_reset_dispatched(&mut self) {}

    /// Atomically reads and clears the record left by
    /// [`note_reset_dispatched`](Self::note_reset_dispatched).
    fn take_reset_dispatched(&mut self) -> bool {
        false
    }

    /// Returns the TLS channel binding token (`tls-unique`, RFC 5929 §3) for
    /// the active connection, if one is available.
    ///
    /// Used to populate channel bindings for integrated-auth Extended
    /// Protection. The default implementation returns `None` so transports
    /// that do not support TLS (e.g. test mocks) need not implement it.
    fn channel_binding_token(&self) -> Option<Vec<u8>> {
        None
    }
}

#[async_trait]
pub(crate) trait NetworkReader: Send {
    fn packet_size(&self) -> u32;
}

#[async_trait]
pub(crate) trait NetworkReaderWriter: NetworkReader + NetworkWriter {
    fn notify_encryption_setting_change(&mut self, setting: NegotiatedEncryptionSetting);
    fn notify_session_setting_change(&mut self, settings: &SessionSettings);
    fn as_writer(&mut self) -> &mut dyn NetworkWriter;
}

#[cfg(test)]
mod tests {
    use crate::connection::client_context::ClientContext;
    use crate::connection::transport::network_transport::tests::MAX_BUFFER_SIZE;
    use crate::connection::transport::network_transport::tests::create_readable_network_transport;
    use crate::io::reader_writer::NetworkWriter;
    use futures::StreamExt;
    use rand::Rng;
    use tokio_util::codec::{BytesCodec, FramedRead};

    #[tokio::test]
    async fn test_send_data() {
        let context = ClientContext::default();
        let (transport, server_side) = create_readable_network_transport(&context);

        let mut network_writer = transport;

        // Fill data_to_send with random values
        let mut rng = rand::rng();
        let data_vector: Vec<u8> = (0..MAX_BUFFER_SIZE).map(|_| rng.random()).collect();

        // Setup the reader to read the data.
        let mut framed_reader = FramedRead::new(server_side, BytesCodec::new());

        // Send the data and read it from the other end of the pipe.
        let result = network_writer.send(&data_vector[..]).await;
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
}
