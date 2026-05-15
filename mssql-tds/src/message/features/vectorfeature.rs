// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;

use crate::connection::client_context::VectorVersion;
use crate::core::TdsResult;
use crate::io::packet_writer::{PacketWriter, TdsPacketWriter};
use crate::message::login::{Feature, FeatureExtension};

#[derive(Debug, Clone, Copy)]
pub(crate) struct VectorFeature {
    acknowledged: bool,
    negotiated_version: u8,
    client_version: u8,
}

impl VectorFeature {
    /// The maximum Vector feature version supported by this library.
    /// This represents the highest version this TDS client can negotiate with the server.
    /// Version 2 supports float16 and float32 dimension types.
    pub const VERSION: u8 = 2;

    /// Creates a new VectorFeature instance with the specified client version.
    pub fn new(client_version: u8) -> Self {
        Self {
            acknowledged: false,
            negotiated_version: 0,
            client_version,
        }
    }

    /// Returns the negotiated version after successful feature exchange.
    /// The negotiated version is min(client_version, server_version).
    /// Returns 0 if feature has not been acknowledged.
    #[allow(dead_code)]
    // This method is not used currently, and exists for completeness.
    pub fn negotiated_version(&self) -> u8 {
        self.negotiated_version
    }
}

impl Default for VectorFeature {
    fn default() -> Self {
        Self::new(Self::VERSION)
    }
}

impl From<VectorVersion> for Option<VectorFeature> {
    fn from(version: VectorVersion) -> Self {
        match version {
            VectorVersion::Off => None,
            VectorVersion::V1 => Some(VectorFeature::new(1)),
            VectorVersion::V2 => Some(VectorFeature::new(2)),
        }
    }
}

#[async_trait]
impl Feature for VectorFeature {
    fn feature_identifier(&self) -> FeatureExtension {
        FeatureExtension::Vector
    }

    fn is_requested(&self) -> bool {
        // Vector support is always requested
        true
    }

    fn data_length(&self) -> i32 {
        // 1 byte for feature identifier, 4 bytes for length, 1 byte for version
        (size_of::<u8>() + size_of::<i32>() + size_of::<u8>()) as i32
    }

    async fn serialize(&self, packet_writer: &mut PacketWriter) -> TdsResult<()> {
        // Write feature identifier (0x0E)
        packet_writer
            .write_byte_async(self.feature_identifier().as_u8())
            .await?;

        // Write data length (1 byte for version)
        packet_writer.write_i32_async(1).await?;

        // Write client-supported version
        packet_writer.write_byte_async(self.client_version).await?;

        Ok(())
    }

    fn deserialize(&mut self, data: &[u8]) -> TdsResult<()> {
        if data.len() != 1 {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid data length {} for VectorSupport feature, expected 1 byte",
                data.len()
            )));
        }

        let server_supported_version = data[0];

        // Server returns 0 if it doesn't support the feature at all
        if server_supported_version == 0 {
            self.negotiated_version = 0;
            return Ok(());
        }

        // Validate that the server version doesn't exceed what the client requested
        if server_supported_version > self.client_version {
            return Err(crate::error::Error::ProtocolError(format!(
                "Server VectorSupport version {} exceeds client requested version {}",
                server_supported_version, self.client_version
            )));
        }

        // Set the negotiated version to what the server returned
        self.negotiated_version = server_supported_version;
        Ok(())
    }

    fn is_acknowledged(&self) -> bool {
        self.acknowledged
    }

    fn set_acknowledged(&mut self, acknowledged: bool) {
        self.acknowledged = acknowledged;
        if !acknowledged {
            self.negotiated_version = 0;
        }
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
        let feature = VectorFeature::default();
        assert_eq!(
            feature.feature_identifier().as_u8(),
            0x0E,
            "VectorSupport feature ID should be 0x0E"
        );
    }

    #[test]
    fn test_deserialize_invalid_version() {
        let mut feature = VectorFeature::default();
        let data = vec![3u8]; // Server supports version 3
        let result = feature.deserialize(&data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exceeds client requested version")
        );
    }

    #[test]
    fn test_deserialize_invalid_length() {
        let mut feature = VectorFeature::default();
        let data = vec![1u8, 0u8]; // Wrong length (2 bytes instead of 1)
        let result = feature.deserialize(&data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid data length")
        );
    }

    #[test]
    fn test_negotiate_v1_client_v1_server() {
        let mut feature = VectorFeature::new(1);
        assert!(!feature.is_acknowledged());
        assert_eq!(feature.negotiated_version(), 0);

        // Simulate server acknowledgment with version 1
        feature.set_acknowledged(true);
        feature.deserialize(&[1u8]).unwrap();

        assert!(feature.is_acknowledged());
        assert_eq!(feature.negotiated_version(), 1);
    }

    #[test]
    fn test_negotiate_v2_client_v1_server() {
        let mut feature = VectorFeature::default(); // Client supports up to version 2;
        assert!(!feature.is_acknowledged());
        assert_eq!(feature.negotiated_version(), 0);

        // Simulate server acknowledgment with version 1 (downgrade fallback)
        feature.set_acknowledged(true);
        feature.deserialize(&[1u8]).unwrap();

        assert!(feature.is_acknowledged());
        assert_eq!(feature.negotiated_version(), 1);
    }

    #[test]
    fn test_negotiate_v2_client_v2_server() {
        let mut feature = VectorFeature::new(2);
        assert!(!feature.is_acknowledged());
        assert_eq!(feature.negotiated_version(), 0);

        // Simulate server acknowledgment with version 2
        feature.set_acknowledged(true);
        feature.deserialize(&[2u8]).unwrap();

        assert!(feature.is_acknowledged());
        assert_eq!(feature.negotiated_version(), 2);
    }
}
