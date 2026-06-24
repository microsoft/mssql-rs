// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # Always Encrypted (Column Encryption / TCE) Feature Extension
//!
//! Implements the LOGIN7 feature extension `0x04` (`FEATUREEXT_TCE`) used to negotiate
//! Always Encrypted support with the server.
//!
//! ## Negotiation
//!
//! The client sends a single version byte indicating the highest Column Encryption (TCE)
//! protocol version it supports:
//!
//! - `1` — Always Encrypted **v1** (no secure enclave): deterministic and randomized
//!   encryption using `AEAD_AES_256_CBC_HMAC_SHA256`.
//! - `2` — Always Encrypted **v2** (secure enclaves). *Not yet implemented.*
//!
//! The server replies in the `FEATUREEXTACK` token with the version it accepted, or `0` if
//! it does not support the feature. The negotiated version gates whether COLMETADATA carries
//! a CEK table and per-column cipher metadata.

use async_trait::async_trait;

use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::message::login::{Feature, FeatureExtension};

/// Feature handler for the Column Encryption (Always Encrypted / TCE) LOGIN7 extension.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AlwaysEncryptedFeature {
    acknowledged: bool,
    negotiated_version: u8,
    client_version: u8,
}

impl AlwaysEncryptedFeature {
    /// Highest Column Encryption protocol version this client can negotiate.
    ///
    /// Currently `1` (Always Encrypted v1, no secure enclave). When enclave support is
    /// added this becomes `2`.
    pub const VERSION: u8 = 1;

    /// Creates a new feature handler requesting the given client version.
    pub fn new(client_version: u8) -> Self {
        Self {
            acknowledged: false,
            negotiated_version: 0,
            client_version,
        }
    }

    /// Returns the version negotiated with the server after a successful ack.
    ///
    /// Returns `0` when the feature was not acknowledged or the server does not support it.
    #[allow(dead_code)]
    // Consumed once result-set decryption wiring reads the negotiated version.
    pub fn negotiated_version(&self) -> u8 {
        self.negotiated_version
    }
}

impl Default for AlwaysEncryptedFeature {
    fn default() -> Self {
        Self::new(Self::VERSION)
    }
}

#[async_trait]
impl Feature for AlwaysEncryptedFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::AlwaysEncrypted
    }

    fn is_requested(&self) -> bool {
        // Only requested when registered, which happens when the connection enables
        // Column Encryption. Once registered it is always sent.
        true
    }

    fn data_length(&self) -> i32 {
        // 1 byte feature identifier + 4 bytes length + 1 byte version.
        (size_of::<u8>() + size_of::<i32>() + size_of::<u8>()) as i32
    }

    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()> {
        // Feature identifier (0x04).
        packet_writer
            .write_byte_async(self.feature_identifier().as_u8())
            .await?;
        // Data length (1 byte for the version).
        packet_writer.write_i32_async(1).await?;
        // Client-supported version.
        packet_writer.write_byte_async(self.client_version).await?;
        Ok(())
    }

    fn deserialize(&mut self, data: &[u8]) -> TdsResult<()> {
        if data.len() != 1 {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid data length {} for Always Encrypted feature, expected 1 byte",
                data.len()
            )));
        }

        let server_supported_version = data[0];

        // Server returns 0 when it does not support the feature at all.
        if server_supported_version == 0 {
            self.negotiated_version = 0;
            return Ok(());
        }

        // The server must not acknowledge a version higher than the client requested.
        if server_supported_version > self.client_version {
            return Err(crate::error::Error::ProtocolError(format!(
                "Server Always Encrypted version {} exceeds client requested version {}",
                server_supported_version, self.client_version
            )));
        }

        self.negotiated_version = server_supported_version;
        Ok(())
    }

    fn is_acknowledged(&self) -> bool {
        self.acknowledged
    }

    fn set_acknowledged(&mut self, acknowledged: bool) {
        self.acknowledged = acknowledged;
    }

    fn clone_box(&self) -> Box<dyn Feature> {
        Box::new(*self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_identifier() {
        let feature = AlwaysEncryptedFeature::default();
        assert_eq!(
            feature.feature_identifier(),
            FeatureExtension::AlwaysEncrypted
        );
    }

    #[test]
    fn test_default_requests_version_1() {
        let feature = AlwaysEncryptedFeature::default();
        assert!(feature.is_requested());
        assert_eq!(feature.client_version, 1);
    }

    #[test]
    fn test_data_length() {
        let feature = AlwaysEncryptedFeature::default();
        assert_eq!(feature.data_length(), 6);
    }

    #[test]
    fn test_serialize_writes_id_length_and_version() {
        use crate::io::packet_writer::tests::MockNetworkWriter;
        use crate::message::messages::PacketType;
        use futures::executor::block_on;

        let mut mock_writer = MockNetworkWriter::new(4096);
        let mut pw = PacketWriter::new(PacketType::Login7, &mut mock_writer, None, None);

        let feature = AlwaysEncryptedFeature::default();
        block_on(feature.serialize(&mut pw)).unwrap();

        let payload = pw.get_payload();
        let bytes = payload.get_ref();
        // Skip the 8-byte packet header.
        let fb = &bytes[8..];
        assert_eq!(fb[0], 0x04); // feature identifier (TCE)
        assert_eq!(u32::from_le_bytes([fb[1], fb[2], fb[3], fb[4]]), 1); // data length
        assert_eq!(fb[5], 0x01); // client version
    }

    #[test]
    fn test_deserialize_accepts_negotiated_version() {
        let mut feature = AlwaysEncryptedFeature::default();
        feature.deserialize(&[1]).unwrap();
        assert_eq!(feature.negotiated_version(), 1);
    }

    #[test]
    fn test_deserialize_zero_means_unsupported() {
        let mut feature = AlwaysEncryptedFeature::default();
        feature.deserialize(&[0]).unwrap();
        assert_eq!(feature.negotiated_version(), 0);
    }

    #[test]
    fn test_deserialize_rejects_higher_version() {
        let mut feature = AlwaysEncryptedFeature::new(1);
        assert!(feature.deserialize(&[2]).is_err());
    }

    #[test]
    fn test_deserialize_rejects_wrong_length() {
        let mut feature = AlwaysEncryptedFeature::default();
        assert!(feature.deserialize(&[1, 2]).is_err());
    }
}
